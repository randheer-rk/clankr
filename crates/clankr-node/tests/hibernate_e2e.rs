//! Hibernation (CL-20-06): a deployment checkpoints each instance at a safe point and releases
//! its compute; resuming hands every slot its checkpoint (with its version) on fresh instances.

mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use clankr_engine::{Engine, RunStatus};
use clankr_node::fleet::{FleetConfig, run_fleet};
use clankr_types::{RequestCtx, Resource};
use serde_json::{Value, json};

const NS: &str = "hib";

// counts tasks; the count is its checkpoint, so it survives hibernation
const AGENT: &str = r#"
from clankr import AgentApp
app = AgentApp()
done = 0
@app.task()
async def work(task, ctx):
    global done
    done += 1
    return {"restored": app.checkpoint, "version": app.checkpoint_version, "done": done}
@app.on_hibernate
async def save(ctx):
    return {"done": done}
app.run()
"#;

fn ctx() -> RequestCtx {
    RequestCtx { actor: "owner".into(), request_id: "hib".into() }
}

fn res(kind: &str, name: &str, spec: Value) -> Resource {
    serde_json::from_value(json!({"apiVersion": "clankr.io/v1alpha1", "kind": kind, "metadata": {"name": name, "namespace": NS}, "spec": spec})).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn hibernate_checkpoints_releases_and_resumes() {
    let cp = common::start("owner", "join", Some(Duration::from_millis(200))).await;
    let s = cp.store.clone();
    let tmp = tempfile::tempdir().unwrap();
    let sdk: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../sdk/python").canonicalize().unwrap();
    std::fs::write(tmp.path().join("agent.py"), AGENT).unwrap();
    s.create(&ctx(), res("Agent", "a", json!({"runtime": {"type": "process", "command": ["python3", tmp.path().join("agent.py")], "env": {"PYTHONPATH": sdk}}, "resources": {"memoryMiB": 64, "cpus": 0.1}})))
        .await
        .unwrap();
    s.create(&ctx(), res("AgentDeployment", "a", json!({"agent": "a", "replicas": 1, "rollout": {"minReadySeconds": 0}}))).await.unwrap();
    s.create(&ctx(), res("Workflow", "job", json!({"entrypoint": "w", "nodes": {"w": {"type": "agent", "agentRef": "a"}}}))).await.unwrap();

    let (stop, rx) = tokio::sync::watch::channel(false);
    let node = tokio::spawn(run_fleet(
        FleetConfig {
            server: cp.base.clone(),
            ca_pem: cp.ca_pem.clone(),
            name: "n1".into(),
            join_token: "join".into(),
            capacity: json!({"memoryMiB": 4096, "cpus": 4.0, "runtimes": ["process"]}),
            labels: json!({}),
            work_dir: tmp.path().join("node"),
            kennel: "kennel".into(),
            kennel_root: None,
            heartbeat: Duration::from_millis(250),
            fence_after: Duration::from_secs(20),
            renew_before: Duration::from_secs(86_400),
        },
        rx,
    ));
    let engine = Engine::new(s.clone());
    let run_job = |tag: &'static str| {
        let (engine, s) = (engine.clone(), s.clone());
        async move {
            let run = engine.start(&ctx(), NS, "job", json!({"tag": tag}), None).await.unwrap();
            let end = Instant::now() + Duration::from_secs(30);
            loop {
                let r = engine.get(NS, run.run_id).await.unwrap();
                if r.status != RunStatus::Running {
                    assert_eq!(r.status, RunStatus::Succeeded, "{:?}", r.state.error);
                    let _ = &s;
                    return r.state.data["w"].clone();
                }
                assert!(Instant::now() < end, "run {tag} didn't finish");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    };
    let status = |s: clankr_store::Store| async move { s.get(NS, "AgentDeployment", "a").await.unwrap().status.unwrap_or_default() };
    let set_hibernate = |on: bool| {
        let s = s.clone();
        async move {
            let mut d = s.get(NS, "AgentDeployment", "a").await.unwrap();
            d.spec["hibernate"] = json!(on);
            s.update(&ctx(), d).await.unwrap();
        }
    };

    let first = run_job("one").await;
    assert_eq!(first, json!({"restored": null, "version": null, "done": 1}), "fresh start: no checkpoint");
    run_job("two").await;

    set_hibernate(true).await;
    let end = Instant::now() + Duration::from_secs(40);
    loop {
        let st = status(s.clone()).await;
        if st["phase"] == "Hibernated" {
            assert!(s.instances().await.unwrap().is_empty(), "compute released");
            assert_eq!(st["hibernation"]["checkpoints"], json!([{"slot": 0, "version": 1, "checkpointed": true}]), "{st}");
            break;
        }
        assert!(Instant::now() < end, "never hibernated: {st}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    // work queued while hibernated waits for the resume
    let queued = tokio::spawn(run_job("while asleep"));
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(!queued.is_finished(), "nothing runs while hibernated");

    set_hibernate(false).await;
    let resumed = queued.await.unwrap();
    assert_eq!(resumed, json!({"restored": {"done": 2}, "version": 1, "done": 1}), "the new instance starts from the checkpoint");
    assert_eq!(status(s.clone()).await["phase"], "Ready");

    // a second hibernation bumps the slot's version
    set_hibernate(true).await;
    let end = Instant::now() + Duration::from_secs(40);
    while status(s.clone()).await["phase"] != "Hibernated" {
        assert!(Instant::now() < end, "second hibernation stuck");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert_eq!(status(s.clone()).await["hibernation"]["checkpoints"][0]["version"], 2);

    stop.send(true).unwrap();
    node.await.unwrap().unwrap();
}
