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
