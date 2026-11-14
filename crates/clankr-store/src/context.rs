//! Versioned key/value context (ContextClaim). Every write bumps `version`; conditional writes
//! fail with Conflict instead of silently overwriting (CL-29-08).

use chrono::{DateTime, Utc};
use clankr_types::{ApiError, ErrorCode};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Store, db};

/// Bigger payloads belong in the artifact store, referenced by digest (CL-25-06, CL-36-05).
pub const MAX_VALUE_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct ContextObject {
    pub key: String,
    pub version: i64,
    pub value: Value,
    pub writer: String,
    pub updated_at: DateTime<Utc>,
}

fn not_found(claim: &str, key: &str) -> ApiError {
    ApiError::new(ErrorCode::NotFound, format!("context {claim}: key {key:?} not found"))
}

fn conflict(claim: &str, key: &str, want: i64) -> ApiError {
    let what = if want == 0 { "already exists".to_owned() } else { format!("is not at version {want}") };
    ApiError::new(ErrorCode::Conflict, format!("context {claim}: key {key:?} {what}; re-read and retry"))
}

impl Store {
    pub async fn context_get(&self, ns: &str, claim: &str, key: &str) -> Result<ContextObject, ApiError> {
        sqlx::query_as("select key, version, value, writer, updated_at from context_objects where namespace = $1 and claim = $2 and key = $3")
            .bind(ns)
            .bind(claim)
            .bind(key)
            .fetch_optional(self.pool())
            .await
            .map_err(db)?
            .ok_or_else(|| not_found(claim, key))
    }

    /// `if_version`: None = unconditional, Some(0) = create only, Some(v) = only if currently v.
    pub async fn context_put(
        &self,
        ns: &str,
        claim: &str,
        key: &str,
        value: &Value,
        if_version: Option<i64>,
        writer: &str,
    ) -> Result<ContextObject, ApiError> {
        if key.is_empty() || key.len() > 512 {
            return Err(ApiError::new(ErrorCode::InvalidSpec, "key must be 1..=512 bytes").field("key"));
        }
        let size = value.to_string().len();
        if size > MAX_VALUE_BYTES {
            return Err(ApiError::new(ErrorCode::QuotaExceeded, format!("value is {size} bytes; max {MAX_VALUE_BYTES} (store large data as an artifact)")));
        }
        const COLS: &str = "returning key, version, value, writer, updated_at";
        let q = match if_version {
            None => format!(
                "insert into context_objects (namespace, claim, key, version, value, writer) values ($1, $2, $3, 1, $4, $5)
                 on conflict (namespace, claim, key) do update
                 set version = context_objects.version + 1, value = excluded.value, writer = excluded.writer, updated_at = now() {COLS}"
            ),
            Some(0) => format!(
                "insert into context_objects (namespace, claim, key, version, value, writer) values ($1, $2, $3, 1, $4, $5)
                 on conflict do nothing {COLS}"
            ),
            Some(_) => format!(
                "update context_objects set version = version + 1, value = $4, writer = $5, updated_at = now()
                 where namespace = $1 and claim = $2 and key = $3 and version = $6 {COLS}"
            ),
        };
        let row: Option<ContextObject> = sqlx::query_as(sqlx::AssertSqlSafe(q))
            .bind(ns)
            .bind(claim)
            .bind(key)
            .bind(value)
            .bind(writer)
            .bind(if_version.unwrap_or(0))
            .fetch_optional(self.pool())
            .await
            .map_err(db)?;
        row.ok_or_else(|| conflict(claim, key, if_version.unwrap_or(0)))
    }

    pub async fn context_delete(&self, ns: &str, claim: &str, key: &str, if_version: Option<i64>) -> Result<(), ApiError> {
        let n = sqlx::query("delete from context_objects where namespace = $1 and claim = $2 and key = $3 and ($4::bigint is null or version = $4)")
            .bind(ns)
            .bind(claim)
            .bind(key)
            .bind(if_version)
            .execute(self.pool())
            .await
            .map_err(db)?
            .rows_affected();
        match (n, if_version) {
            (1, _) => Ok(()),
            (_, Some(v)) if self.context_get(ns, claim, key).await.is_ok() => Err(conflict(claim, key, v)),
            _ => Err(not_found(claim, key)),
        }
    }

    pub async fn context_list(&self, ns: &str, claim: &str, prefix: &str, limit: i64) -> Result<Vec<ContextObject>, ApiError> {
        // ponytail: prefix via starts_with; add a text_pattern_ops index if claims get large
        sqlx::query_as(
            "select key, version, value, writer, updated_at from context_objects
             where namespace = $1 and claim = $2 and starts_with(key, $3) order by key limit $4",
        )
        .bind(ns)
        .bind(claim)
        .bind(prefix)
        .bind(limit)
        .fetch_all(self.pool())
        .await
        .map_err(db)
    }
}

/// A point in a claim's history to read context at.
#[derive(Debug, Clone)]
pub enum AsOf {
    Snapshot(String),
    /// Everything written up to and including this moment.
    Time(DateTime<Utc>),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub name: String,
    pub seq: i64,
    pub created_by: String,
    pub created_at: DateTime<Utc>,
}

