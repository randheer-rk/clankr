//! Canary releases: a new Agent generation runs on `canary.replicas` instances while the rest
//! stay on the stable one; failing tasks or a canary that never gets ready rolls everything
//! back, a healthy analysis window promotes it to a normal rolling update.

use std::time::Duration;

use clankr_engine::Engine;
use clankr_store::Store;
use clankr_store::embedded::DevPostgres;
use clankr_types::{RequestCtx, Resource};
use serde_json::{Value, json};

const NS: &str = "can";

fn ctx() -> RequestCtx {
    RequestCtx { actor: "owner".into(), request_id: "can".into() }
}

fn res(kind: &str, name: &str, spec: Value) -> Resource {
    serde_json::from_value(json!({"apiVersion": "clankr.io/v1alpha1", "kind": kind, "metadata": {"name": name, "namespace": NS}, "spec": spec})).unwrap()
}

fn agent(version: &str) -> Value {
    json!({"runtime": {"type": "process", "command": ["agent"], "env": {"VERSION": version}}, "resources": {"memoryMiB": 64, "cpus": 0.1}})
}

#[tokio::test]
async fn canary_rolls_back_bad_releases_and_promotes_good_ones() {
    let pg = DevPostgres::start(None).await.unwrap();
    let s = Store::connect(&pg.database("clankr").await.unwrap()).await.unwrap();
    s.register_node("n1", "cred", &json!({}), &json!({"memoryMiB": 8192, "cpus": 8.0, "runtimes": ["process"]})).await.unwrap();
    s.create(&ctx(), res("Agent", "w", agent("1"))).await.unwrap();
    let canary = json!({"replicas": 1, "analysisSeconds": 1, "progressDeadlineSeconds": 2, "maxFailureRatio": 0.5});
    s.create(&ctx(), res("AgentDeployment", "w", json!({"agent": "w", "replicas": 4, "rollout": {"maxUnavailable": 4, "minReadySeconds": 0, "canary": canary}}))).await.unwrap();
    s.create(&ctx(), res("Workflow", "job", json!({"entrypoint": "w", "nodes": {"w": {"type": "agent", "agentRef": "w"}}}))).await.unwrap();

    let lead = s.acquire_leadership("scheduler", "t", 30.0).await.unwrap().unwrap();
    let pass = || async { clankrd::scheduler::reconcile(&s, &lead).await.unwrap() };
    // every instance reports ready at its current epoch
    let ready = || async {
        let reports: Vec<_> = s.instances().await.unwrap().into_iter().map(|i| (i.id, i.epoch, true, None)).collect();
        s.report_readiness("n1", &reports).await.unwrap();
    };
    let gens = || async {
        let mut g: Vec<i64> = s.instances().await.unwrap().iter().map(|i| i.agent_generation).collect();
        g.sort();
        g
    };
    let canary_status = || async { s.get(NS, "AgentDeployment", "w").await.unwrap().status.unwrap()["canary"].clone() };
    let release = |v: &'static str| {
        let s = s.clone();
        async move {
            let mut a = s.get(NS, "Agent", "w").await.unwrap();
            a.spec = agent(v);
            s.update(&ctx(), a).await.unwrap().metadata.generation
        }
    };
    let spec_of = |id: String| {
        let s = s.clone();
        async move {
            sqlx::query_scalar::<_, sqlx::types::Json<Value>>("select agent_spec from instances where id = $1").bind(id).fetch_one(s.pool()).await.unwrap().0
        }
    };

    pass().await;
    let g1 = s.get(NS, "Agent", "w").await.unwrap().metadata.generation;
    assert_eq!(gens().await, vec![g1; 4]);
    assert!(canary_status().await.is_null(), "a fresh deployment has nothing to canary against");
    ready().await;

    // bad release: one canary, the others hold on g1 even across passes
    let g2 = release("2-bad").await;
    pass().await;
    pass().await;
    assert_eq!(gens().await, vec![g1, g1, g1, g2], "exactly one canary");
    assert_eq!(canary_status().await["phase"], "Canary");
    ready().await;
    let canary_id = s.instances().await.unwrap().into_iter().find(|i| i.agent_generation == g2).unwrap().id;
    let engine = Engine::new(s.clone());
    for i in 0..2 {
        engine.start(&ctx(), NS, "job", json!({"i": i}), None).await.unwrap();
        let t = engine.lease(NS, "agent/w", &format!("agent/w/{canary_id}"), Duration::from_secs(30)).await.unwrap().unwrap();
        engine.fail(NS, t.run_id, &t.task, t.epoch, "boom", false).await.unwrap();
    }
    pass().await;
    let st = canary_status().await;
    assert_eq!(st["phase"], "Failed", "{st}");
    assert!(st["reason"].as_str().unwrap().contains("2 of 2 canary tasks failed"), "{st}");
    assert_eq!(gens().await, vec![g1; 4], "rolled back");
    assert_eq!(spec_of(canary_id.clone()).await["runtime"]["env"]["VERSION"], "1", "the canary runs the stable spec again");
    pass().await;
    assert_eq!(gens().await, vec![g1; 4], "a failed generation stays rolled back");
    ready().await;

    // good release: canary, healthy for the analysis window, then promoted to everyone
    let g3 = release("3").await;
    pass().await;
    assert_eq!(gens().await, vec![g1, g1, g1, g3]);
    ready().await;
    pass().await;
    assert_eq!(canary_status().await["phase"], "Canary", "analysis window not over yet");
    tokio::time::sleep(Duration::from_millis(1100)).await;
    pass().await;
    assert_eq!(canary_status().await["phase"], "Promoted");
    assert_eq!(gens().await, vec![g3; 4]);
    for i in s.instances().await.unwrap() {
        assert_eq!(spec_of(i.id).await["runtime"]["env"]["VERSION"], "3");
    }
    ready().await;

    // a canary that never becomes ready fails at the progress deadline
    let g4 = release("4-hangs").await;
    pass().await;
    assert_eq!(gens().await, vec![g3, g3, g3, g4]);
    tokio::time::sleep(Duration::from_millis(2100)).await;
    pass().await;
    let st = canary_status().await;
    assert_eq!(st["phase"], "Failed");
    assert!(st["reason"].as_str().unwrap().contains("not ready within 2s"), "{st}");
    assert_eq!(gens().await, vec![g3; 4]);
}
