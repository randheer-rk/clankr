//! Encrypted secret storage (SEC-09). Values are only decrypted inside the control plane (model
//! gateway, credential broker) and every decryption is audited (CL-31-10). There is no API that
//! returns a value.
//!
//! Envelope encryption: each secret version gets a random data key (AES-256-GCM) that encrypts
//! the value; the data key is stored wrapped by a key-encryption key, either the local master
//! key or a KMS (Vault Transit). Rotating the key-encryption key rewraps the data keys only.

use std::collections::BTreeMap;

use aes_gcm::aead::{Aead, KeyInit, Payload};
use base64::Engine;
use aes_gcm::{Aes256Gcm, Key, Nonce};
use chrono::{DateTime, Utc};
use clankr_types::{ApiError, ErrorCode, RequestCtx};
use serde::{Deserialize, Serialize};
use sqlx::Row;

use crate::audit::{AuditTarget, record};
use crate::{Store, db};

/// The control plane's key material: a 32-byte local key (held outside the database, file
/// with 0600 or env; it also derives the workload-identity signing key), and optionally a KMS
/// that wraps secrets' data keys instead of the local key.
#[derive(Clone)]
pub struct MasterKey(Key<Aes256Gcm>, Option<std::sync::Arc<VaultTransit>>);

/// HashiCorp Vault's Transit engine as the key-encryption key: data keys are wrapped and
/// unwrapped by Vault, and the key never leaves it.
// ponytail: Vault Transit only; AWS/GCP KMS fit the same wrap/unwrap pair when someone needs them
#[derive(Clone)]
pub struct VaultTransit {
    /// e.g. https://vault.example:8200
    pub addr: String,
    /// Transit mount and key name: `transit` / `clankr`.
    pub mount: String,
    pub key: String,
    pub token: String,
    http: reqwest::Client,
}

impl VaultTransit {
    /// `vault+https://host:8200/<mount>/<key>` (or `vault+http://` to loopback, for development).
    pub fn parse(url: &str, token: String) -> anyhow::Result<Self> {
        let rest = url.strip_prefix("vault+").ok_or_else(|| anyhow::anyhow!("KMS URL must look like vault+https://host:8200/transit/<key>"))?;
        let u = reqwest::Url::parse(rest)?;
        let loopback = matches!(u.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
        anyhow::ensure!(u.scheme() == "https" || loopback, "Vault must be reached over https (plain http only to loopback)");
        let seg: Vec<&str> = u.path_segments().map(|s| s.filter(|x| !x.is_empty()).collect()).unwrap_or_default();
        let [mount, key] = seg.as_slice() else { anyhow::bail!("KMS URL path must be /<transit mount>/<key name>") };
        let addr = format!("{}://{}", u.scheme(), u.host_str().unwrap_or_default()) + &u.port().map(|p| format!(":{p}")).unwrap_or_default();
        let http = reqwest::Client::builder().timeout(std::time::Duration::from_secs(10)).build()?;
        Ok(Self { addr, mount: (*mount).into(), key: (*key).into(), token, http })
    }

    async fn call(&self, op: &str, body: serde_json::Value) -> Result<serde_json::Value, ApiError> {
        let unavailable = |m: String| ApiError::new(ErrorCode::StorageUnavailable, format!("KMS (vault {}): {m}", self.key));
        let r = self.http.post(format!("{}/v1/{}/{op}/{}", self.addr, self.mount, self.key)).header("x-vault-token", &self.token).json(&body).send().await.map_err(|e| unavailable(e.to_string()))?;
        let status = r.status();
        let v: serde_json::Value = r.json().await.map_err(|e| unavailable(e.to_string()))?;
        if !status.is_success() {
            return Err(unavailable(format!("{status}: {}", v["errors"])));
        }
        Ok(v["data"].clone())
    }
}

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;
/// Binds a wrapped data key to its purpose, so it can't be swapped for another ciphertext.
const DEK_AAD: &[u8] = b"clankr secret data key v1";

impl MasterKey {
    pub fn from_bytes(b: [u8; 32]) -> Self {
        Self(b.into(), None)
    }

    /// Wraps data keys with `kms` from now on (the local key still unwraps what it wrapped).
    pub fn with_kms(mut self, kms: VaultTransit) -> Self {
        self.1 = Some(std::sync::Arc::new(kms));
        self
    }

    /// The same local key, without a KMS.
    pub fn local_only(&self) -> Self {
        Self(self.0, None)
    }

