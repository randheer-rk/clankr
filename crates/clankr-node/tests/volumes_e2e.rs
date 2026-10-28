//! Local volumes and node maintenance across two fleet nodes (CL-29-02/03/06, E2E-05/E2E-14):
//! data persists, a claim pins its instance to one node, ReadWriteOnce allows one attachment,
//! drain moves what can move and reports what can't, and node loss never restarts a stateful
//! instance on empty storage elsewhere.

mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use clankr_node::fleet::{FleetConfig, run_fleet};
use clankr_types::{RequestCtx, Resource};
use serde_json::{Value, json};

const NS: &str = "vol";

/// Increments a counter file on the mounted volume (CLANKR_VOLUMES maps mountPath -> host dir).
const STATEFUL: &str = r#"
import json, os
from clankr import AgentApp
app = AgentApp()
@app.task()
async def bump(task, ctx):
    data = json.loads(os.environ["CLANKR_VOLUMES"])["/data"]
    path = os.path.join(data, "counter")
    n = int(open(path).read()) + 1 if os.path.exists(path) else 1
    open(path, "w").write(str(n))
    return {"count": n}
app.run()
"#;

const STATELESS: &str = r#"
from clankr import AgentApp
app = AgentApp()
@app.task()
async def noop(task, ctx):
    return {}
app.run()
"#;

fn ctx() -> RequestCtx {
    RequestCtx { actor: "owner".into(), request_id: "vol".into() }
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
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("timed out waiting for {what}");
}

fn node(cp: &common::ControlPlane, name: &str, dir: &Path) -> (tokio::sync::watch::Sender<bool>, tokio::task::JoinHandle<anyhow::Result<()>>) {
    let (stop, rx) = tokio::sync::watch::channel(false);
    let cfg = FleetConfig {
        server: cp.base.clone(),
        ca_pem: cp.ca_pem.clone(),
        name: name.into(),
        join_token: "join".into(),
        capacity: json!({"memoryMiB": 2048, "cpus": 4.0, "runtimes": ["process"]}),
        labels: json!({}),
        work_dir: dir.join(name),
        kennel: "kennel".into(),
        kennel_root: None,
        heartbeat: Duration::from_millis(500),
        fence_after: Duration::from_secs(10),
        renew_before: Duration::from_secs(86_400),
    };
    (stop, tokio::spawn(run_fleet(cfg, rx)))
}

