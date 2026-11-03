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
