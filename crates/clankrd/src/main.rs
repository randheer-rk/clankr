use std::net::SocketAddr;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;

use anyhow::Context;
use clankr_store::Store;
use clankr_store::embedded::DevPostgres;
use clankr_store::secrets::MasterKey;
use clap::Parser;
use rand::distr::{Alphanumeric, SampleString};

/// Clankr control-plane daemon.
#[derive(Parser)]
struct Args {
    #[arg(long, env = "CLANKR_LISTEN", default_value = "127.0.0.1:7600")]
    listen: SocketAddr,
    /// Postgres URL. Required unless --dev.
    #[arg(long, env = "DATABASE_URL", required_unless_present = "dev")]
    database_url: Option<String>,
    /// Run a private embedded Postgres under --data-dir instead of DATABASE_URL.
    #[arg(long)]
    dev: bool,
    #[arg(long, env = "CLANKR_DATA_DIR")]
    data_dir: Option<PathBuf>,
    /// Serve plain HTTP (no TLS). Nodes can't join; for debugging only.
    #[arg(long, env = "CLANKR_INSECURE_HTTP", value_parser = clap::builder::FalseyValueParser::new())]
    insecure_http: bool,
    /// Extra names (DNS or IP) for the API server certificate.
    #[arg(long = "tls-san")]
    tls_san: Vec<String>,
    /// OIDC issuer URL for user login (ID tokens as bearer tokens). Off when unset.
    #[arg(long, env = "CLANKR_OIDC_ISSUER", requires = "oidc_audience")]
    oidc_issuer: Option<String>,
    /// Expected `aud` (your client id at the IdP).
    #[arg(long, env = "CLANKR_OIDC_AUDIENCE")]
    oidc_audience: Option<String>,
    /// Claim naming the user in RoleBinding subjects (`user:<value>`).
    #[arg(long, env = "CLANKR_OIDC_USER_CLAIM", default_value = "email")]
    oidc_user_claim: String,
    #[arg(long, env = "CLANKR_OIDC_GROUPS_CLAIM", default_value = "groups")]
    oidc_groups_claim: String,
    /// Public OIDC client the web UI uses (authorization code + PKCE); defaults to the audience.
    #[arg(long, env = "CLANKR_OIDC_CLIENT_ID")]
    oidc_client_id: Option<String>,
    /// kennel binary for sandboxed tools (`sandbox` and `mcpStdio` executors); off when unset.
    #[arg(long, env = "CLANKR_TOOL_KENNEL")]
    tool_kennel: Option<PathBuf>,
    /// kennel image store for sandboxed tools (default: kennel's own).
    #[arg(long, env = "CLANKR_TOOL_KENNEL_ROOT")]
    tool_kennel_root: Option<PathBuf>,
    /// Wrap secrets' data keys with a KMS instead of the local master key:
    /// vault+https://vault:8200/<transit mount>/<key> (token from $VAULT_TOKEN).
    #[arg(long, env = "CLANKR_KMS", global = true)]
    kms: Option<String>,
    /// Ship the audit chain off-site as it's written: file:///path.jsonl or https://receiver.
    #[arg(long, env = "CLANKR_AUDIT_EXPORT")]
    audit_export: Option<String>,
    /// Directory with pg_dump/pg_restore (default: PATH, or the embedded server's with --dev).
    #[arg(long, env = "CLANKR_PG_BIN", global = true)]
    pg_bin: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

/// Without a subcommand clankrd serves the API.
#[derive(clap::Subcommand)]
enum Cmd {
    /// Write a consistent backup (database snapshot + manifest + CA cert) to a new directory.
    Backup {
        out: PathBuf,
        /// Also copy master.key and ca.key into the backup. Off by default: escrow keys separately.
        #[arg(long)]
        include_keys: bool,
    },
    /// Restore a backup into an empty database, then verify it.
    Restore {
        dir: PathBuf,
        /// Master key file when the backup doesn't include it (or set CLANKR_MASTER_KEY).
        #[arg(long)]
        master_key_file: Option<PathBuf>,
    },
    /// Check a database against a backup's manifest (audit chain, secrets, run replay, counts).
    Verify { dir: PathBuf },
    /// Compare an off-site audit export (JSONL) with the database: exit 1 if rows were removed or
    /// changed, or the export itself was edited.
    AuditCheck { export: PathBuf },
    /// Rewrap every secret's data key with the current key-encryption key: moves secrets onto
    /// --kms (or a rotated Vault key version), and upgrades rows from before envelope encryption.
    RewrapSecrets,
    /// Re-encrypt every secret under a new master key and swap `<data-dir>/master.key`. Stop
    /// every clankrd first: running ones hold the old key. The old key is kept as
    /// `master.key.<fingerprint>.old` for restoring older backups.
    RotateMasterKey {
        /// The new key (64 hex chars); generated when omitted.
        #[arg(long)]
        new_key_file: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let otel = clankrd::telemetry::init()?;
    let args = Args::parse();
    let data_dir = args.data_dir.unwrap_or_else(|| xdg("XDG_DATA_HOME", ".local/share"));
    std::fs::create_dir_all(&data_dir)?;

    // Keep the embedded server alive for the life of the process; dropping it stops Postgres.
    let mut _dev_pg = None;
    let url = match args.database_url {
        Some(u) if !args.dev => u,
        _ => {
            let pg = DevPostgres::start(Some(data_dir.join("pg"))).await.context("start embedded postgres")?;
            let url = pg.database("clankr").await?;
            _dev_pg = Some(pg);
            url
        }
    };

    if let Some(url) = &args.kms {
        let token = std::env::var("VAULT_TOKEN").context("--kms needs $VAULT_TOKEN")?;
        let _ = KMS.set(clankr_store::secrets::VaultTransit::parse(url, token)?);
    }
    let pg_bin = args.pg_bin.clone().or_else(|| _dev_pg.as_ref().map(DevPostgres::bin_dir));
    if let Some(cmd) = args.cmd {
        return run_maintenance(cmd, &url, &data_dir, pg_bin.as_deref()).await;
    }

    let store = Store::connect(&url).await.context("connect metadata store")?;
    finish_rotation(&store, &data_dir).await?;
    let master = load_or_create_master_key(&data_dir)?;
    let join_token = match std::env::var("CLANKR_JOIN_TOKEN") {
        Ok(t) => t,
        Err(_) => load_or_create_secret_file("join-token")?,
    };
    let mut state = clankrd::AppState::with_join_token(store.clone(), load_or_create_token()?, master, join_token).await?;
    if let (Some(issuer), Some(aud)) = (args.oidc_issuer, args.oidc_audience) {
        tracing::info!(%issuer, "OIDC user login enabled");
        let mut o = clankrd::authz::Oidc::new(issuer, aud, args.oidc_user_claim, args.oidc_groups_claim);
        o.client_id = args.oidc_client_id;
        state.oidc = Some(std::sync::Arc::new(o));
    }
    if let Some(kennel) = args.tool_kennel {
        tracing::info!(kennel = %kennel.display(), "sandboxed tools enabled");
        state.gateway = state.gateway.clone().with_sandbox(clankr_gateway::sandbox::Sandbox { kennel, root: args.tool_kennel_root });
    }
    let tls = if args.insecure_http {
        tracing::warn!("serving plain HTTP: credentials travel unencrypted and nodes can't join");
        None
    } else {
        let ca = clankr_tls::Ca::load_or_create(&data_dir.join("pki"))?;
        let mut names = vec!["localhost".to_owned(), "127.0.0.1".to_owned(), "::1".to_owned()];
        names.extend(args.tls_san.iter().cloned());
        let (cert, key) = ca.server_cert(&names)?;
        let cfg = ca.server_config(&cert, &key)?;
        tracing::info!(ca = %data_dir.join("pki/ca.crt").display(), "TLS enabled; clients and nodes trust this CA");
        state.ca = Some(std::sync::Arc::new(ca));
        Some(cfg)
    };
    clankrd::spawn_ticker(state.engine.clone());
    clankrd::spawn_tool_sweeper(state.gateway.clone());
    // every clankrd runs the loop; the Postgres lease makes exactly one of them the scheduler
    let holder = format!("{}-{}", std::fs::read_to_string("/etc/hostname").unwrap_or_default().trim(), std::process::id());
    if let Some(dest) = args.audit_export.clone() {
        clankrd::audit_export::spawn(store.clone(), holder.clone(), dest, std::time::Duration::from_secs(2));
    }
    clankrd::cron::spawn(store.clone(), clankr_engine::Engine::new(store.clone()), holder.clone(), std::time::Duration::from_secs(15));
    clankrd::chat::spawn(state.clone(), holder.clone(), std::time::Duration::from_secs(10));
    clankrd::scheduler::spawn(store, holder, std::time::Duration::from_secs(1));
    let app = clankrd::router(state);

    let listener = tokio::net::TcpListener::bind(args.listen).await?;
    tracing::info!(listen = %args.listen, dev = args.dev, tls = tls.is_some(), "clankrd ready");
    match tls {
        Some(cfg) => {
            tokio::select! {
                r = clankrd::tls::serve_tls(listener, app, cfg) => r?,
                _ = tokio::signal::ctrl_c() => {}
            }
        }
        None => {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = tokio::signal::ctrl_c().await;
                })
                .await?
        }
    }
    // flush buffered spans before exit
    if let Some(p) = otel {
        let _ = p.shutdown();
    }
    Ok(())
}

