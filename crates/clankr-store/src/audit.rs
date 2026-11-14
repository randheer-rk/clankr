//! Hash-chained audit records, written in the same transaction as the change they describe.

use chrono::{DateTime, SubsecRound, Utc};
use clankr_types::{ApiError, RequestCtx, Resource};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::PgPool;

use crate::{Tx, db};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct AuditEvent {
    pub seq: i64,
    pub at: DateTime<Utc>,
    pub actor: String,
    pub request_id: String,
    pub action: String,
    pub kind: Option<String>,
    pub namespace: Option<String>,
    pub name: Option<String>,
    pub uid: Option<String>,
    pub decision: String,
    pub reason: Option<String>,
    pub rev: Option<i64>,
    pub prev_hash: String,
    pub hash: String,
}

impl AuditEvent {
    /// Whether `hash` is right for the rest of the record.
    pub fn hash_ok(&self) -> bool {
        self.compute_hash() == self.hash
    }

    /// sha256(prev_hash || JSON of every field except `hash`). Field order is the struct order.
    fn compute_hash(&self) -> String {
        let mut body = serde_json::to_value(self).expect("serializable");
        body.as_object_mut().unwrap().remove("hash");
        let mut h = Sha256::new();
        h.update(self.prev_hash.as_bytes());
        h.update(body.to_string().as_bytes());
        hex::encode(h.finalize())
    }
}

pub struct AuditTarget<'a> {
    pub kind: &'a str,
    pub namespace: &'a str,
    pub name: &'a str,
    pub uid: Option<String>,
}

pub(crate) async fn allow(tx: &mut Tx, ctx: &RequestCtx, action: &str, r: &Resource) -> Result<(), ApiError> {
    let m = &r.metadata;
    let target = AuditTarget { kind: &r.kind, namespace: &m.namespace, name: &m.name, uid: m.uid.clone() };
    record(tx, ctx, action, Some(target), true, None, Some(m.resource_version)).await
}

pub(crate) async fn deny(tx: &mut Tx, ctx: &RequestCtx, action: &str, reason: &str) -> Result<(), ApiError> {
    record(tx, ctx, action, None, false, Some(reason), None).await
}

/// Appends one audit record inside the caller's transaction, so it commits or rolls back with the action.
pub async fn record(
    tx: &mut Tx,
    ctx: &RequestCtx,
    action: &str,
    target: Option<AuditTarget<'_>>,
    allowed: bool,
    reason: Option<&str>,
    rev: Option<i64>,
) -> Result<(), ApiError> {
    let decision = if allowed { "allow" } else { "deny" };
    // row lock on the head serializes appends, so the chain is linear
    let (seq, prev_hash): (i64, String) = sqlx::query_as("update audit_head set seq = seq + 1 returning seq, hash")
        .fetch_one(&mut **tx)
        .await
        .map_err(db)?;
    let (kind, namespace, name, uid) = match target {
        Some(t) => (Some(t.kind.to_owned()), Some(t.namespace.to_owned()), Some(t.name.to_owned()), t.uid),
        None => (None, None, None, None),
    };
    let mut e = AuditEvent {
        seq,
        // Postgres keeps microseconds; hash what will be read back
        at: Utc::now().trunc_subsecs(6),
        actor: ctx.actor.clone(),
        request_id: ctx.request_id.clone(),
        action: action.to_owned(),
        kind,
        namespace,
        name,
        uid,
        decision: decision.to_owned(),
        reason: reason.map(str::to_owned),
        rev,
        prev_hash,
        hash: String::new(),
    };
    e.hash = e.compute_hash();
    sqlx::query(
        "insert into audit_events (seq, at, actor, request_id, action, kind, namespace, name, uid, decision, reason, rev, prev_hash, hash)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)",
    )
    .bind(e.seq)
    .bind(e.at)
    .bind(&e.actor)
    .bind(&e.request_id)
    .bind(&e.action)
    .bind(&e.kind)
    .bind(&e.namespace)
    .bind(&e.name)
    .bind(&e.uid)
    .bind(&e.decision)
    .bind(&e.reason)
    .bind(e.rev)
    .bind(&e.prev_hash)
    .bind(&e.hash)
    .execute(&mut **tx)
    .await
    .map_err(db)?;
    sqlx::query("update audit_head set hash = $1").bind(&e.hash).execute(&mut **tx).await.map_err(db)?;
    Ok(())
}

pub(crate) async fn since(pool: &PgPool, after_seq: i64, limit: i64) -> Result<Vec<AuditEvent>, ApiError> {
    sqlx::query_as("select * from audit_events where seq > $1 order by seq limit $2")
        .bind(after_seq)
        .bind(limit)
        .fetch_all(pool)
        .await
        .map_err(db)
}

pub(crate) async fn latest(pool: &PgPool, limit: i64) -> Result<Vec<AuditEvent>, ApiError> {
    sqlx::query_as("select * from audit_events order by seq desc limit $1").bind(limit).fetch_all(pool).await.map_err(db)
}

// ponytail: loads the whole chain into memory; page through it once audit volume makes that hurt.
pub(crate) async fn verify(pool: &PgPool) -> Result<Option<i64>, ApiError> {
    let rows: Vec<AuditEvent> = sqlx::query_as("select * from audit_events order by seq").fetch_all(pool).await.map_err(db)?;
    let mut prev = "0".repeat(64);
    for (i, e) in rows.iter().enumerate() {
        if e.seq != i as i64 + 1 || e.prev_hash != prev || e.compute_hash() != e.hash {
            return Ok(Some(e.seq));
        }
        prev = e.hash.clone();
    }
    Ok(None)
}
