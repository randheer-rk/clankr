//! The node re-checks ImagePolicy at every launch (SEC-07): an Agent admitted before a stricter
//! policy existed is refused by the node, not run.

use std::path::Path;
use std::time::Duration;

use clankr_node::client::Client;
use clankr_node::daemon::{Config, run_instance};
use clankr_store::Store;
use clankr_store::embedded::DevPostgres;
use clankr_types::{RequestCtx, Resource};
use serde_json::json;

const TOKEN: &str = "owner";
const NS: &str = "pol";

#[tokio::test(flavor = "multi_thread")]
async fn node_refuses_what_policy_forbids() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let pg = DevPostgres::start(None).await.unwrap();
    let store = Store::connect(&pg.database("clankr").await.unwrap()).await.unwrap();
    let state = clankrd::AppState::new(store.clone(), TOKEN.into(), clankr_store::secrets::MasterKey::generate()).await.unwrap();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let app = clankrd::router(state);
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });

    let ctx = RequestCtx { actor: "owner".into(), request_id: "pol".into() };
    let res = |kind: &str, name: &str, spec: serde_json::Value| -> Resource {
        serde_json::from_value(json!({"apiVersion": "clankr.io/v1alpha1", "kind": kind, "metadata": {"name": name, "namespace": NS}, "spec": spec})).unwrap()
    };
    let sdk = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../sdk/python").canonicalize().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.py"), "from clankr import AgentApp\napp = AgentApp()\napp.run()\n").unwrap();
    // admitted while no policy existed
    store.create(&ctx, res("Agent", "dev-tool", json!({"runtime": {"type": "process", "command": ["python3", tmp.path().join("a.py")], "env": {"PYTHONPATH": sdk}}}))).await.unwrap();

    let launch = || {
        let ready = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cfg = Config {
            agent: "dev-tool".into(),
            namespace: NS.into(),
            work_dir: tmp.path().join("node"),
            instance: "dev-tool-1".into(),
            kennel: "kennel".into(),
            kennel_root: None,
            ready: Some(ready.clone()),
            spec: None,
        };
        let (stop, rx) = tokio::sync::watch::channel(false);
        (ready, stop, tokio::spawn(run_instance(Client::new(&base, TOKEN, NS), cfg, rx)))
    };
    let wait_ready = |ready: std::sync::Arc<std::sync::atomic::AtomicBool>| async move {
        for _ in 0..50 {
            if ready.load(std::sync::atomic::Ordering::Relaxed) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        false
    };

    // without a policy it runs
    let (ready, stop, task) = launch();
    assert!(wait_ready(ready).await, "agent should start with no policy");
    stop.send(true).unwrap();
    let _ = task.await;

    // a policy appears that forbids unsandboxed process runtimes: the node refuses to launch
    store.create(&ctx, res("ImagePolicy", "sandboxed-only", json!({"allowedRepositories": ["registry.example/"]}))).await.unwrap();
    let (ready, stop, task) = launch();
    assert!(!wait_ready(ready).await, "node must not start an agent its namespace's policy forbids");
    stop.send(true).unwrap();
    let _ = task.await;
}