async fn run_maintenance(cmd: Cmd, url: &str, data_dir: &std::path::Path, pg_bin: Option<&std::path::Path>) -> anyhow::Result<()> {
    use clankrd::backup;
    let print = |r: &backup::Report| {
        for c in &r.checks {
            println!("{} {:<11} {}", if c.ok { "ok  " } else { "FAIL" }, c.name, c.detail);
        }
    };
    match cmd {
        Cmd::RewrapSecrets => {
            let store = Store::connect(url).await.context("connect metadata store")?;
            let key = existing_master_key(data_dir, None)?;
            let ctx = clankr_types::RequestCtx { actor: "clankrd rewrap-secrets".into(), request_id: "maintenance".into() };
            let n = store.secrets_rekey(&ctx, &key, &key).await.map_err(|e| anyhow::anyhow!("{}", e.message))?;
            println!("rewrapped {n} secrets with {}", if KMS.get().is_some() { "the KMS" } else { "the local master key" });
            Ok(())
        }
        Cmd::AuditCheck { export } => {
            let store = Store::connect(url).await.context("connect metadata store")?;
            let c = clankrd::audit_export::audit_check(&store, &std::fs::read_to_string(&export).with_context(|| format!("read {}", export.display()))?).await?;
            println!("{} rows in the export", c.exported);
            if let Some(s) = c.export_broken_at {
                println!("FAIL  the export's own chain breaks at seq {s} (it was edited or is incomplete)");
            }
            if !c.missing.is_empty() {
                println!("FAIL  {} exported rows are gone from the database: {:?}", c.missing.len(), c.missing);
            }
            if !c.altered.is_empty() {
                println!("FAIL  {} exported rows differ in the database: {:?}", c.altered.len(), c.altered);
            }
            anyhow::ensure!(c.ok(), "the database's audit log doesn't match its off-site copy");
            println!("ok    every exported row is in the database, unchanged");
            Ok(())
        }
        Cmd::RotateMasterKey { new_key_file } => {
            anyhow::ensure!(std::env::var_os("CLANKR_MASTER_KEY").is_none(), "the key comes from CLANKR_MASTER_KEY; rotation swaps <data-dir>/master.key, so move the key there first");
            let store = Store::connect(url).await.context("connect metadata store")?;
            finish_rotation(&store, data_dir).await?;
            let old = existing_master_key(data_dir, None)?;
            let new = match new_key_file {
                Some(f) => MasterKey::from_hex(&std::fs::read_to_string(&f).with_context(|| format!("read {}", f.display()))?)?,
                None => MasterKey::generate(),
            };
            let new = with_kms(new);
            anyhow::ensure!(backup::key_fingerprint(&new) != backup::key_fingerprint(&old), "the new key is the current key");
            // the new key is on disk before any row uses it; finish_rotation settles a crash
            let pending = data_dir.join("master.key.new");
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&pending)?;
            f.write_all(new.to_hex().as_bytes())?;
            f.sync_all()?;
            let ctx = clankr_types::RequestCtx { actor: "clankrd rotate-master-key".into(), request_id: "maintenance".into() };
            let n = match store.secrets_rekey(&ctx, &old, &new).await {
                Ok(n) => n,
                Err(e) => {
                    let _ = std::fs::remove_file(&pending);
                    anyhow::bail!("{}", e.message);
                }
            };
            swap_in_new_key(data_dir, &old)?;
            println!("re-encrypted {n} secrets; master key {} -> {}", &backup::key_fingerprint(&old)[..16], &backup::key_fingerprint(&new)[..16]);
            println!("old key kept as master.key.{}.old (older backups need it); escrow the new one", &backup::key_fingerprint(&old)[..16]);
            Ok(())
        }
        Cmd::Backup { out, include_keys } => {
            let master = existing_master_key(data_dir, None)?;
            let m = backup::backup(backup::BackupOpts { database_url: url, out: &out, master: &master, data_dir, include_keys, pg_bin }).await?;
            println!("backup written to {}", out.display());
            println!("  revision {} · audit seq {} · schema {} · {} tables", m.revision, m.audit_seq, m.schema_version, m.rows.len());
            if !include_keys {
                println!("  keys not included: the master key (sha256 {}) must be escrowed separately", &m.master_key_sha256[..16]);
            }
            Ok(())
        }
        Cmd::Restore { dir, master_key_file } => {
            let fallback = existing_master_key(data_dir, master_key_file.as_deref()).ok();
            let master = backup::restore_key(&dir, fallback)?;
            let r = backup::restore(backup::RestoreOpts { dir: &dir, database_url: url, master: &master, data_dir, pg_bin }).await?;
            print(&r);
            anyhow::ensure!(r.ok(), "restore finished but verification failed; do not serve from this database");
            println!("restored and verified; start clankrd normally (pending schema migrations apply on start)");
            let ca_key = data_dir.join("pki/ca.key");
            if !ca_key.exists() {
                println!("next: put the escrowed CA key at {} before starting, or nodes can't authenticate", ca_key.display());
            }
            Ok(())
        }
        Cmd::Verify { dir } => {
            let m = backup::read_manifest(&dir)?;
            let master = backup::restore_key(&dir, existing_master_key(data_dir, None).ok())?;
            let r = backup::verify(url, &master, &m).await?;
            print(&r);
            anyhow::ensure!(r.ok(), "verification failed");
            Ok(())
        }
    }
}

