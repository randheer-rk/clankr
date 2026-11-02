//! Rolling restarts (M5): changing an Agent rolls through its deployment at most
//! `maxUnavailable` instances at a time, waiting for each restarted instance to be ready; a
//! release that never becomes ready stalls after one instance instead of taking them all down.

mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use clankr_node::fleet::{FleetConfig, run_fleet};
use clankr_types::{RequestCtx, Resource};
use serde_json::{Value, json};

const NS: &str = "roll";

const AGENT: &str = r#"
from clankr import AgentApp
app = AgentApp()
@app.task()
async def work(task, ctx):
    return {}
app.run()
"#;

fn ctx() -> RequestCtx {
    RequestCtx { actor: "owner".into(), request_id: "roll".into() }
}

fn res(kind: &str, name: &str, spec: Value) -> Resource {
    serde_json::from_value(json!({"apiVersion": "clankr.io/v1alpha1", "kind": kind, "metadata": {"name": name, "namespace": NS}, "spec": spec})).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn rolling_restart_keeps_capacity_and_stalls_on_a_bad_release() {
    let cp = common::start("owner", "join", Some(Duration::from_millis(200))).await;
    let s = cp.store.clone();
    let tmp = tempfile::tempdir().unwrap();
    let sdk: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../sdk/python").canonicalize().unwrap();
    std::fs::write(tmp.path().join("agent.py"), AGENT).unwrap();
    let agent_spec = |version: &str, cmd: Value| json!({"runtime": {"type": "process", "command": cmd, "env": {"PYTHONPATH": sdk, "VERSION": version}}, "resources": {"memoryMiB": 64, "cpus": 0.1}});
    let good = json!(["python3", tmp.path().join("agent.py")]);
    s.create(&ctx(), res("Agent", "a", agent_spec("1", good.clone()))).await.unwrap();
    s.create(&ctx(), res("AgentDeployment", "a", json!({"agent": "a", "replicas": 3, "rollout": {"maxUnavailable": 1, "minReadySeconds": 1}}))).await.unwrap();

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
    let ready = |generation: i64| {
        let s = s.clone();
        async move { s.instances().await.unwrap().iter().filter(|i| i.agent_generation == generation && i.available(chrono::Duration::seconds(1))).count() }
    };
    let deadline = |secs| Instant::now() + Duration::from_secs(secs);
    let end = deadline(30);
    while ready(1).await < 3 {
        assert!(Instant::now() < end, "initial instances never became ready");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // roll to version 2, sampling how many instances are down at any moment
    let mut a = s.get(NS, "Agent", "a").await.unwrap();
    a.spec = agent_spec("2", good.clone());
    let v2 = s.update(&ctx(), a).await.unwrap().metadata.generation;
    let (mut worst, end) = (0, deadline(60));
    while ready(v2).await < 3 {
        let down = s.instances().await.unwrap().iter().filter(|i| !i.available(chrono::Duration::zero())).count();
        worst = worst.max(down);
        assert!(Instant::now() < end, "rollout didn't finish");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(worst, 1, "never more than maxUnavailable instances down during the rollout");
    let d = s.get(NS, "AgentDeployment", "a").await.unwrap();
    tokio::time::sleep(Duration::from_millis(600)).await;
    let d2 = s.get(NS, "AgentDeployment", "a").await.unwrap();
    assert_eq!(d2.status.as_ref().unwrap()["phase"], "Ready", "{:?} / {:?}", d.status, d2.status);
    assert_eq!(d2.status.unwrap()["available"], 3);

    // a broken release: the first restarted instance never gets ready, so the rest stay put
    let mut a = s.get(NS, "Agent", "a").await.unwrap();
    a.spec = agent_spec("3", json!(["python3", "-c", "import sys; sys.exit(3)"]));
    let bad = s.update(&ctx(), a).await.unwrap().metadata.generation;
    tokio::time::sleep(Duration::from_secs(4)).await;
    let inst = s.instances().await.unwrap();
    assert_eq!(inst.iter().filter(|i| i.agent_generation == bad).count(), 1, "only one instance tried the bad release");
    assert_eq!(inst.iter().filter(|i| i.agent_generation == v2 && i.available(chrono::Duration::zero())).count(), 2, "the others keep serving");
    let status = s.get(NS, "AgentDeployment", "a").await.unwrap().status.unwrap();
    assert_eq!(status["phase"], "RollingOut", "{status}");
    assert!(status["rollout"].as_str().unwrap().contains("1/3 on generation"), "{status}");

    // fixing the Agent resumes and completes the rollout
    let mut a = s.get(NS, "Agent", "a").await.unwrap();
    a.spec = agent_spec("4", good);
    let fixed = s.update(&ctx(), a).await.unwrap().metadata.generation;
    let end = deadline(60);
    while ready(fixed).await < 3 {
        assert!(Instant::now() < end, "rollout after the fix didn't finish");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    stop.send(true).unwrap();
    node.await.unwrap().unwrap();
}