#[tokio::test(flavor = "multi_thread")]
async fn local_volumes_pin_persist_and_block_unsafe_moves() {
    let cp = common::start("owner", "join", Some(Duration::from_millis(300))).await;
    let s = cp.store.clone();
    let tmp = tempfile::tempdir().unwrap();
    let sdk: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../sdk/python").canonicalize().unwrap();
    let agent = |name: &str, code: &str, vols: Value| {
        let script = tmp.path().join(format!("{name}.py"));
        std::fs::write(&script, code).unwrap();
        res("Agent", name, json!({"runtime": {"type": "process", "command": ["python3", script], "env": {"PYTHONPATH": sdk}},
            "resources": {"memoryMiB": 256, "cpus": 0.25}, "taskTtlSeconds": 5, "volumes": vols}))
    };
    for r in [
        res("VolumeClaim", "ws", json!({"capacityMiB": 64})),
        agent("stateful", STATEFUL, json!([{"claim": "ws", "mountPath": "/data"}])),
        agent("stateless", STATELESS, json!([])),
        res("AgentDeployment", "stateful", json!({"agent": "stateful", "replicas": 1})),
        res("AgentDeployment", "stateless", json!({"agent": "stateless", "replicas": 2})),
        res("Workflow", "bump", json!({"entrypoint": "b", "nodes": {"b": {"type": "agent", "agentRef": "stateful"}}})),
    ] {
        s.create(&ctx(), r).await.unwrap();
    }
    // shared-writer modes are refused for local disks
    let rwx = clankr_types::agent::admit_volume_claim(&json!({"capacityMiB": 1, "accessMode": "ReadWriteMany"})).unwrap_err();
    assert_eq!(rwx.code, clankr_types::ErrorCode::UnsupportedCapability);

    let mut nodes = std::collections::HashMap::new();
    for n in ["node-a", "node-b"] {
        nodes.insert(n.to_owned(), node(&cp, n, tmp.path()));
    }
    let placed = |dep: &'static str| {
        let s = s.clone();
        async move { s.instances().await.unwrap().into_iter().filter(|i| i.deployment == dep).collect::<Vec<_>>() }
    };
    wait("3 instances placed", 20, || async { s.instances().await.unwrap().iter().filter(|i| i.node.is_some()).count() == 3 }).await;

    let engine = clankr_engine::Engine::new(s.clone());
    let bump = |key: String| {
        let engine = engine.clone();
        async move {
            let id = engine.start(&ctx(), NS, "bump", json!(null), Some(&key)).await.unwrap().run_id;
            for _ in 0..240 {
                let v = engine.get(NS, id).await.unwrap();
                if v.status.is_terminal() {
                    assert_eq!(v.status, clankr_engine::RunStatus::Succeeded, "{:?}", v.state.error);
                    return v.state.data["b"]["count"].as_i64().unwrap();
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            panic!("bump run didn't finish");
        }
    };
    assert_eq!(bump("1".into()).await, 1);
    assert_eq!(bump("2".into()).await, 2, "state persisted on the volume");

    let home = placed("stateful").await[0].node.clone().unwrap();
    let binding = s.volume_bindings().await.unwrap();
    assert_eq!((binding.len(), binding[0].node.as_str()), (1, home.as_str()), "claim bound to the node that first ran it");

    // RWO: a second replica can't attach the claim
    let mut d = s.get(NS, "AgentDeployment", "stateful").await.unwrap();
    d.spec["replicas"] = json!(2);
    s.update(&ctx(), d).await.unwrap();
    wait("second replica pending with RWO note", 10, || async {
        placed("stateful").await.iter().any(|i| i.node.is_none() && i.note.as_deref().is_some_and(|n| n.contains("ReadWriteOnce")))
    })
    .await;
    let mut d = s.get(NS, "AgentDeployment", "stateful").await.unwrap();
    d.spec["replicas"] = json!(1);
    s.update(&ctx(), d).await.unwrap();

    // drain the stateful node: stateless instances leave, the stateful one stays and says why
    s.set_node_maintenance(&home, true, true).await.unwrap();
    wait("stateless moved off the draining node", 15, || async { placed("stateless").await.iter().all(|i| i.node.as_deref() != Some(home.as_str())) }).await;
    wait("drain blocker reported", 10, || async {
        placed("stateful").await.iter().any(|i| i.node.as_deref() == Some(home.as_str()) && i.note.as_deref().is_some_and(|n| n.starts_with("drain blocked: local volume vol/ws")))
    })
    .await;
    assert_eq!(bump("3".into()).await, 3, "blocked instance keeps serving");
    s.set_node_maintenance(&home, false, false).await.unwrap();

    // the volume's node dies: the instance waits for it instead of starting empty elsewhere
    let t = Instant::now();
    let (stop, task) = nodes.remove(&home).unwrap();
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
    wait("stateful instance unplaced with a local-volume reason", 25, || async {
        placed("stateful").await.iter().all(|i| i.node.is_none() && i.note.as_deref().is_some_and(|n| n.contains("is local to node") && n.contains("data doesn't move")))
    })
    .await;
    eprintln!("stateful unplaced {:?} after its node stopped", t.elapsed());
    assert!(placed("stateless").await.iter().all(|i| i.node.is_some()), "stateless instances run on the other node");
    let dep = s.get(NS, "AgentDeployment", "stateful").await.unwrap();
    assert_eq!(dep.status.unwrap()["phase"], "Pending");

    // the node comes back (same name, same disk): the instance returns with its data
    nodes.insert(home.clone(), node(&cp, &home, tmp.path()));
    wait("stateful back on its node", 20, || async { placed("stateful").await.iter().all(|i| i.node.as_deref() == Some(home.as_str())) }).await;
    assert_eq!(bump("4".into()).await, 4, "data survived the node restart");

    for (_, (stop, task)) in nodes {
        let _ = stop.send(true);
        let _ = task.await;
    }
}
