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

    let store = Store::connect(&url).await.context("connect metadata store")?;
    let master = load_or_create_master_key(&data_dir)?;
    let join_token = match std::env::var("CLANKR_JOIN_TOKEN") {
        Ok(t) => t,
        Err(_) => load_or_create_secret_file("join-token")?,
    };
    let mut state = clankrd::AppState::with_join_token(store.clone(), load_or_create_token()?, master, join_token).await?;
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
    // every clankrd runs the loop; the Postgres lease makes exactly one of them the scheduler
    let holder = format!("{}-{}", std::fs::read_to_string("/etc/hostname").unwrap_or_default().trim(), std::process::id());
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

/// Secret-encryption key (SEC-09): CLANKR_MASTER_KEY (hex) or `<data-dir>/master.key` (0600).
/// Losing it makes every stored secret unreadable, so it's never regenerated if the file exists.
fn load_or_create_master_key(data_dir: &std::path::Path) -> anyhow::Result<MasterKey> {
    if let Ok(k) = std::env::var("CLANKR_MASTER_KEY") {
        return MasterKey::from_hex(&k);
    }
    let path = data_dir.join("master.key");
    if path.exists() {
        anyhow::ensure!(std::fs::metadata(&path)?.permissions().mode() & 0o077 == 0, "{} must be chmod 600", path.display());
        return MasterKey::from_hex(&std::fs::read_to_string(&path)?);
    }
    let key = MasterKey::generate();
    use std::io::Write;
    std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path)?.write_all(key.to_hex().as_bytes())?;
    tracing::warn!(path = %path.display(), "generated secret master key; back it up, secrets are unreadable without it");
    Ok(key)
}

fn xdg(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").expect("HOME set")).join(fallback))
        .join("clankr")
}

/// Local owner token (CL-31-01 dev identity). The CLI reads the same file.
// ponytail: single static owner token; OIDC + scoped API tokens arrive with RBAC in M4.
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
