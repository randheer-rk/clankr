//! Context snapshots (every write kept; read as of a snapshot or a moment; restore) and run
//! time travel / fork (a new run from any point of another's history).

use std::time::Duration;

use clankr_engine::{Engine, Event, RunStatus};
use clankr_store::Store;
use clankr_store::context::AsOf;
use clankr_store::embedded::DevPostgres;
use clankr_types::{ErrorCode, RequestCtx, Resource};
use serde_json::{Value, json};

const NS: &str = "tt";

fn ctx() -> RequestCtx {
    RequestCtx { actor: "owner".into(), request_id: "tt".into() }
}

fn res(kind: &str, name: &str, spec: Value) -> Resource {
    serde_json::from_value(json!({"apiVersion": "clankr.io/v1alpha1", "kind": kind, "metadata": {"name": name, "namespace": NS}, "spec": spec})).unwrap()
}

async fn store() -> (DevPostgres, Store) {
    let pg = DevPostgres::start(None).await.unwrap();
    let s = Store::connect(&pg.database("clankr").await.unwrap()).await.unwrap();
    (pg, s)
}

fn kv(objs: &[clankr_store::context::ContextObject]) -> Vec<(String, Value)> {
    objs.iter().map(|o| (o.key.clone(), o.value.clone())).collect()
}

#[tokio::test]
async fn context_snapshots_read_back_and_restore() {
    let (_pg, s) = store().await;
    s.context_put(NS, "mem", "k1", &json!("v1"), None, "a").await.unwrap();
    s.context_put(NS, "mem", "k2", &json!({"n": 1}), None, "a").await.unwrap();
    s.context_put(NS, "other", "k1", &json!("elsewhere"), None, "a").await.unwrap();
    let snap = s.context_snapshot(NS, "mem", "before", "owner").await.unwrap();
    assert_eq!(s.context_snapshot(NS, "mem", "before", "owner").await.unwrap_err().code, ErrorCode::AlreadyExists);
    tokio::time::sleep(Duration::from_millis(20)).await;
    let t1 = chrono::Utc::now();
    tokio::time::sleep(Duration::from_millis(20)).await;

    s.context_put(NS, "mem", "k1", &json!("v2"), Some(1), "b").await.unwrap();
    s.context_delete(NS, "mem", "k2", None).await.unwrap();
    s.context_put(NS, "mem", "k3", &json!("new"), None, "b").await.unwrap();

    let then = vec![("k1".into(), json!("v1")), ("k2".into(), json!({"n": 1}))];
    assert_eq!(kv(&s.context_list_at(NS, "mem", "", 100, &AsOf::Snapshot("before".into())).await.unwrap()), then);
    assert_eq!(kv(&s.context_list_at(NS, "mem", "", 100, &AsOf::Time(t1)).await.unwrap()), then, "as of a moment, not just snapshots");
    assert_eq!(kv(&s.context_list_at(NS, "mem", "k2", 100, &AsOf::Time(chrono::Utc::now())).await.unwrap()), vec![], "deleted keys are gone now");
    assert_eq!(s.context_snapshots(NS, "mem").await.unwrap()[0].seq, snap.seq);

    assert_eq!(s.context_restore(&ctx(), NS, "mem", "before").await.unwrap(), (2, 1), "k1 and k2 rewritten, k3 deleted");
    assert_eq!(kv(&s.context_list(NS, "mem", "", 100).await.unwrap()), then);
    assert_eq!(s.context_get(NS, "mem", "k1").await.unwrap().version, 3, "a restore is a new version; stale writers get Conflict");
    assert_eq!(s.context_get(NS, "other", "k1").await.unwrap().value, json!("elsewhere"), "other claims untouched");
    assert_eq!(s.context_restore(&ctx(), NS, "mem", "before").await.unwrap(), (0, 0), "restoring again changes nothing");
    assert_eq!(s.context_restore(&ctx(), NS, "mem", "nope").await.unwrap_err().code, ErrorCode::NotFound);
}

#[tokio::test]
async fn runs_can_be_inspected_at_any_point_and_forked() {
    let (_pg, s) = store().await;
    s.create(&ctx(), res("Workflow", "wf", json!({"entrypoint": "plan", "nodes": {"plan": {"type": "agent", "agentRef": "planner", "next": "write"}, "write": {"type": "agent", "agentRef": "writer"}}})))
        .await
        .unwrap();
    let e = Engine::new(s.clone());
    let run = e.start(&ctx(), NS, "wf", json!({"topic": "canaries"}), None).await.unwrap();
    let t = e.lease(NS, "agent/planner", "w", Duration::from_secs(30)).await.unwrap().unwrap();
    e.complete(NS, t.run_id, &t.task, t.epoch, json!({"outline": ["a", "b"]})).await.unwrap();
    let t = e.lease(NS, "agent/writer", "w", Duration::from_secs(30)).await.unwrap().unwrap();
    e.fail(NS, t.run_id, &t.task, t.epoch, "writer crashed", false).await.unwrap();
    let orig = e.get(NS, run.run_id).await.unwrap();
    assert_eq!(orig.status, RunStatus::Failed);

    // time travel: the run right after the plan was completed
    let h = e.history(NS, run.run_id).await.unwrap();
    let done = h.iter().find(|x| matches!(x.event, Event::TaskCompleted { .. })).unwrap().seq;
    let then = e.state_at(NS, run.run_id, done).await.unwrap();
    assert_eq!(then.state.data["plan"], json!({"outline": ["a", "b"]}));
    assert_eq!(then.state.status, RunStatus::Running);
    assert!(then.state.tasks.is_empty(), "mid-decision: the write task isn't scheduled yet at this exact event");
    assert_eq!(e.state_at(NS, run.run_id, 999).await.unwrap_err().code, ErrorCode::InvalidSpec);

    // fork there with a tweak: the fork finishes the decision, then the write task runs again
    let patch = json!({"input": {"topic": "canaries", "tone": "dry"}}).as_object().unwrap().clone();
    let fork = e.fork(&ctx(), NS, run.run_id, done, patch).await.unwrap();
    assert_eq!((fork.forked_from, fork.status), (Some(run.run_id), RunStatus::Running));
    assert!(fork.forked_at.unwrap() > done, "snapped to the end of that decision");
    assert_eq!(fork.state.data["input"]["tone"], "dry");
    assert_eq!(fork.state.data["plan"], json!({"outline": ["a", "b"]}), "the plan isn't redone");
    assert_eq!(fork.state.tasks.len(), 1);
    let t = e.lease(NS, "agent/writer", "w", Duration::from_secs(30)).await.unwrap().unwrap();
    assert_eq!(t.run_id, fork.run_id);
    assert!(e.lease(NS, "agent/planner", "w", Duration::from_secs(1)).await.unwrap().is_none(), "nothing before the fork point reruns");
    e.complete(NS, t.run_id, &t.task, t.epoch, json!({"draft": "..."})).await.unwrap();
    let fork = e.get(NS, fork.run_id).await.unwrap();
    assert_eq!(fork.status, RunStatus::Succeeded);
    e.verify(NS, fork.run_id).await.expect("the fork's history replays to its state");
    assert_eq!(e.get(NS, run.run_id).await.unwrap().status, RunStatus::Failed, "the original is untouched");

    let last = h.last().unwrap().seq;
    assert_eq!(e.fork(&ctx(), NS, run.run_id, last, Default::default()).await.unwrap_err().code, ErrorCode::InvalidSpec, "can't fork a finished run's end");
}
