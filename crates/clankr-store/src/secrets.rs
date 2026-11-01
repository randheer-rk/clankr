//! Encrypted secret storage (SEC-09). Values are only decrypted inside the control plane (model
//! gateway, credential broker) and every decryption is audited (CL-31-10). There is no API that
//! returns a value.

use std::collections::BTreeMap;

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use chrono::{DateTime, Utc};
use clankr_types::{ApiError, ErrorCode, RequestCtx};
use serde::{Deserialize, Serialize};
use sqlx::Row;

use crate::audit::{AuditTarget, record};
use crate::{Store, db};

/// 32-byte master key. Hold it outside the database (file with 0600, env, or KMS later).
// ponytail: single master key, no rotation yet; envelope encryption + KMS (SEC-09) when rotation is needed
#[derive(Clone)]
pub struct MasterKey(Key<Aes256Gcm>);

impl MasterKey {
    pub fn from_bytes(b: [u8; 32]) -> Self {
        Self(b.into())
    }

    pub fn generate() -> Self {
        let mut b = [0u8; 32];
        getrandom::fill(&mut b).expect("os rng");
        Self::from_bytes(b)
    }

    pub fn from_hex(s: &str) -> anyhow::Result<Self> {
        let v = hex::decode(s.trim())?;
        let b: [u8; 32] = v.try_into().map_err(|_| anyhow::anyhow!("master key must be 32 bytes (64 hex chars)"))?;
        Ok(Self::from_bytes(b))
    }

