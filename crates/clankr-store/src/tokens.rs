//! ServiceAccount API tokens (SEC-02): random, prefixed `clk_`, stored hashed, optional expiry,
//! revocable. Issue and revoke are audited.

use chrono::{DateTime, Utc};
use clankr_types::{ApiError, ErrorCode, RequestCtx};
use serde::{Deserialize, Serialize};

use crate::audit::{AuditTarget, record};
use crate::cluster::hash_credential;
use crate::{Store, db};

pub const TOKEN_PREFIX: &str = "clk_";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct ApiToken {
    pub id: String,
    pub namespace: String,
    pub service_account: String,
    pub description: String,
    pub created_by: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
}

macro_rules! cols {
    () => {
        "id, namespace, service_account, description, created_by, created_at, expires_at, revoked_at"
    };
}

fn random(n: usize) -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut b = vec![0u8; n];
    getrandom::fill(&mut b).expect("os rng");
    // 62 symbols: modulo bias is < 2^-2 per char at worst, negligible against a 40-char secret
    b.iter().map(|x| ALPHABET[*x as usize % ALPHABET.len()] as char).collect()
}

impl Store {
    /// Issues a token for `ns/sa` (which must exist). Returns the record and the plaintext,
    /// which is never retrievable again.
    pub async fn token_issue(&self, ctx: &RequestCtx, ns: &str, sa: &str, description: &str, ttl_secs: Option<i64>) -> Result<(ApiToken, String), ApiError> {
        self.get(ns, "ServiceAccount", sa).await?;
        if ttl_secs.is_some_and(|t| t <= 0) {
            return Err(ApiError::new(ErrorCode::InvalidSpec, "ttlSeconds must be positive").field("ttlSeconds"));
        }
        let id = random(12);
        let plain = format!("{TOKEN_PREFIX}{}", random(40));
        let mut tx = self.pool().begin().await.map_err(db)?;
        let t: ApiToken = sqlx::query_as(concat!(
            "insert into api_tokens (id, namespace, service_account, token_hash, description, created_by, expires_at)
             values ($1, $2, $3, $4, $5, $6, now() + make_interval(secs => $7::float8)) returning ",
            cols!()
        ))
        .bind(&id)
        .bind(ns)
        .bind(sa)
        .bind(hash_credential(&plain))
        .bind(description)
        .bind(&ctx.actor)
        .bind(ttl_secs.map(|t| t as f64))
        .fetch_one(&mut *tx)
        .await
        .map_err(db)?;
        record(&mut tx, ctx, &format!("token issue {id}"), Some(AuditTarget { kind: "ServiceAccount", namespace: ns, name: sa, uid: None }), true, None, None).await?;
        tx.commit().await.map_err(db)?;
        Ok((t, plain))
    }

    pub async fn tokens(&self, ns: &str, sa: &str) -> Result<Vec<ApiToken>, ApiError> {
        sqlx::query_as(concat!("select ", cols!(), " from api_tokens where namespace = $1 and service_account = $2 order by created_at"))
            .bind(ns)
            .bind(sa)
            .fetch_all(self.pool())
            .await
            .map_err(db)
    }

    pub async fn token_revoke(&self, ctx: &RequestCtx, ns: &str, sa: &str, id: &str) -> Result<(), ApiError> {
        let mut tx = self.pool().begin().await.map_err(db)?;
        let n = sqlx::query("update api_tokens set revoked_at = coalesce(revoked_at, now()) where id = $1 and namespace = $2 and service_account = $3")
            .bind(id)
            .bind(ns)
            .bind(sa)
            .execute(&mut *tx)
            .await
            .map_err(db)?
            .rows_affected();
        if n == 0 {
            return Err(ApiError::new(ErrorCode::NotFound, format!("token {id} not found")));
        }
        record(&mut tx, ctx, &format!("token revoke {id}"), Some(AuditTarget { kind: "ServiceAccount", namespace: ns, name: sa, uid: None }), true, None, None).await?;
        tx.commit().await.map_err(db)
    }

    /// The live (unrevoked, unexpired) token a plaintext belongs to.
    pub async fn token_lookup(&self, plain: &str) -> Result<Option<ApiToken>, ApiError> {
        if !plain.starts_with(TOKEN_PREFIX) {
            return Ok(None);
        }
        sqlx::query_as(concat!("select ", cols!(), " from api_tokens where token_hash = $1 and revoked_at is null and (expires_at is null or expires_at > now())"))
            .bind(hash_credential(plain))
            .fetch_optional(self.pool())
            .await
            .map_err(db)
    }
}
