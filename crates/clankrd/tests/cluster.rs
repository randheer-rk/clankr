//! Leader election with fencing.

use clankr_store::Store;
use clankr_store::embedded::DevPostgres;
use clankr_types::{ErrorCode, RequestCtx, Resource};
use serde_json::{Value, json};

async fn store() -> (DevPostgres, Store) {
    let pg = DevPostgres::start(None).await.unwrap();
    let s = Store::connect(&pg.database("clankr").await.unwrap()).await.unwrap();
    (pg, s)
}

fn res(kind: &str, ns: &str, name: &str, spec: Value) -> Resource {
    serde_json::from_value(json!({"apiVersion": "clankr.io/v1alpha1", "kind": kind, "metadata": {"name": name, "namespace": ns}, "spec": spec})).unwrap()
}

fn ctx() -> RequestCtx {
    RequestCtx { actor: "owner".into(), request_id: "t".into() }
}

#[tokio::test]
async fn one_leader_and_stale_leaders_are_fenced() {
    let (_pg, s) = store().await;
    let a = s.acquire_leadership("scheduler", "a", 30.0).await.unwrap().unwrap();
    assert_eq!(a.epoch, 1);
    assert!(s.acquire_leadership("scheduler", "b", 30.0).await.unwrap().is_none(), "only one leader");
    assert_eq!(s.acquire_leadership("scheduler", "a", 30.0).await.unwrap().unwrap().epoch, 1, "renewal keeps the epoch");

    // a's lease lapses (process paused, partitioned...); b takes over with a higher epoch
    sqlx::query("update leader_leases set expires_at = now() - interval '1 second'").execute(s.pool()).await.unwrap();
    let b = s.acquire_leadership("scheduler", "b", 30.0).await.unwrap().unwrap();
    assert_eq!(b.epoch, 2);

    // the old leader wakes up and tries to schedule: refused before writing anything
    s.create(&ctx(), res("Agent", "ns", "w", json!({"runtime": {"type": "process", "command": ["true"]}}))).await.unwrap();
    s.create(&ctx(), res("AgentDeployment", "ns", "d", json!({"agent": "w", "replicas": 2}))).await.unwrap();
    let e = clankrd::scheduler::reconcile(&s, &a).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::StaleLease);
    assert!(s.instances().await.unwrap().is_empty(), "stale leader wrote nothing");
    let r = clankrd::scheduler::reconcile(&s, &b).await.unwrap();
    assert_eq!((r.created, r.pending), (2, 2), "current leader schedules; no nodes yet so both pending");
    let d = s.get("ns", "AgentDeployment", "d").await.unwrap();
    assert_eq!(d.status.as_ref().unwrap()["reason"], "no nodes registered");
}