/// Keeps the old key beside it, then makes `master.key.new` the master key.
fn swap_in_new_key(data_dir: &std::path::Path, old: &MasterKey) -> anyhow::Result<()> {
    let key = data_dir.join("master.key");
    std::fs::rename(&key, data_dir.join(format!("master.key.{}.old", &clankrd::backup::key_fingerprint(old)[..16])))?;
    std::fs::rename(data_dir.join("master.key.new"), key)?;
    Ok(())
}

/// A rotation that died between writing `master.key.new` and swapping it in: if the secrets
/// decrypt with the new key the rotation committed, so finish the swap; otherwise drop it.
async fn finish_rotation(store: &Store, data_dir: &std::path::Path) -> anyhow::Result<()> {
    let pending = data_dir.join("master.key.new");
    if std::env::var_os("CLANKR_MASTER_KEY").is_some() || !pending.exists() {
        return Ok(());
    }
    let new = with_kms(MasterKey::from_hex(&std::fs::read_to_string(&pending)?)?);
    let (n, bad) = store.secrets_check(&new).await.map_err(|e| anyhow::anyhow!("{}", e.message))?;
    if n > 0 && bad.is_empty() {
        let old = existing_master_key(data_dir, None)?;
        swap_in_new_key(data_dir, &old)?;
        tracing::warn!("finished an interrupted master key rotation");
    } else {
        std::fs::remove_file(&pending)?;
        tracing::warn!("discarded an uncommitted master key rotation");
    }
    Ok(())
}

