//! Restart budgets (CL-20-04): an agent that keeps crashing is restarted with backoff until its
//! `restartPolicy` runs out; then its node gives up, the deployment reports CrashLoop with the
//! reason, and a fixed Agent revision brings it back.

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use clankr_node::fleet::{FleetConfig, run_fleet};
use clankr_types::{RequestCtx, Resource};
use serde_json::{Value, json};

const NS: &str = "rs";

fn ctx() -> RequestCtx {
    RequestCtx { actor: "owner".into(), request_id: "rs".into() }
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

#[tokio::test(flavor = "multi_thread")]
async fn crash_looping_agents_stop_restarting_until_fixed() {
    let cp = common::start("owner", "join", Some(Duration::from_millis(300))).await;
    let s = cp.store.clone();
    let tmp = tempfile::tempdir().unwrap();
    let sdk = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../sdk/python").canonicalize().unwrap();
    let ok = tmp.path().join("ok.py");
    std::fs::write(&ok, "from clankr import AgentApp\napp = AgentApp()\n@app.task()\nasync def t(task, ctx):\n    return {}\napp.run()\n").unwrap();
    let starts = tmp.path().join("starts");
    let crashing = json!({"runtime": {"type": "process", "command": ["sh", "-c", format!("echo x >> {}; exit 3", starts.display())]},
        "restartPolicy": {"maxRestarts": 2, "windowSeconds": 60}});
    s.create(&ctx(), res("Agent", "flaky", crashing)).await.unwrap();
    s.create(&ctx(), res("AgentDeployment", "flaky", json!({"agent": "flaky", "replicas": 1}))).await.unwrap();

    let (stop, rx) = tokio::sync::watch::channel(false);
    let node = tokio::spawn(run_fleet(
        FleetConfig {
            server: cp.base.clone(),
            ca_pem: cp.ca_pem.clone(),
            name: "node-a".into(),
            join_token: "join".into(),
            capacity: json!({"memoryMiB": 1024, "cpus": 2.0, "runtimes": ["process"]}),
            labels: json!({}),
            work_dir: tmp.path().join("node"),
            kennel: "kennel".into(),
            kennel_root: None,
            heartbeat: Duration::from_millis(300),
            fence_after: Duration::from_secs(10),
            renew_before: Duration::from_secs(86_400),
        },
        rx,
    ));

    let status = || {
        let s = s.clone();
        async move { s.get(NS, "AgentDeployment", "flaky").await.unwrap().status.unwrap_or_default() }
    };
    wait("deployment reports CrashLoop", 40, || async { status().await["phase"] == "CrashLoop" }).await;
    let st = status().await;
    let reason = st["crashLooping"][0]["reason"].as_str().unwrap_or_default();
    assert!(reason.contains("CrashLoop: 3 failures in 60s") && reason.contains("exit status: 3"), "{st}");
    // the budget allows 2 restarts: 3 starts, then no more
    let count = || std::fs::read_to_string(&starts).unwrap_or_default().lines().count();
    assert_eq!(count(), 3);
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(count(), 3, "no restarts after giving up");

    // a fixed revision is a reassignment: a fresh budget, and the instance comes up
    let mut a = s.get(NS, "Agent", "flaky").await.unwrap();
    a.spec["runtime"] = json!({"type": "process", "command": ["python3", ok], "env": {"PYTHONPATH": sdk}});
    s.update(&ctx(), a).await.unwrap();
    wait("deployment healthy again", 40, || async {
        let st = status().await;
        st["phase"] == "Ready" && st["crashLooping"].is_null() && st["available"] == 1
    })
    .await;

    stop.send(true).unwrap();
    let _ = node.await;
}