    fn local_ref(&self) -> String {
        use sha2::Digest;
        format!("local:{}", &hex::encode(sha2::Sha256::digest(self.0.as_slice()))[..16])
    }

    /// Wraps a data key: (wrapped bytes, which key wrapped it).
    async fn wrap(&self, dek: &[u8; 32]) -> Result<(Vec<u8>, String), ApiError> {
        if let Some(kms) = &self.1 {
            let d = kms.call("encrypt", serde_json::json!({"plaintext": B64.encode(dek)})).await?;
            let ct = d["ciphertext"].as_str().ok_or_else(|| ApiError::new(ErrorCode::StorageUnavailable, "KMS returned no ciphertext"))?;
            return Ok((ct.as_bytes().to_vec(), format!("vault:{}", kms.key)));
        }
        let mut nonce = [0u8; 12];
        getrandom::fill(&mut nonce).expect("os rng");
        let ct = Aes256Gcm::new(&self.0).encrypt(&Nonce::from(nonce), Payload { msg: dek, aad: DEK_AAD }).map_err(|_| ApiError::new(ErrorCode::StorageUnavailable, "wrap failed"))?;
        Ok(([&nonce[..], &ct].concat(), self.local_ref()))
    }

    async fn unwrap(&self, wrapped: &[u8], kek: &str) -> Result<[u8; 32], ApiError> {
        let bad = |m: &str| ApiError::new(ErrorCode::StorageUnavailable, m.to_owned());
        let dek: Vec<u8> = if let Some(key) = kek.strip_prefix("vault:") {
            let kms = self.1.as_ref().filter(|k| k.key == key).ok_or_else(|| bad(&format!("this secret's data key is wrapped by Vault key {key:?}; start clankrd with --kms for it")))?;
            let ct = std::str::from_utf8(wrapped).map_err(|_| bad("bad wrapped key"))?;
            let d = kms.call("decrypt", serde_json::json!({"ciphertext": ct})).await?;
            B64.decode(d["plaintext"].as_str().unwrap_or_default()).map_err(|_| bad("KMS returned a bad data key"))?
        } else {
            if kek != self.local_ref() {
                return Err(bad(&format!("this secret's data key is wrapped by {kek}, not the current master key ({})", self.local_ref())));
            }
            let (nonce, ct) = wrapped.split_at_checked(12).ok_or_else(|| bad("bad wrapped key"))?;
            let nonce: [u8; 12] = nonce.try_into().expect("12 bytes");
            Aes256Gcm::new(&self.0).decrypt(&Nonce::from(nonce), Payload { msg: ct, aad: DEK_AAD }).map_err(|_| bad("data key failed to unwrap (wrong master key or tampered row)"))?
        };
        dek.try_into().map_err(|_| bad("data key isn't 32 bytes"))
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
        let plain = serde_json::to_vec(data).expect("serializable");
        let (nonce, ct, dek, kek) = seal(key, &plain, &aad(ns, name, version)).await?;
        let keys: Vec<String> = data.keys().cloned().collect();
        let row = sqlx::query(
            "insert into secrets (namespace, name, version, keys, nonce, ciphertext, dek, kek) values ($1, $2, $3, $4, $5, $6, $7, $8)
             on conflict (namespace, name) do update set version = excluded.version, keys = excluded.keys,
                 nonce = excluded.nonce, ciphertext = excluded.ciphertext, dek = excluded.dek, kek = excluded.kek, updated_at = now()
             returning updated_at",
        )
        .bind(ns)
        .bind(name)
        .bind(version)
        .bind(&keys)
        .bind(&nonce[..])
        .bind(&ct)
        .bind(&dek)
        .bind(&kek)
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
        let r = sqlx::query("select version, nonce, ciphertext, dek, kek from secrets where namespace = $1 and name = $2")
            .bind(ns)
            .bind(name)
            .fetch_optional(self.pool())
            .await
            .map_err(db)?
            .ok_or_else(|| not_found(name))?;
        let version: i64 = r.get("version");
        let plain = open(master, &r, &aad(ns, name, version)).await.map_err(|e| ApiError::new(ErrorCode::StorageUnavailable, format!("secret {name}: {}", e.message)))?;
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
        let rows = sqlx::query("select namespace, name, version, nonce, ciphertext, dek, kek from secrets order by namespace, name").fetch_all(self.pool()).await.map_err(db)?;
        let mut bad = vec![];
        for r in &rows {
            let (ns, name, version): (String, String, i64) = (r.get("namespace"), r.get("name"), r.get("version"));
            if open(master, r, &aad(&ns, &name, version)).await.is_err() {
                bad.push(format!("{ns}/{name}"));
            }
        }
        Ok((rows.len(), bad))
    }

