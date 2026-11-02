//! Queue-driven autoscaling (M5): replicas follow the agent's queue, up at once, down after the
//! delay, to zero when idle and back when work arrives.

use std::time::Duration;

use clankr_engine::Engine;
use clankr_store::Store;
use clankr_store::embedded::DevPostgres;
use clankr_types::{RequestCtx, Resource};
use serde_json::{Value, json};

const NS: &str = "auto";

fn ctx() -> RequestCtx {
    RequestCtx { actor: "owner".into(), request_id: "auto".into() }
}

fn res(kind: &str, name: &str, spec: Value) -> Resource {
    serde_json::from_value(json!({"apiVersion": "clankr.io/v1alpha1", "kind": kind, "metadata": {"name": name, "namespace": NS}, "spec": spec})).unwrap()
}

#[tokio::test]
async fn replicas_follow_the_queue() {
    let pg = DevPostgres::start(None).await.unwrap();
    let s = Store::connect(&pg.database("clankr").await.unwrap()).await.unwrap();
    s.register_node("n1", "cred", &json!({}), &json!({"memoryMiB": 8192, "cpus": 8.0, "runtimes": ["oci"]})).await.unwrap();
    for r in [
        res("Agent", "worker", json!({"runtime": {"type": "oci", "image": format!("r.example/w@sha256:{}", "a".repeat(64))}, "resources": {"memoryMiB": 128, "cpus": 0.25}})),
        res("AgentDeployment", "worker", json!({"agent": "worker", "replicas": 1, "autoscale": {"minReplicas": 0, "maxReplicas": 4, "targetTasksPerReplica": 2, "scaleDownDelaySeconds": 1}})),
        res("Workflow", "job", json!({"entrypoint": "w", "nodes": {"w": {"type": "agent", "agentRef": "worker"}}})),
    ] {
        s.create(&ctx(), r).await.unwrap();
    }
    let lead = s.acquire_leadership("scheduler", "t", 30.0).await.unwrap().unwrap();
    let pass = || async { clankrd::scheduler::reconcile(&s, &lead).await.unwrap() };
    let count = || async { s.instances().await.unwrap().len() };

    pass().await;
    assert_eq!(count().await, 0, "idle: scaled to zero (spec.replicas is ignored when autoscaling)");

    let engine = Engine::new(s.clone());
    for i in 0..5 {
        engine.start(&ctx(), NS, "job", json!({"i": i}), None).await.unwrap();
    }
    pass().await;
    assert_eq!(count().await, 3, "5 queued / 2 per replica");
    let d = s.get(NS, "AgentDeployment", "worker").await.unwrap();
    assert_eq!(d.status.unwrap()["autoscale"], json!({"replicas": 3, "queueDepth": 5}));

    for i in 0..20 {
        engine.start(&ctx(), NS, "job", json!({"more": i}), None).await.unwrap();
    }
    pass().await;
    assert_eq!(count().await, 4, "capped at maxReplicas");

    // drain the queue: replicas hold through the delay, then drop to zero
    while let Some(t) = engine.lease(NS, "agent/worker", "w", Duration::from_secs(30)).await.unwrap() {
        engine.complete(NS, t.run_id, &t.task, t.epoch, json!({})).await.unwrap();
    }
    pass().await;
    assert_eq!(count().await, 4, "no scale-down before the delay");
    tokio::time::sleep(Duration::from_millis(1200)).await;
    pass().await;
    assert_eq!(count().await, 0, "idle long enough: back to zero");

    engine.start(&ctx(), NS, "job", json!({"late": true}), None).await.unwrap();
    pass().await;
    assert_eq!(count().await, 1, "work arriving brings an instance back");
}