    pub fn to_hex(&self) -> String {
        hex::encode(self.0.as_slice())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SecretMeta {
    pub name: String,
    pub version: i64,
    pub keys: Vec<String>,
    pub updated_at: DateTime<Utc>,
}

fn aad(ns: &str, name: &str, version: i64) -> Vec<u8> {
    format!("{ns}/{name}/{version}").into_bytes()
}

fn not_found(name: &str) -> ApiError {
    ApiError::new(ErrorCode::NotFound, format!("secret {name} not found"))
}

impl Store {
    pub async fn secret_put(&self, ctx: &RequestCtx, key: &MasterKey, ns: &str, name: &str, data: &BTreeMap<String, String>) -> Result<SecretMeta, ApiError> {
        if data.is_empty() {
            return Err(ApiError::new(ErrorCode::InvalidSpec, "secret needs at least one key").field("data"));
        }
        let mut tx = self.pool().begin().await.map_err(db)?;
        let version: i64 = sqlx::query_scalar("select coalesce(max(version), 0) + 1 from secrets where namespace = $1 and name = $2")
            .bind(ns)
            .bind(name)
            .fetch_one(&mut *tx)
            .await
            .map_err(db)?;
        let mut nonce = [0u8; 12];
        getrandom::fill(&mut nonce).expect("os rng");
        let plain = serde_json::to_vec(data).expect("serializable");
        let ct = Aes256Gcm::new(&key.0)
            .encrypt(&Nonce::from(nonce), Payload { msg: &plain, aad: &aad(ns, name, version) })
            .map_err(|_| ApiError::new(ErrorCode::StorageUnavailable, "encryption failed"))?;
        let keys: Vec<String> = data.keys().cloned().collect();
        let row = sqlx::query(
            "insert into secrets (namespace, name, version, keys, nonce, ciphertext) values ($1, $2, $3, $4, $5, $6)
             on conflict (namespace, name) do update set version = excluded.version, keys = excluded.keys,
                 nonce = excluded.nonce, ciphertext = excluded.ciphertext, updated_at = now()
             returning updated_at",
        )
        .bind(ns)
        .bind(name)
        .bind(version)
        .bind(&keys)
        .bind(&nonce[..])
        .bind(&ct)
        .fetch_one(&mut *tx)
        .await
        .map_err(db)?;
        let target = AuditTarget { kind: "Secret", namespace: ns, name, uid: None };
        record(&mut tx, ctx, "secret write", Some(target), true, None, Some(version)).await?;
        tx.commit().await.map_err(db)?;
        Ok(SecretMeta { name: name.into(), version, keys, updated_at: row.get("updated_at") })
    }

    pub async fn secret_meta(&self, ns: &str, name: &str) -> Result<SecretMeta, ApiError> {
        let r = sqlx::query("select version, keys, updated_at from secrets where namespace = $1 and name = $2")
            .bind(ns)
            .bind(name)
            .fetch_optional(self.pool())
            .await
            .map_err(db)?
            .ok_or_else(|| not_found(name))?;
        Ok(SecretMeta { name: name.into(), version: r.get("version"), keys: r.get("keys"), updated_at: r.get("updated_at") })
    }

    pub async fn secret_list(&self, ns: &str) -> Result<Vec<SecretMeta>, ApiError> {
        let rows = sqlx::query("select name, version, keys, updated_at from secrets where namespace = $1 order by name")
            .bind(ns)
            .fetch_all(self.pool())
            .await
            .map_err(db)?;
        Ok(rows
            .into_iter()
            .map(|r| SecretMeta { name: r.get("name"), version: r.get("version"), keys: r.get("keys"), updated_at: r.get("updated_at") })
            .collect())
    }

    pub async fn secret_delete(&self, ctx: &RequestCtx, ns: &str, name: &str) -> Result<(), ApiError> {
        let mut tx = self.pool().begin().await.map_err(db)?;
        let n = sqlx::query("delete from secrets where namespace = $1 and name = $2").bind(ns).bind(name).execute(&mut *tx).await.map_err(db)?.rows_affected();
        if n == 0 {
            return Err(not_found(name));
        }
        record(&mut tx, ctx, "secret delete", Some(AuditTarget { kind: "Secret", namespace: ns, name, uid: None }), true, None, None).await?;
        tx.commit().await.map_err(db)
    }

    /// Decrypts one value for trusted control-plane use; the read is audited with `purpose`.
    pub async fn secret_read(&self, ctx: &RequestCtx, master: &MasterKey, ns: &str, name: &str, key: &str, purpose: &str) -> Result<String, ApiError> {
        let r = sqlx::query("select version, nonce, ciphertext from secrets where namespace = $1 and name = $2")
            .bind(ns)
            .bind(name)
            .fetch_optional(self.pool())
            .await
            .map_err(db)?
            .ok_or_else(|| not_found(name))?;
        let version: i64 = r.get("version");
        let (nonce, ct): (Vec<u8>, Vec<u8>) = (r.get("nonce"), r.get("ciphertext"));
        let nonce: [u8; 12] = nonce.try_into().map_err(|_| ApiError::new(ErrorCode::StorageUnavailable, format!("secret {name}: bad nonce")))?;
        let plain = Aes256Gcm::new(&master.0)
            .decrypt(&Nonce::from(nonce), Payload { msg: &ct, aad: &aad(ns, name, version) })
            .map_err(|_| ApiError::new(ErrorCode::StorageUnavailable, format!("secret {name} failed to decrypt (wrong master key or tampered row)")))?;
        let map: BTreeMap<String, String> = serde_json::from_slice(&plain).map_err(|e| ApiError::new(ErrorCode::StorageUnavailable, e.to_string()))?;
        let value = map.get(key).cloned().ok_or_else(|| ApiError::new(ErrorCode::NotFound, format!("secret {name} has no key {key:?}")))?;

        let mut tx = self.pool().begin().await.map_err(db)?;
        let target = AuditTarget { kind: "Secret", namespace: ns, name, uid: None };
        record(&mut tx, ctx, &format!("secret read {key}"), Some(target), true, Some(purpose), Some(version)).await?;
        tx.commit().await.map_err(db)?;
        Ok(value)
    }

    /// Decrypts every stored secret without returning or auditing values. Returns the number
    /// checked and the `namespace/name` of any that fail (wrong master key or tampered row).
    pub async fn secrets_check(&self, master: &MasterKey) -> Result<(usize, Vec<String>), ApiError> {
        let rows = sqlx::query("select namespace, name, version, nonce, ciphertext from secrets order by namespace, name")
            .fetch_all(self.pool())
            .await
            .map_err(db)?;
        let cipher = Aes256Gcm::new(&master.0);
        let mut bad = vec![];
        for r in &rows {
            let (ns, name, version): (String, String, i64) = (r.get("namespace"), r.get("name"), r.get("version"));
            let nonce: Vec<u8> = r.get("nonce");
            let ct: Vec<u8> = r.get("ciphertext");
            let ok = <[u8; 12]>::try_from(nonce.as_slice())
                .ok()
                .is_some_and(|n| cipher.decrypt(&Nonce::from(n), Payload { msg: &ct, aad: &aad(&ns, &name, version) }).is_ok());
            if !ok {
                bad.push(format!("{ns}/{name}"));
            }
        }
        Ok((rows.len(), bad))
    }
}
