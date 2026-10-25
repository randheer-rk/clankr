//! Two fleet nodes, one deployment: instances spread, work completes, a node dies, its instance
//! moves to the survivor, work keeps completing (CL-18-02, DEMO-03-style worker loss).
//! Nodes join over TLS with their own certificates and act under workload identity tokens.

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use clankr_node::fleet::{FleetConfig, run_fleet};
use clankr_types::{RequestCtx, Resource};
use serde_json::{Value, json};

const NS: &str = "fleet";

const AGENT: &str = r#"
import os
from clankr import AgentApp
app = AgentApp()
@app.task()
async def work(task, ctx):
    return {"by": os.environ["CLANKR_SOCKET"].split("/")[-2]}
app.run()
"#;

fn ctx() -> RequestCtx {
    RequestCtx { actor: "owner".into(), request_id: "fleet".into() }
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

#[tokio::test(flavor = "multi_thread")]
async fn instances_spread_and_survive_node_loss() {
    let cp = common::start("owner", "join", Some(Duration::from_millis(300))).await;
    let (store, base) = (cp.store.clone(), cp.base.clone());

    let tmp = tempfile::tempdir().unwrap();
    let script = tmp.path().join("agent.py");
    std::fs::write(&script, AGENT).unwrap();
    let sdk = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../sdk/python").canonicalize().unwrap();
    let mut nodes = vec![];
    for name in ["node-a", "node-b"] {
        let (stop, rx) = tokio::sync::watch::channel(false);
        let cfg = FleetConfig {
            server: base.clone(),
            ca_pem: cp.ca_pem.clone(),
            name: name.into(),
            join_token: "join".into(),
            capacity: json!({"memoryMiB": 1024, "cpus": 2.0, "runtimes": ["process"]}),
            labels: json!({}),
            work_dir: tmp.path().join(name),
            kennel: "kennel".into(),
            kennel_root: None,
            heartbeat: Duration::from_millis(500),
            fence_after: Duration::from_secs(10),
            renew_before: Duration::from_secs(86_400),
        };
        nodes.push((stop, tokio::spawn(run_fleet(cfg, rx))));
    }

    let r = |k: &str, n: &str, spec: Value| -> Resource {
        serde_json::from_value(json!({"apiVersion": "clankr.io/v1alpha1", "kind": k, "metadata": {"name": n, "namespace": NS}, "spec": spec})).unwrap()
    };
    store.create(&ctx(), r("Agent", "worker", json!({
        "runtime": {"type": "process", "command": ["python3", script], "env": {"PYTHONPATH": sdk}},
        "resources": {"memoryMiB": 512, "cpus": 0.5}, "taskTtlSeconds": 5}))).await.unwrap();
    store.create(&ctx(), r("AgentDeployment", "worker", json!({"agent": "worker", "replicas": 2}))).await.unwrap();
    let mut nodes_spec = json!({"entrypoint": "fan", "nodes": {"fan": {"type": "agent", "agentRef": "worker", "next": ["a", "b", "c", "d"]}}});
    for n in ["a", "b", "c", "d"] {
        nodes_spec["nodes"][n] = json!({"type": "agent", "agentRef": "worker"});
    }
    store.create(&ctx(), r("Workflow", "wf", nodes_spec)).await.unwrap();

    // spread: one instance per node (most-free-memory scoring)
    let s = store.clone();
    wait("both instances placed", 15, || {
        let s = s.clone();
        async move {
            let i = s.instances().await.unwrap();
            i.len() == 2 && i.iter().all(|x| x.node.is_some()) && i[0].node != i[1].node
        }
    })
    .await;

    let engine = clankr_engine::Engine::new(store.clone());
    let run = |key: &'static str| {
        let engine = engine.clone();
        async move {
            let id = engine.start(&ctx(), NS, "wf", json!(null), Some(key)).await.unwrap().run_id;
            for _ in 0..240 {
                let v = engine.get(NS, id).await.unwrap();
                if v.status.is_terminal() {
                    engine.verify(NS, id).await.unwrap();
                    return v;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            panic!("run {key} didn't finish");
        }
    };
    let first = run("before").await;
    assert_eq!(first.status, clankr_engine::RunStatus::Succeeded, "{:?}", first.state.error);

    // node-a dies: no more heartbeats, its instances stop
    let (stop_a, task_a) = nodes.remove(0);
    stop_a.send(true).unwrap();
    task_a.await.unwrap().unwrap();
    let t = Instant::now();
    let s = store.clone();
    wait("instance moved to node-b", 30, || {
        let s = s.clone();
        async move { s.instances().await.unwrap().iter().all(|x| x.node.as_deref() == Some("node-b")) }
    })
    .await;
    eprintln!("rescheduled after {:?}", t.elapsed());
    let moved = store.instances().await.unwrap();
    assert!(moved.iter().any(|i| i.epoch > 1), "reassignment bumps the epoch");
    let nodes_now = store.nodes().await.unwrap();
    assert!(!nodes_now.iter().find(|n| n.name == "node-a").unwrap().ready);

    let second = run("after").await;
    assert_eq!(second.status, clankr_engine::RunStatus::Succeeded, "{:?}", second.state.error);
    let d = store.get(NS, "AgentDeployment", "worker").await.unwrap();
    assert_eq!(d.status.unwrap()["scheduled"], 2);

    for (stop, task) in nodes {
        stop.send(true).unwrap();
        let _ = task.await;
    }
}

/// Certificate rotation under load: the node renews on every heartbeat (renew_before exceeds the
/// cert lifetime) while workflows run; its instances switch to each new cert without a hiccup.
#[tokio::test(flavor = "multi_thread")]
async fn node_certificates_rotate_without_disrupting_work() {
    let cp = common::start("owner", "join", Some(Duration::from_millis(300))).await;
    let store = cp.store.clone();
    let tmp = tempfile::tempdir().unwrap();
    let script = tmp.path().join("agent.py");
    std::fs::write(&script, AGENT).unwrap();
    let sdk = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../sdk/python").canonicalize().unwrap();
    let (stop, rx) = tokio::sync::watch::channel(false);
    let cfg = FleetConfig {
        server: cp.base.clone(),
        ca_pem: cp.ca_pem.clone(),
        name: "rotor".into(),
        join_token: "join".into(),
        capacity: json!({"memoryMiB": 1024, "cpus": 2.0, "runtimes": ["process"]}),
        labels: json!({}),
        work_dir: tmp.path().join("rotor"),
        kennel: "kennel".into(),
        kennel_root: None,
        heartbeat: Duration::from_millis(400),
        fence_after: Duration::from_secs(10),
        renew_before: Duration::from_secs(31 * 86_400),
    };
    let task = tokio::spawn(run_fleet(cfg, rx));
    let r = |k: &str, n: &str, spec: Value| -> Resource {
        serde_json::from_value(json!({"apiVersion": "clankr.io/v1alpha1", "kind": k, "metadata": {"name": n, "namespace": NS}, "spec": spec})).unwrap()
    };
    store.create(&ctx(), r("Agent", "worker", json!({"runtime": {"type": "process", "command": ["python3", script], "env": {"PYTHONPATH": sdk}}, "resources": {"memoryMiB": 128}}))).await.unwrap();
    store.create(&ctx(), r("AgentDeployment", "worker", json!({"agent": "worker", "replicas": 1}))).await.unwrap();
    store.create(&ctx(), r("Workflow", "one", json!({"entrypoint": "a", "nodes": {"a": {"type": "agent", "agentRef": "worker"}}}))).await.unwrap();

    let engine = clankr_engine::Engine::new(store.clone());
    let renewals = || async { store.audit_since(0, 10_000).await.unwrap().into_iter().filter(|e| e.action == "node renew").count() };
    // keep work flowing until the cert has rotated several times underneath it
    let mut k = 0;
    while renewals().await < 4 || k < 6 {
        assert!(k < 200, "rotation never happened enough times");
        k += 1;
        let id = engine.start(&ctx(), NS, "one", json!(null), Some(&format!("rot-{k}"))).await.unwrap().run_id;
        let mut done = false;
        for _ in 0..120 {
            let v = engine.get(NS, id).await.unwrap();
            if v.status.is_terminal() {
                assert_eq!(v.status, clankr_engine::RunStatus::Succeeded, "{:?}", v.state.error);
                done = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(done, "run {k} didn't finish during rotation");
    }
    eprintln!("{k} runs completed across {} certificate rotations", renewals().await);
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
}
