//! Envelope encryption: per-secret data keys wrapped by the master key or by Vault Transit,
//! rows from before envelopes still readable, and moving between key-encryption keys rewraps
//! data keys without touching values.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use axum::Json;
use base64::Engine;
use clankr_store::Store;
use clankr_store::embedded::DevPostgres;
use clankr_store::secrets::{MasterKey, VaultTransit};
use clankr_types::RequestCtx;
use serde_json::{Value, json};

fn ctx() -> RequestCtx {
    RequestCtx { actor: "owner".into(), request_id: "sec".into() }
}

/// A stand-in for Vault's Transit engine: "encrypts" by XOR, counts calls.
async fn fake_vault() -> (String, Arc<Mutex<usize>>) {
    let calls = Arc::new(Mutex::new(0));
    const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;
    let xor = |v: Vec<u8>| v.into_iter().map(|b| b ^ 0x5a).collect::<Vec<u8>>();
    let (c1, c2) = (calls.clone(), calls.clone());
    let app = axum::Router::new()
        .route("/v1/transit/encrypt/clankr", axum::routing::post(move |Json(b): Json<Value>| async move {
            *c1.lock().unwrap() += 1;
            let p = B64.decode(b["plaintext"].as_str().unwrap()).unwrap();
            Json(json!({"data": {"ciphertext": format!("vault:v1:{}", B64.encode(xor(p)))}}))
        }))
        .route("/v1/transit/decrypt/clankr", axum::routing::post(move |Json(b): Json<Value>| async move {
            *c2.lock().unwrap() += 1;
            let c = B64.decode(b["ciphertext"].as_str().unwrap().strip_prefix("vault:v1:").unwrap()).unwrap();
            Json(json!({"data": {"plaintext": B64.encode(xor(c))}}))
        }));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("vault+http://127.0.0.1:{}/transit/clankr", l.local_addr().unwrap().port());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (url, calls)
}

#[tokio::test(flavor = "multi_thread")]
async fn envelopes_legacy_rows_and_kms() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let pg = DevPostgres::start(None).await.unwrap();
    let store = Store::connect(&pg.database("clankr").await.unwrap()).await.unwrap();
    let local = MasterKey::generate();
    let read = |k: &MasterKey, name: &str| {
        let (store, k, name) = (store.clone(), k.clone(), name.to_owned());
        async move { store.secret_read(&ctx(), &k, "ns", &name, "v", "test").await }
    };
    let kek = |name: &str| {
        let (store, name) = (store.clone(), name.to_owned());
        async move { sqlx::query_scalar::<_, Option<String>>("select kek from secrets where name = $1").bind(name).fetch_one(store.pool()).await.unwrap() }
    };

    // new writes are envelopes under the local key
    store.secret_put(&ctx(), &local, "ns", "api", &BTreeMap::from([("v".into(), "sk-1".into())])).await.unwrap();
    assert!(kek("api").await.unwrap().starts_with("local:"));
    assert_eq!(read(&local, "api").await.unwrap(), "sk-1");

    // a row from before envelopes (value encrypted directly with the master key) still reads
    let mut nonce = [7u8; 12];
    nonce[0] = 1;
    let ct = Aes256Gcm::new(&aes_gcm::Key::<Aes256Gcm>::try_from(hex::decode(local.to_hex()).unwrap().as_slice()).unwrap())
        .encrypt(&Nonce::from(nonce), Payload { msg: br#"{"v":"old"}"#, aad: b"ns/legacy/1" })
        .unwrap();
    sqlx::query("insert into secrets (namespace, name, version, keys, nonce, ciphertext) values ('ns', 'legacy', 1, '{v}', $1, $2)").bind(&nonce[..]).bind(&ct).execute(store.pool()).await.unwrap();
    assert_eq!(read(&local, "legacy").await.unwrap(), "old");

    // onto Vault: data keys rewrapped (the legacy row becomes an envelope), values untouched
    let (url, calls) = fake_vault().await;
    let with_vault = local.clone().with_kms(VaultTransit::parse(&url, "t".into()).unwrap());
    let before: Vec<u8> = sqlx::query_scalar("select ciphertext from secrets where name = 'api'").fetch_one(store.pool()).await.unwrap();
    assert_eq!(store.secrets_rekey(&ctx(), &with_vault, &with_vault).await.unwrap(), 2);
    let after: Vec<u8> = sqlx::query_scalar("select ciphertext from secrets where name = 'api'").fetch_one(store.pool()).await.unwrap();
    assert_eq!(before, after, "rewrapping doesn't re-encrypt values");
    assert_eq!((kek("api").await.as_deref(), kek("legacy").await.as_deref()), (Some("vault:clankr"), Some("vault:clankr")));
    assert_eq!(read(&with_vault, "api").await.unwrap(), "sk-1");
    assert_eq!(read(&with_vault, "legacy").await.unwrap(), "old");
    let e = read(&local, "api").await.unwrap_err();
    assert!(e.message.contains("wrapped by Vault key"), "the local key alone can't open them: {}", e.message);
    assert!(*calls.lock().unwrap() >= 4);

    // a new local key doesn't matter to Vault-wrapped secrets
    let other = MasterKey::generate().with_kms(VaultTransit::parse(&url, "t".into()).unwrap());
    assert_eq!(read(&other, "api").await.unwrap(), "sk-1");
    assert_eq!(store.secrets_check(&other).await.unwrap(), (2, vec![]));

    // and back to local-only: rewrapped by the local key again
    store.secrets_rekey(&ctx(), &with_vault, &local).await.unwrap();
    assert!(kek("api").await.unwrap().starts_with("local:"));
    assert_eq!(read(&local, "api").await.unwrap(), "sk-1");
    let wrong = MasterKey::generate();
    assert!(read(&wrong, "api").await.unwrap_err().message.contains("not the current master key"));
}
