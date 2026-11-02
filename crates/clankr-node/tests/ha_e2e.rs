//! Control-plane HA (M5): two clankrd replicas over one Postgres and one cluster CA. A fleet node
//! knows both. Killing a replica (its runtime is dropped: listener, open connections, scheduler
//! and ticker all vanish at once, like kill -9) must not stop work: the node fails over, the
//! surviving replica takes the scheduler lease, and running instances are not disturbed.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use clankr_node::fleet::{FleetConfig, run_fleet};
use clankr_store::Store;
use clankr_store::embedded::DevPostgres;
use clankr_types::{RequestCtx, Resource};
use serde_json::{Value, json};

const NS: &str = "ha";

const AGENT: &str = r#"
from clankr import AgentApp
app = AgentApp()
@app.task()
async def work(task, ctx):
    return {"ok": True, "n": task.input["input"]["n"]}
app.run()
"#;

fn ctx() -> RequestCtx {
    RequestCtx { actor: "owner".into(), request_id: "ha".into() }
}

fn res(kind: &str, name: &str, spec: Value) -> Resource {
    serde_json::from_value(json!({"apiVersion": "clankr.io/v1alpha1", "kind": kind, "metadata": {"name": name, "namespace": NS}, "spec": spec})).unwrap()
}

async fn wait<F: std::future::Future<Output = bool>>(what: &str, secs: u64, mut f: impl FnMut() -> F) {
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    panic!("timed out waiting for {what}");
}

/// One clankrd replica on its own runtime; dropping the runtime is the crash.
async fn replica(name: &'static str, db: String, pki: PathBuf) -> (tokio::runtime::Runtime, String) {
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    rt.spawn(async move {
        let store = Store::connect(&db).await.unwrap();
        let mut state = clankrd::AppState::with_join_token(store.clone(), "owner".into(), clankr_store::secrets::MasterKey::from_bytes([7; 32]), "join".into()).await.unwrap();
        let ca = clankr_tls::Ca::load_or_create(&pki).unwrap();
        let (cert, key) = ca.server_cert(&["127.0.0.1".into()]).unwrap();
        let cfg = ca.server_config(&cert, &key).unwrap();
        state.ca = Some(std::sync::Arc::new(ca));
        clankrd::spawn_ticker(state.engine.clone());
        clankrd::scheduler::spawn(store, format!("replica-{name}"), Duration::from_millis(300));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        tx.send(format!("https://127.0.0.1:{}", l.local_addr().unwrap().port())).unwrap();
        clankrd::tls::serve_tls(l, clankrd::router(state), cfg).await.unwrap();
    });
    let base = rx.await.unwrap();
    (rt, base)
}

#[tokio::test(flavor = "multi_thread")]
async fn work_continues_when_a_replica_dies() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let pg = DevPostgres::start(None).await.unwrap();
    let db = pg.database("clankr").await.unwrap();
    let store = Store::connect(&db).await.unwrap();
    let pki = tempfile::tempdir().unwrap();
    let ca_pem = clankr_tls::Ca::load_or_create(pki.path()).unwrap().cert_pem;
    let (rt_a, a) = replica("a", db.clone(), pki.path().to_owned()).await;
    let (rt_b, b) = replica("b", db.clone(), pki.path().to_owned()).await;

    let tmp = tempfile::tempdir().unwrap();
    let sdk: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../sdk/python").canonicalize().unwrap();
    std::fs::write(tmp.path().join("agent.py"), AGENT).unwrap();
    for r in [
        res("Agent", "worker", json!({"runtime": {"type": "process", "command": ["python3", tmp.path().join("agent.py")], "env": {"PYTHONPATH": sdk}}, "resources": {"memoryMiB": 128, "cpus": 0.25}, "taskTtlSeconds": 5})),
        res("AgentDeployment", "worker", json!({"agent": "worker", "replicas": 1})),
        res("Workflow", "job", json!({"entrypoint": "w", "nodes": {"w": {"type": "agent", "agentRef": "worker"}}})),
    ] {
        store.create(&ctx(), r).await.unwrap();
    }

    let (stop, rx) = tokio::sync::watch::channel(false);
    let node = tokio::spawn(run_fleet(
        FleetConfig {
            server: format!("{a}, {b}"),
            ca_pem,
            name: "node-1".into(),
            join_token: "join".into(),
            capacity: json!({"memoryMiB": 2048, "cpus": 4.0, "runtimes": ["process"]}),
            labels: json!({}),
            work_dir: tmp.path().join("node"),
            kennel: "kennel".into(),
            kennel_root: None,
            heartbeat: Duration::from_millis(400),
            fence_after: Duration::from_secs(8),
            renew_before: Duration::from_secs(86_400),
        },
        rx,
    ));
    wait("instance placed", 20, || async { store.instances().await.unwrap().iter().any(|i| i.node.is_some()) }).await;

    let engine = clankr_engine::Engine::new(store.clone());
    let finish = |n: i64| {
        let engine = engine.clone();
        async move {
            let id = engine.start(&ctx(), NS, "job", json!({"n": n}), None).await.unwrap().run_id;
            let end = Instant::now() + Duration::from_secs(30);
            loop {
                let v = engine.get(NS, id).await.unwrap();
                if v.status.is_terminal() {
                    assert_eq!(v.status, clankr_engine::RunStatus::Succeeded, "{:?}", v.state.error);
                    return v.state.data["w"]["n"].as_i64();
                }
                assert!(Instant::now() < end, "run {n} didn't finish");
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
        }
    };
    assert_eq!(finish(1).await, Some(1));
    let before = store.instances().await.unwrap().remove(0);
    let leader: String = sqlx::query_scalar("select holder from leader_leases where name = 'scheduler'").fetch_one(store.pool()).await.unwrap();

    // kill whichever replica the node is talking to first; then the other one is all that's left
    let t = Instant::now();
    rt_a.shutdown_background();
    assert_eq!(finish(2).await, Some(2), "work completes through the surviving replica");
    eprintln!("first run after losing replica a finished in {:?}", t.elapsed());
    for n in 3..6 {
        assert_eq!(finish(n).await, Some(n));
    }

    // the instance was never disturbed: same node, same epoch (no fencing churn)
    let after = store.instances().await.unwrap().remove(0);
    assert_eq!((after.id, after.node, after.epoch), (before.id, before.node, before.epoch));

    // scheduling moved to the survivor if the dead replica was leading; new work gets placed
    wait("replica b holds the scheduler lease", 15, || async {
        let h: String = sqlx::query_scalar("select holder from leader_leases where name = 'scheduler' and expires_at > now()").fetch_optional(store.pool()).await.unwrap().unwrap_or_default();
        h == "replica-b"
    })
    .await;
    eprintln!("scheduler leader was {leader}, now replica-b");
    store.create(&ctx(), res("AgentDeployment", "worker-2", json!({"agent": "worker", "replicas": 1}))).await.unwrap();
    wait("new deployment placed by replica b", 15, || async { store.instances().await.unwrap().iter().filter(|i| i.node.is_some()).count() == 2 }).await;
    let nodes = store.nodes().await.unwrap();
    assert!(nodes[0].ready, "node kept heartbeating through the failover");

    stop.send(true).unwrap();
    node.await.unwrap().unwrap();
    rt_b.shutdown_background();
}
