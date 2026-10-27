//! Test control plane: Postgres + clankrd served over TLS with a fresh cluster CA.

#![allow(dead_code)]

use std::sync::Arc;

use clankr_store::Store;
use clankr_store::embedded::DevPostgres;

pub struct ControlPlane {
    pub _pg: DevPostgres,
    pub _pki: tempfile::TempDir,
    pub store: Store,
    pub base: String,
    pub ca_pem: String,
}

pub async fn start(owner: &str, join: &str, scheduler_every: Option<std::time::Duration>) -> ControlPlane {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let pg = DevPostgres::start(None).await.unwrap();
    let store = Store::connect(&pg.database("clankr").await.unwrap()).await.unwrap();
    let mut state = clankrd::AppState::with_join_token(store.clone(), owner.into(), clankr_store::secrets::MasterKey::generate(), join.into()).await.unwrap();
    let pki = tempfile::tempdir().unwrap();
    let ca = clankr_tls::Ca::load_or_create(pki.path()).unwrap();
    let (cert, key) = ca.server_cert(&["localhost".into(), "127.0.0.1".into()]).unwrap();
    let cfg = ca.server_config(&cert, &key).unwrap();
    let ca_pem = ca.cert_pem.clone();
    state.ca = Some(Arc::new(ca));
    clankrd::spawn_ticker(state.engine.clone());
    if let Some(every) = scheduler_every {
        clankrd::scheduler::spawn(store.clone(), "test-scheduler".into(), every);
    }
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("https://127.0.0.1:{}", l.local_addr().unwrap().port());
    let app = clankrd::router(state);
    tokio::spawn(async move { clankrd::tls::serve_tls(l, app, cfg).await.unwrap() });
    ControlPlane { _pg: pg, _pki: pki, store, base, ca_pem }
}
