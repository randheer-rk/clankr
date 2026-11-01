//! Backup, restore and verification (PRD §37, DR runbook in docs/runbooks/backup-restore.md).
//!
//! A backup is a directory:
//!
//!   db.dump         pg_dump custom format, taken from an exported snapshot
//!   manifest.json   what the dump contains, read in that same snapshot
//!   ca.crt          cluster CA certificate (public)
//!   master.key      only with --include-keys (otherwise escrow it separately)
//!   ca.key          only with --include-keys
//!
//! Because the manifest and the dump come from one snapshot, verification after a restore can
//! demand exact equality (revision, audit head, row counts) rather than "roughly the same".

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use clankr_store::Store;
use clankr_store::secrets::MasterKey;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::Connection;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub format: u32,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub clankr_version: String,
    pub schema_version: i64,
    /// Global resource revision at the snapshot.
    pub revision: i64,
    pub audit_seq: i64,
    pub audit_head: String,
    pub rows: BTreeMap<String, i64>,
    /// SHA-256 of the master key bytes: proves the right key is used on restore without storing it.
    pub master_key_sha256: String,
    pub keys_included: bool,
    /// SHA-256 of db.dump, so a damaged or swapped dump is caught before restoring it.
    pub dump_sha256: String,
}

pub const FORMAT: u32 = 1;

fn sha256_file(p: &Path) -> Result<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(p).with_context(|| format!("open {}", p.display()))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex::encode(h.finalize()))
}

pub fn key_fingerprint(k: &MasterKey) -> String {
    hex::encode(Sha256::digest(k.to_hex().as_bytes()))
}

fn write_private(p: &Path, data: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(p).with_context(|| format!("create {}", p.display()))?.write_all(data)?;
    Ok(())
}

/// pg_dump / pg_restore: `--pg-bin`, else PATH.
pub fn pg_tool(bin_dir: Option<&Path>, name: &str) -> PathBuf {
    bin_dir.map_or_else(|| PathBuf::from(name), |d| d.join(name))
}

async fn run(cmd: &mut tokio::process::Command, what: &str) -> Result<()> {
    let out = cmd.output().await.with_context(|| format!("run {what} (is it installed? pass --pg-bin)"))?;
    if !out.status.success() {
        bail!("{what} failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

/// Facts about the database as seen by `conn` (inside the snapshot for backups).
async fn facts(conn: &mut sqlx::PgConnection) -> Result<(i64, i64, i64, String, BTreeMap<String, i64>)> {
    let schema: i64 = sqlx::query_scalar("select coalesce(max(version), 0) from _sqlx_migrations where success").fetch_one(&mut *conn).await?;
    let revision: i64 = sqlx::query_scalar("select rev from revision").fetch_one(&mut *conn).await?;
    let (seq, head): (i64, String) = sqlx::query_as("select seq, hash from audit_head").fetch_one(&mut *conn).await?;
    let tables: Vec<String> = sqlx::query_scalar(
        "select table_name::text from information_schema.tables where table_schema = 'public' and table_type = 'BASE TABLE' order by 1",
    )
    .fetch_all(&mut *conn)
    .await?;
    let mut rows = BTreeMap::new();
    for t in tables {
        // names come from the catalog, quoted as identifiers
        let n: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!("select count(*) from \"{}\"", t.replace('"', "\"\"")))).fetch_one(&mut *conn).await?;
        rows.insert(t, n);
    }
    Ok((schema, revision, seq, head, rows))
}

pub struct BackupOpts<'a> {
    pub database_url: &'a str,
    pub out: &'a Path,
    pub master: &'a MasterKey,
    /// Directory holding `pki/ca.crt` (and `pki/ca.key`).
    pub data_dir: &'a Path,
    pub include_keys: bool,
    pub pg_bin: Option<&'a Path>,
}

