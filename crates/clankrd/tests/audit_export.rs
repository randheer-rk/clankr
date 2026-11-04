//! Off-site audit export (CL-33-06): the copy outside the database catches what a database
//! superuser can do inside it, deleting or rewriting audit rows.

use std::sync::{Arc, Mutex};

use clankr_store::Store;
use clankr_store::embedded::DevPostgres;
use clankr_types::RequestCtx;
use clankrd::audit_export::{audit_check, export_once};

fn ctx(who: &str) -> RequestCtx {
    RequestCtx { actor: who.into(), request_id: "ae".into() }
}

#[tokio::test(flavor = "multi_thread")]
async fn export_catches_superuser_tampering() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let pg = DevPostgres::start(None).await.unwrap();
    let store = Store::connect(&pg.database("clankr").await.unwrap()).await.unwrap();
    for i in 0..5 {
        store.audit_deny(&ctx(&format!("user-{i}")), "probe", "test").await.unwrap();
    }
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("audit.jsonl");
    let dest = format!("file://{}", file.display());
    let http = reqwest::Client::new();
    assert_eq!(export_once(&store, &http, &dest).await.unwrap(), 5);
    assert_eq!(export_once(&store, &http, &dest).await.unwrap(), 0, "the cursor remembers what was sent");
    store.audit_deny(&ctx("user-5"), "probe", "test").await.unwrap();
    assert_eq!(export_once(&store, &http, &dest).await.unwrap(), 1);

    // an HTTP receiver gets the same rows, as NDJSON
    let got = Arc::new(Mutex::new(String::new()));
    let app = {
        let got = got.clone();
        axum::Router::new().route("/ingest", axum::routing::post(move |body: String| async move { got.lock().unwrap().push_str(&body); "ok" }))
    };
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://127.0.0.1:{}/ingest", l.local_addr().unwrap().port());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    assert_eq!(export_once(&store, &http, &url).await.unwrap(), 6);
    assert_eq!(got.lock().unwrap().lines().count(), 6);

    let copy = std::fs::read_to_string(&file).unwrap();
    let c = audit_check(&store, &copy).await.unwrap();
    assert!(c.ok() && c.exported == 6, "{c:?}");

    // a superuser drops the append-only guard, deletes one row and rewrites another
    sqlx::query("alter table audit_events disable trigger audit_events_no_update").execute(store.pool()).await.unwrap();
    sqlx::query("delete from audit_events where seq = 2").execute(store.pool()).await.unwrap();
    sqlx::query("update audit_events set actor = 'nobody' where seq = 4").execute(store.pool()).await.unwrap();
    let c = audit_check(&store, &copy).await.unwrap();
    assert_eq!((c.missing.clone(), c.altered.clone()), (vec![2], vec![4]), "{c:?}");
    assert!(!c.ok());

    // and an edited export is caught by its own chain
    let edited = copy.replacen("user-3", "user-x", 1);
    assert_eq!(audit_check(&store, &edited).await.unwrap().export_broken_at, Some(4));
}

/// A path-style S3 fake: PUT stores, GET returns, `?list-type=2` lists as XML. It checks that
/// every request is SigV4-signed for the s3 service and that the body hash header is right.
#[tokio::test(flavor = "multi_thread")]
async fn s3_export_is_signed_and_readable_back() {
    use axum::extract::{Path, Query, State};
    use axum::http::HeaderMap;
    use std::collections::BTreeMap;
    let _ = rustls::crypto::ring::default_provider().install_default();
    type Objects = Arc<Mutex<BTreeMap<String, String>>>;
    let objects: Objects = Default::default();
    fn check(h: &HeaderMap, body: &[u8]) {
        let auth = h.get("authorization").expect("signed").to_str().unwrap();
        assert!(auth.starts_with("AWS4-HMAC-SHA256 Credential=AKIDTEST/") && auth.contains("/eu-central-1/s3/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature="), "{auth}");
        let sha = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(body));
        assert_eq!(h.get("x-amz-content-sha256").unwrap().to_str().unwrap(), sha, "the body hash is signed");
        assert!(h.get("x-amz-date").is_some());
    }
    async fn put(State(o): State<Objects>, Path((bucket, key)): Path<(String, String)>, h: HeaderMap, body: String) -> &'static str {
        assert_eq!(bucket, "audit-bucket");
        check(&h, body.as_bytes());
        o.lock().unwrap().insert(key, body);
        ""
    }
    async fn get_object(State(o): State<Objects>, Path((_, key)): Path<(String, String)>, h: HeaderMap) -> String {
        check(&h, b"");
        o.lock().unwrap().get(&key).cloned().unwrap_or_default()
    }
    async fn list(State(o): State<Objects>, Path(_bucket): Path<String>, Query(q): Query<BTreeMap<String, String>>, h: HeaderMap) -> String {
        check(&h, b"");
        assert_eq!(q.get("list-type").map(String::as_str), Some("2"));
        let prefix = q.get("prefix").cloned().unwrap_or_default();
        let keys: String = o.lock().unwrap().keys().filter(|k| k.starts_with(&prefix)).map(|k| format!("<Contents><Key>{k}</Key></Contents>")).collect();
        format!("<ListBucketResult><IsTruncated>false</IsTruncated>{keys}</ListBucketResult>")
    }
    let app = axum::Router::new().route("/{bucket}/{*key}", axum::routing::put(put).get(get_object)).route("/{bucket}", axum::routing::get(list)).with_state(objects.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    // SAFETY: test-only process environment, set before any reader
    unsafe {
        std::env::set_var("AWS_ACCESS_KEY_ID", "AKIDTEST");
        std::env::set_var("AWS_SECRET_ACCESS_KEY", "sekrit");
        std::env::set_var("AWS_REGION", "eu-central-1");
        std::env::set_var("CLANKR_AUDIT_S3_ENDPOINT", &endpoint);
    }

    let pg = DevPostgres::start(None).await.unwrap();
    let store = Store::connect(&pg.database("clankr").await.unwrap()).await.unwrap();
    for i in 0..3 {
        store.audit_deny(&ctx(&format!("s3-{i}")), "probe", "test").await.unwrap();
    }
    let http = reqwest::Client::new();
    let dest = "s3://audit-bucket/clankr/prod";
    assert_eq!(export_once(&store, &http, dest).await.unwrap(), 3);
    store.audit_deny(&ctx("s3-3"), "probe", "test").await.unwrap();
    assert_eq!(export_once(&store, &http, dest).await.unwrap(), 1);
    let keys: Vec<String> = objects.lock().unwrap().keys().cloned().collect();
    assert_eq!(keys, ["clankr/prod/audit-000000000001-000000000003.jsonl", "clankr/prod/audit-000000000004-000000000004.jsonl"], "one object per batch, named by its seq range");

    let copy = clankrd::audit_export::read_s3(&http, dest).await.unwrap();
    assert_eq!(copy.lines().count(), 4);
    let c = audit_check(&store, &copy).await.unwrap();
    assert!(c.ok() && c.exported == 4, "{c:?}");
}