    /// Moves every secret from `old` to `new` in one transaction, all or nothing (refused if any
    /// row doesn't open with `old`). Envelope rows only get their data key rewrapped; rows from
    /// before envelopes are re-encrypted into one. Returns how many rows changed.
    pub async fn secrets_rekey(&self, ctx: &RequestCtx, old: &MasterKey, new: &MasterKey) -> Result<usize, ApiError> {
        let mut tx = self.pool().begin().await.map_err(db)?;
        let rows = sqlx::query("select namespace, name, version, nonce, ciphertext, dek, kek from secrets for update").fetch_all(&mut *tx).await.map_err(db)?;
        let refused = |ns: &str, name: &str, e: ApiError| ApiError::new(ErrorCode::StorageUnavailable, format!("secret {ns}/{name}: {}; nothing was changed", e.message));
        for r in &rows {
            let (ns, name, version): (String, String, i64) = (r.get("namespace"), r.get("name"), r.get("version"));
            let (dek, kek): (Option<Vec<u8>>, Option<String>) = (r.get("dek"), r.get("kek"));
            match (dek, kek) {
                (Some(w), Some(k)) => {
                    let plain_dek = old.unwrap(&w, &k).await.map_err(|e| refused(&ns, &name, e))?;
                    let (w, k) = new.wrap(&plain_dek).await?;
                    sqlx::query("update secrets set dek = $3, kek = $4 where namespace = $1 and name = $2").bind(&ns).bind(&name).bind(&w).bind(&k).execute(&mut *tx).await.map_err(db)?;
                }
                _ => {
                    let aad = aad(&ns, &name, version);
                    let plain = open(old, r, &aad).await.map_err(|e| refused(&ns, &name, e))?;
                    let (nonce, ct, w, k) = seal(new, &plain, &aad).await?;
                    sqlx::query("update secrets set nonce = $3, ciphertext = $4, dek = $5, kek = $6 where namespace = $1 and name = $2")
                        .bind(&ns)
                        .bind(&name)
                        .bind(&nonce[..])
                        .bind(&ct)
                        .bind(&w)
                        .bind(&k)
                        .execute(&mut *tx)
                        .await
                        .map_err(db)?;
                }
            }
        }
        record(&mut tx, ctx, &format!("secrets rewrapped ({} secrets)", rows.len()), None, true, None, None).await?;
        tx.commit().await.map_err(db)?;
        Ok(rows.len())
    }
}

/// Encrypts `plain` under a fresh data key, wrapped by `master`: (nonce, ciphertext, wrapped
/// data key, key-encryption key ref).
async fn seal(master: &MasterKey, plain: &[u8], aad: &[u8]) -> Result<([u8; 12], Vec<u8>, Vec<u8>, String), ApiError> {
    let mut dek = [0u8; 32];
    getrandom::fill(&mut dek).expect("os rng");
    let mut nonce = [0u8; 12];
    getrandom::fill(&mut nonce).expect("os rng");
    let ct = Aes256Gcm::new(&dek.into()).encrypt(&Nonce::from(nonce), Payload { msg: plain, aad }).map_err(|_| ApiError::new(ErrorCode::StorageUnavailable, "encryption failed"))?;
    let (wrapped, kek) = master.wrap(&dek).await?;
    Ok((nonce, ct, wrapped, kek))
}

/// Decrypts a `secrets` row: through its data key, or directly with the master key for rows
/// from before envelope encryption.
async fn open(master: &MasterKey, r: &sqlx::postgres::PgRow, aad: &[u8]) -> Result<Vec<u8>, ApiError> {
    let (nonce, ct): (Vec<u8>, Vec<u8>) = (r.get("nonce"), r.get("ciphertext"));
    let (dek, kek): (Option<Vec<u8>>, Option<String>) = (r.get("dek"), r.get("kek"));
    let nonce: [u8; 12] = nonce.try_into().map_err(|_| ApiError::new(ErrorCode::StorageUnavailable, "bad nonce"))?;
    let key: Key<Aes256Gcm> = match (dek, kek) {
        (Some(w), Some(k)) => master.unwrap(&w, &k).await?.into(),
        _ => master.0,
    };
    Aes256Gcm::new(&key).decrypt(&Nonce::from(nonce), Payload { msg: &ct, aad }).map_err(|_| ApiError::new(ErrorCode::StorageUnavailable, "failed to decrypt (wrong key or tampered row)"))
}