pub async fn backup(o: BackupOpts<'_>) -> Result<Manifest> {
    ensure!(!o.out.exists() || std::fs::read_dir(o.out)?.next().is_none(), "{} exists and is not empty", o.out.display());
    std::fs::create_dir_all(o.out)?;
    // hold a repeatable-read transaction open: pg_dump joins its snapshot, so the manifest we
    // read here describes exactly what is in the dump
    let mut conn = sqlx::PgConnection::connect(o.database_url).await.context("connect")?;
    let mut tx = conn.begin().await?;
    sqlx::query("set transaction isolation level repeatable read, read only").execute(&mut *tx).await?;
    let snapshot: String = sqlx::query_scalar("select pg_export_snapshot()").fetch_one(&mut *tx).await?;
    let (schema_version, revision, audit_seq, audit_head, rows) = facts(&mut tx).await?;
    let dump = o.out.join("db.dump");
    run(
        tokio::process::Command::new(pg_tool(o.pg_bin, "pg_dump"))
            .args(["--format=custom", "--no-owner", "--no-acl", "--snapshot", &snapshot, "--file"])
            .arg(&dump)
            .arg(o.database_url),
        "pg_dump",
    )
    .await?;
    tx.rollback().await?;

    let pki = o.data_dir.join("pki");
    if pki.join("ca.crt").exists() {
        std::fs::copy(pki.join("ca.crt"), o.out.join("ca.crt"))?;
    }
    if o.include_keys {
        write_private(&o.out.join("master.key"), o.master.to_hex().as_bytes())?;
        if pki.join("ca.key").exists() {
            write_private(&o.out.join("ca.key"), &std::fs::read(pki.join("ca.key"))?)?;
        }
    }
    let m = Manifest {
        format: FORMAT,
        created_at: chrono::Utc::now(),
        clankr_version: env!("CARGO_PKG_VERSION").into(),
        schema_version,
        revision,
        audit_seq,
        audit_head,
        rows,
        master_key_sha256: key_fingerprint(o.master),
        keys_included: o.include_keys,
        dump_sha256: sha256_file(&dump)?,
    };
    std::fs::write(o.out.join("manifest.json"), serde_json::to_vec_pretty(&m)?)?;
    Ok(m)
}

pub fn read_manifest(dir: &Path) -> Result<Manifest> {
    let m: Manifest = serde_json::from_slice(&std::fs::read(dir.join("manifest.json")).context("read manifest.json")?)?;
    ensure!(m.format == FORMAT, "backup format {} not supported by this clankrd (wants {FORMAT})", m.format);
    Ok(m)
}

/// The master key for a restore: the backup's own copy, else `fallback` (escrowed separately).
pub fn restore_key(dir: &Path, fallback: Option<MasterKey>) -> Result<MasterKey> {
    match (dir.join("master.key").exists(), fallback) {
        (true, _) => MasterKey::from_hex(&std::fs::read_to_string(dir.join("master.key"))?),
        (false, Some(k)) => Ok(k),
        (false, None) => bail!("backup has no master.key; pass the escrowed key (CLANKR_MASTER_KEY or --master-key-file)"),
    }
}

pub struct RestoreOpts<'a> {
    pub dir: &'a Path,
    pub database_url: &'a str,
    pub master: &'a MasterKey,
    /// Receives `pki/` and `master.key` from the backup when they're missing there.
    pub data_dir: &'a Path,
    pub pg_bin: Option<&'a Path>,
}