impl Store {
    /// Records the claim's current state under `name` (names are write-once).
    // ponytail: the snapshot covers history up to max(seq); a write committing concurrently with a
    // lower seq would show up in it afterwards. Context writes are single statements, so the window is tiny.
    pub async fn context_snapshot(&self, ns: &str, claim: &str, name: &str, who: &str) -> Result<Snapshot, ApiError> {
        if name.is_empty() || name.len() > 128 {
            return Err(ApiError::new(ErrorCode::InvalidSpec, "snapshot name must be 1..=128 bytes").field("name"));
        }
        sqlx::query_as(
            "insert into context_snapshots (namespace, claim, name, seq, created_by)
             values ($1, $2, $3, (select coalesce(max(seq), 0) from context_history), $4)
             on conflict do nothing returning name, seq, created_by, created_at",
        )
        .bind(ns)
        .bind(claim)
        .bind(name)
        .bind(who)
        .fetch_optional(self.pool())
        .await
        .map_err(db)?
        .ok_or_else(|| ApiError::new(ErrorCode::AlreadyExists, format!("context {claim}: snapshot {name:?} already exists")))
    }

    pub async fn context_snapshots(&self, ns: &str, claim: &str) -> Result<Vec<Snapshot>, ApiError> {
        sqlx::query_as("select name, seq, created_by, created_at from context_snapshots where namespace = $1 and claim = $2 order by seq desc, name")
            .bind(ns)
            .bind(claim)
            .fetch_all(self.pool())
            .await
            .map_err(db)
    }

    /// The claim's keys as they were at `at` (deleted keys are absent).
    pub async fn context_list_at(&self, ns: &str, claim: &str, prefix: &str, limit: i64, at: &AsOf) -> Result<Vec<ContextObject>, ApiError> {
        let (seq, time) = match at {
            AsOf::Snapshot(name) => {
                let seq: i64 = sqlx::query_scalar("select seq from context_snapshots where namespace = $1 and claim = $2 and name = $3")
                    .bind(ns)
                    .bind(claim)
                    .bind(name)
                    .fetch_optional(self.pool())
                    .await
                    .map_err(db)?
                    .ok_or_else(|| ApiError::new(ErrorCode::NotFound, format!("context {claim}: no snapshot {name:?}")))?;
                (Some(seq), None)
            }
            AsOf::Time(t) => (None, Some(*t)),
        };
        sqlx::query_as(
            "select key, version, value, writer, at as updated_at from (
                 select distinct on (key) key, version, value, writer, at from context_history
                 where namespace = $1 and claim = $2 and starts_with(key, $3)
                   and ($4::bigint is null or seq <= $4) and ($5::timestamptz is null or at <= $5)
                 order by key, seq desc) h
             where value is not null order by key limit $6",
        )
        .bind(ns)
        .bind(claim)
        .bind(prefix)
        .bind(seq)
        .bind(time)
        .bind(limit)
        .fetch_all(self.pool())
        .await
        .map_err(db)
    }

    /// Puts the claim back the way the snapshot recorded it: changed keys are rewritten (new
    /// versions, so readers holding old versions get Conflict), keys created since are deleted.
    /// Returns (written, deleted).
    pub async fn context_restore(&self, ctx: &clankr_types::RequestCtx, ns: &str, claim: &str, name: &str) -> Result<(u64, u64), ApiError> {
        let who = &ctx.actor;
        let want = self.context_list_at(ns, claim, "", i64::MAX, &AsOf::Snapshot(name.into())).await?;
        let mut tx = self.pool().begin().await.map_err(db)?;
        // lock the claim's rows so concurrent writers serialize behind the restore
        let now: Vec<(String, Value)> = sqlx::query_as("select key, value from context_objects where namespace = $1 and claim = $2 for update")
            .bind(ns)
            .bind(claim)
            .fetch_all(&mut *tx)
            .await
            .map_err(db)?;
        let current: std::collections::HashMap<String, Value> = now.into_iter().collect();
        let writer = format!("{who} (restore {name})");
        let mut written = 0;
        for o in &want {
            if current.get(&o.key) == Some(&o.value) {
                continue;
            }
            sqlx::query(
                "insert into context_objects (namespace, claim, key, version, value, writer) values ($1, $2, $3, 1, $4, $5)
                 on conflict (namespace, claim, key) do update
                 set version = context_objects.version + 1, value = excluded.value, writer = excluded.writer, updated_at = now()",
            )
            .bind(ns)
            .bind(claim)
            .bind(&o.key)
            .bind(&o.value)
            .bind(&writer)
            .execute(&mut *tx)
            .await
            .map_err(db)?;
            written += 1;
        }
        let keep: Vec<&str> = want.iter().map(|o| o.key.as_str()).collect();
        let deleted = sqlx::query("delete from context_objects where namespace = $1 and claim = $2 and not (key = any($3))")
            .bind(ns)
            .bind(claim)
            .bind(&keep)
            .execute(&mut *tx)
            .await
            .map_err(db)?
            .rows_affected();
        let target = crate::AuditTarget { kind: "ContextClaim", namespace: ns, name: claim, uid: None };
        crate::audit_in_tx(&mut tx, ctx, &format!("restore snapshot {name} ({written} written, {deleted} deleted)"), Some(target), true, None, None).await?;
        tx.commit().await.map_err(db)?;
        Ok((written, deleted))
    }
}