/// The KMS from --kms, set once at startup.
static KMS: std::sync::OnceLock<clankr_store::secrets::VaultTransit> = std::sync::OnceLock::new();

fn with_kms(k: MasterKey) -> MasterKey {
    match KMS.get() {
        Some(kms) => k.with_kms(kms.clone()),
        None => k,
    }
}

/// The master key from CLANKR_MASTER_KEY, an explicit file, or `<data-dir>/master.key`. Never creates one.
fn existing_master_key(data_dir: &std::path::Path, file: Option<&std::path::Path>) -> anyhow::Result<MasterKey> {
    if let Ok(k) = std::env::var("CLANKR_MASTER_KEY") {
        return MasterKey::from_hex(&k).map(with_kms);
    }
    let path = file.map_or_else(|| data_dir.join("master.key"), std::path::Path::to_path_buf);
    MasterKey::from_hex(&std::fs::read_to_string(&path).with_context(|| format!("read master key {}", path.display()))?).map(with_kms)
}

/// Secret-encryption key (SEC-09): CLANKR_MASTER_KEY (hex) or `<data-dir>/master.key` (0600).
/// Losing it makes every stored secret unreadable, so it's never regenerated if the file exists.
fn load_or_create_master_key(data_dir: &std::path::Path) -> anyhow::Result<MasterKey> {
    if let Ok(k) = std::env::var("CLANKR_MASTER_KEY") {
        return MasterKey::from_hex(&k).map(with_kms);
    }
    let path = data_dir.join("master.key");
    if path.exists() {
        anyhow::ensure!(std::fs::metadata(&path)?.permissions().mode() & 0o077 == 0, "{} must be chmod 600", path.display());
        return MasterKey::from_hex(&std::fs::read_to_string(&path)?).map(with_kms);
    }
    let key = MasterKey::generate();
    use std::io::Write;
    std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path)?.write_all(key.to_hex().as_bytes())?;
    tracing::warn!(path = %path.display(), "generated secret master key; back it up, secrets are unreadable without it");
    Ok(with_kms(key))
}

