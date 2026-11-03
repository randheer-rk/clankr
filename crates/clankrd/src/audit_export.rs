//! Off-site audit export (CL-33-06). The audit chain lives in Postgres with an append-only
//! trigger, but a database superuser can drop the trigger and rewrite history. The exporter
//! streams every row, in order, to somewhere that superuser doesn't control, and `audit_check`
//! compares that copy with the database later.
//!
//! Destinations:
//! - `file:///path/audit.jsonl`: appended and fsynced, one JSON row per line. Put it on storage
//!   that only appends (`chattr +a`, a WORM mount, a log shipper's spool).
//! - `https://…`: each batch is POSTed as NDJSON (`$CLANKR_AUDIT_EXPORT_TOKEN` as a bearer
//!   token); any 2xx counts as received.
//!
//! One replica exports at a time (leader lease `audit-export`). Delivery is at-least-once: after
//! a crash a batch may be sent again, so receivers dedupe by `seq`.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use clankr_store::Store;
use clankr_store::AuditEvent;

const LEASE: &str = "audit-export";
const BATCH: i64 = 500;

/// Exports forever (as leader), every `every`.
pub fn spawn(store: Store, holder: String, dest: String, every: Duration) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(30)).build().expect("http client");
        let mut tick = tokio::time::interval(every);
        loop {
            tick.tick().await;
            match store.acquire_leadership(LEASE, &holder, (every.as_secs_f64() * 5.0).max(10.0)).await {
                Ok(Some(_)) => {}
                Ok(None) => continue,
                Err(e) => {
                    tracing::warn!(error = %e, "audit export lease");
                    continue;
                }
            }
            if let Err(e) = export_once(&store, &http, &dest).await {
                tracing::warn!(dest = %redact(&dest), error = %format!("{e:#}"), "audit export failed; will retry");
            }
        }
    })
}

/// URLs may carry credentials; logs get the scheme and host only.
fn redact(dest: &str) -> String {
    reqwest::Url::parse(dest).map(|u| format!("{}://{}", u.scheme(), u.host_str().unwrap_or(""))).unwrap_or_else(|_| "?".into())
}

/// Sends everything after the cursor, batch by batch. Returns how many rows went out.
pub async fn export_once(store: &Store, http: &reqwest::Client, dest: &str) -> Result<usize> {
    let mut sent = 0;
    loop {
        let (seq, hash) = store.audit_export_cursor(dest).await.map_err(|e| anyhow::anyhow!(e.message))?;
        let rows = store.audit_since(seq, BATCH).await.map_err(|e| anyhow::anyhow!(e.message))?;
        let Some(last) = rows.last() else { return Ok(sent) };
        // the chain must continue from what was already sent: a rewritten history breaks here
        if rows[0].prev_hash != hash || rows.windows(2).any(|w| w[1].prev_hash != w[0].hash) || rows.iter().any(|r| !r.hash_ok()) {
            bail!("audit chain doesn't continue from the exported copy at seq {seq}; refusing to export (run `clankrd audit-check`)");
        }
        let body: String = rows.iter().map(|r| serde_json::to_string(r).expect("serializable") + "\n").collect();
        deliver(http, dest, &body).await?;
        store.set_audit_export_cursor(dest, last.seq, &last.hash).await.map_err(|e| anyhow::anyhow!(e.message))?;
        sent += rows.len();
        if (rows.len() as i64) < BATCH {
            return Ok(sent);
        }
    }
}

async fn deliver(http: &reqwest::Client, dest: &str, body: &str) -> Result<()> {
    if let Some(path) = dest.strip_prefix("file://") {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path).with_context(|| format!("open {path}"))?;
        f.write_all(body.as_bytes())?;
        f.sync_data()?;
        return Ok(());
    }
    if !(dest.starts_with("https://") || dest.starts_with("http://127.0.0.1") || dest.starts_with("http://localhost")) {
        bail!("audit export destination must be file://, https://, or http:// to loopback");
    }
    let mut rb = http.post(dest).header("content-type", "application/x-ndjson").body(body.to_owned());
    if let Ok(t) = std::env::var("CLANKR_AUDIT_EXPORT_TOKEN") {
        rb = rb.bearer_auth(t);
    }
    let r = rb.send().await.context("send audit batch")?;
    if !r.status().is_success() {
        bail!("audit receiver answered {}", r.status());
    }
    Ok(())
}

/// What comparing an exported copy with the database found.
#[derive(Debug, Default, PartialEq)]
pub struct Check {
    /// Rows in the export (deduplicated by seq).
    pub exported: usize,
    /// Where the export's own chain breaks (it was edited or truncated in the middle).
    pub export_broken_at: Option<i64>,
    /// Exported rows the database no longer has.
    pub missing: Vec<i64>,
    /// Exported rows the database has with different content.
    pub altered: Vec<i64>,
}

impl Check {
    pub fn ok(&self) -> bool {
        self.export_broken_at.is_none() && self.missing.is_empty() && self.altered.is_empty()
    }
}

/// Compares an exported JSONL copy (from `file://`, or collected from a receiver) with the
/// database: the copy must be an unbroken chain, and every row in it must still be in the
/// database, unchanged.
pub async fn audit_check(store: &Store, export: &str) -> Result<Check> {
    let mut rows: Vec<AuditEvent> = export.lines().filter(|l| !l.trim().is_empty()).map(serde_json::from_str).collect::<Result<_, _>>().context("parse export")?;
    rows.sort_by_key(|r| r.seq);
    rows.dedup_by_key(|r| r.seq);
    let mut c = Check { exported: rows.len(), ..Default::default() };
    let mut prev = "0".repeat(64);
    for (i, r) in rows.iter().enumerate() {
        if r.seq != i as i64 + 1 || r.prev_hash != prev || !r.hash_ok() {
            c.export_broken_at = Some(r.seq);
            break;
        }
        prev = r.hash.clone();
    }
    for r in &rows {
        match store.audit_at(r.seq).await.map_err(|e| anyhow::anyhow!(e.message))? {
            None => c.missing.push(r.seq),
            Some(db) if db != *r => c.altered.push(r.seq),
            Some(_) => {}
        }
    }
    Ok(c)
}