/// Restores into an empty database, then verifies it against the manifest. Refuses to touch a
/// database that already has clankr tables: restoring over live state is never what you want.
pub async fn restore(o: RestoreOpts<'_>) -> Result<Report> {
    let m = read_manifest(o.dir)?;
    ensure!(
        key_fingerprint(o.master) == m.master_key_sha256,
        "this master key is not the one the backup was taken with (its secrets would be unreadable)"
    );
    ensure!(
        m.schema_version <= Store::schema_version(),
        "backup schema {} is newer than this clankrd ({}); restore with the release that took it",
        m.schema_version,
        Store::schema_version()
    );
    let dump = o.dir.join("db.dump");
    ensure!(sha256_file(&dump)? == m.dump_sha256, "db.dump does not match the manifest checksum (damaged or replaced)");
    let mut conn = sqlx::PgConnection::connect(o.database_url).await.context("connect to restore target")?;
    let existing: i64 = sqlx::query_scalar("select count(*) from information_schema.tables where table_schema = 'public'").fetch_one(&mut conn).await?;
    ensure!(existing == 0, "target database is not empty ({existing} tables); restore into a fresh database");
    conn.close().await?;
    run(
        tokio::process::Command::new(pg_tool(o.pg_bin, "pg_restore"))
            .args(["--exit-on-error", "--single-transaction", "--no-owner", "--no-acl", "--dbname", o.database_url])
            .arg(&dump),
        "pg_restore",
    )
    .await?;

    let pki = o.data_dir.join("pki");
    std::fs::create_dir_all(&pki)?;
    for (from, to, private) in [("ca.crt", pki.join("ca.crt"), false), ("ca.key", pki.join("ca.key"), true)] {
        let src = o.dir.join(from);
        if src.exists() && !to.exists() {
            if private { write_private(&to, &std::fs::read(&src)?)? } else { std::fs::copy(&src, &to).map(drop)? }
        }
    }
    let key_file = o.data_dir.join("master.key");
    if !key_file.exists() && std::env::var("CLANKR_MASTER_KEY").is_err() {
        write_private(&key_file, o.master.to_hex().as_bytes())?;
    }
    verify(o.database_url, o.master, &m).await
}

#[derive(Debug, Default, Serialize)]
pub struct Report {
    pub checks: Vec<Check>,
}

#[derive(Debug, Serialize)]
pub struct Check {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

impl Report {
    pub fn ok(&self) -> bool {
        self.checks.iter().all(|c| c.ok)
    }

    fn add(&mut self, name: &str, ok: bool, detail: impl Into<String>) {
        self.checks.push(Check { name: name.into(), ok, detail: detail.into() });
    }
}

/// Checks a restored (or live) database against a manifest: same schema, revision, audit head
/// and row counts; an intact audit hash chain; every secret decrypts; every run's history
/// replays to its stored state. Read-only; doesn't migrate.
pub async fn verify(database_url: &str, master: &MasterKey, m: &Manifest) -> Result<Report> {
    let mut r = Report::default();
    let mut conn = sqlx::PgConnection::connect(database_url).await?;
    let (schema, revision, seq, head, rows) = facts(&mut conn).await?;
    r.add("schema", schema == m.schema_version, format!("migration {schema} (backup {})", m.schema_version));
    r.add("revision", revision == m.revision, format!("{revision} (backup {})", m.revision));
    r.add("audit head", seq == m.audit_seq && head == m.audit_head, format!("seq {seq}"));
    let diff: Vec<String> = m
        .rows
        .iter()
        .filter(|(t, n)| rows.get(*t) != Some(n))
        .map(|(t, n)| format!("{t}: {} (backup {n})", rows.get(t).map_or("missing".into(), |x| x.to_string())))
        .collect();
    r.add("row counts", diff.is_empty(), if diff.is_empty() { format!("{} tables match", m.rows.len()) } else { diff.join(", ") });

    let store = Store::open(database_url).await?;
    match store.verify_audit_chain().await? {
        None => r.add("audit chain", true, format!("{seq} records, hashes link")),
        Some(bad) => r.add("audit chain", false, format!("broken at seq {bad}")),
    }
    let (n, bad) = store.secrets_check(master).await?;
    r.add("secrets", bad.is_empty(), if bad.is_empty() { format!("{n} decrypt") } else { format!("{} of {n} don't decrypt: {}", bad.len(), bad.join(", ")) });

    let runs: Vec<(String, uuid::Uuid)> = sqlx::query_as("select namespace, run_id from workflow_runs order by created_at").fetch_all(store.pool()).await?;
    let engine = clankr_engine::Engine::new(store.clone());
    let mut diverged = vec![];
    for (ns, id) in &runs {
        if let Err(e) = engine.verify(ns, *id).await {
            diverged.push(format!("{ns}/{id}: {}", e.message));
        }
    }
    r.add("run replay", diverged.is_empty(), if diverged.is_empty() { format!("{} runs replay to their stored state", runs.len()) } else { diverged.join("; ") });
    Ok(r)
}