fn xdg(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").expect("HOME set")).join(fallback))
        .join("clankr")
}

/// Owner token: break-glass cluster-admin (everyone else uses OIDC or ServiceAccount tokens
/// bound by RoleBindings). The CLI reads the same file.
fn load_or_create_token() -> anyhow::Result<String> {
    if let Ok(t) = std::env::var("CLANKR_TOKEN") {
        return Ok(t);
    }
    load_or_create_secret_file("token")
}

/// A random secret in `~/.config/clankr/<name>` (0600), created on first use.
fn load_or_create_secret_file(name: &str) -> anyhow::Result<String> {
    let path = xdg("XDG_CONFIG_HOME", ".config").join(name);
    if let Ok(t) = std::fs::read_to_string(&path) {
        anyhow::ensure!(
            std::fs::metadata(&path)?.permissions().mode() & 0o077 == 0,
            "{} must not be readable by group/others (chmod 600)",
            path.display()
        );
        return Ok(t.trim().to_owned());
    }
    std::fs::create_dir_all(path.parent().unwrap())?;
    let token = Alphanumeric.sample_string(&mut rand::rng(), 40);
    use std::io::Write;
    std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path)?.write_all(token.as_bytes())?;
    tracing::info!(path = %path.display(), "created {name}");
    Ok(token)
}
