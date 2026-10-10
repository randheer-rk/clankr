use std::collections::BTreeSet;
use std::time::Duration;

use clankr_engine::{Engine, Event, RunStatus};
use clankr_store::Store;
use clankr_store::embedded::DevPostgres;
use clankr_types::{ErrorCode, RequestCtx, Resource};
use serde_json::{Value, json};

const NS: &str = "research";
const TTL: Duration = Duration::from_secs(30);

fn ctx() -> RequestCtx {
    RequestCtx { actor: "owner".into(), request_id: "t".into() }
}

async fn setup(workflows: &[(&str, &str)]) -> (DevPostgres, Store, Engine) {
    let pg = DevPostgres::start(None).await.unwrap();
    let store = Store::connect(&pg.database("clankr").await.unwrap()).await.unwrap();
    for (name, yaml) in workflows {
        let spec: Value = serde_yaml_ng::from_str(yaml).unwrap();
        let r: Resource = serde_json::from_value(json!({
            "apiVersion": "clankr.io/v1alpha1", "kind": "Workflow",
            "metadata": {"name": name, "namespace": NS}, "spec": spec
        }))
        .unwrap();
        store.create(&ctx(), r).await.unwrap();
    }
    let engine = Engine::new(store.clone());
    (pg, store, engine)
}

const RESEARCH: &str = "
entrypoint: plan
nodes:
  plan: {type: agent, agentRef: planner, next: [research, verify]}
  research: {type: agent, agentRef: researcher, next: gather}
  verify: {type: agent, agentRef: verifier, next: gather}
  gather: {type: join, next: review}
  review: {type: approval, approverGroup: editors, next: publish}
  publish: {type: activity, toolRef: publisher}";

#[tokio::test]
async fn research_run_end_to_end_with_replay_check() {
    let (_pg, _s, e) = setup(&[("research-report", RESEARCH)]).await;
    let run = e.start(&ctx(), NS, "research-report", json!({"topic": "durability"}), None).await.unwrap();
    let id = run.run_id;
    assert_eq!(run.status, RunStatus::Running);

    let t = e.lease(NS, "agent/planner", "w1", TTL).await.unwrap().unwrap();
    assert_eq!((t.task.as_str(), t.epoch), ("plan#1.1", 1));
    assert_eq!(t.input["input"]["topic"], "durability");
    assert!(e.lease(NS, "agent/planner", "w1", TTL).await.unwrap().is_none(), "one delivery per ready task");
    e.complete(NS, id, &t.task, t.epoch, json!({"topics": ["a"]})).await.unwrap();

    for q in ["agent/researcher", "agent/verifier"] {
        let t = e.lease(NS, q, "w1", TTL).await.unwrap().unwrap();
        assert_eq!(t.input["plan"]["topics"][0], "a", "later tasks see earlier results");
        e.complete(NS, id, &t.task, t.epoch, json!({"ok": true})).await.unwrap();
    }
    assert!(e.get(NS, id).await.unwrap().state.waits.contains_key("approve.review"));

    // wrong signal is a Conflict and records nothing
    let seq_before = e.get(NS, id).await.unwrap().seq;
    assert_eq!(e.signal(&ctx(), NS, id, "approve.nope", json!({})).await.unwrap_err().code, ErrorCode::Conflict);
    assert_eq!(e.get(NS, id).await.unwrap().seq, seq_before);

    e.signal(&ctx(), NS, id, "approve.review", json!({"approved": true, "by": "ed"})).await.unwrap();
    let t = e.lease(NS, "tool/publisher", "w1", TTL).await.unwrap().unwrap();
    e.complete(NS, id, &t.task, t.epoch, json!({"url": "x"})).await.unwrap();

    let run = e.get(NS, id).await.unwrap();
    assert_eq!(run.status, RunStatus::Succeeded);
    e.verify(NS, id).await.unwrap();
    let h = e.history(NS, id).await.unwrap();
    assert_eq!(h.iter().filter(|x| matches!(x.event, Event::RunCompleted)).count(), 1);
}

#[tokio::test]
async fn stale_lease_is_fenced_and_duplicate_ack_is_idempotent() {
    let (_pg, s, e) = setup(&[("one", "entrypoint: a\nnodes:\n  a: {type: agent, agentRef: x}")]).await;
    let id = e.start(&ctx(), NS, "one", json!(null), None).await.unwrap().run_id;
    let old = e.lease(NS, "agent/x", "w-old", TTL).await.unwrap().unwrap();

    // the old worker goes silent; its lease expires and the task is redelivered with a new epoch
    sqlx::query("update tasks set lease_expires = now() - interval '1 second'").execute(s.pool()).await.unwrap();
    assert_eq!(e.tick().await.unwrap().leases_expired, 1);
    let new = e.lease(NS, "agent/x", "w-new", TTL).await.unwrap().unwrap();
    assert_eq!((new.task.as_str(), new.epoch), (old.task.as_str(), old.epoch + 1));

    // E2E-04 / CL-22-03: the stale holder can neither extend nor commit
    assert_eq!(e.heartbeat(NS, id, &old.task, old.epoch, TTL).await.unwrap_err().code, ErrorCode::StaleLease);
    assert_eq!(e.complete(NS, id, &old.task, old.epoch, json!("stale")).await.unwrap_err().code, ErrorCode::StaleLease);
    e.heartbeat(NS, id, &new.task, new.epoch, TTL).await.unwrap();
    e.complete(NS, id, &new.task, new.epoch, json!("fresh")).await.unwrap();
    // transport-level duplicate of the accepted ack: success, no second transition
    e.complete(NS, id, &new.task, new.epoch, json!("fresh")).await.unwrap();

    let run = e.get(NS, id).await.unwrap();
    assert_eq!(run.status, RunStatus::Succeeded);
    assert_eq!(run.state.data["a"], "fresh");
    let completions = e.history(NS, id).await.unwrap().into_iter().filter(|h| matches!(h.event, Event::TaskCompleted { .. })).count();
    assert_eq!(completions, 1);
    e.verify(NS, id).await.unwrap();
}

#[tokio::test]
async fn retry_is_delayed_by_backoff() {
    let (_pg, s, e) = setup(&[("r", "entrypoint: a\nretryPolicy: {maxAttempts: 2, initialBackoff: 1h, maxBackoff: 1h}\nnodes:\n  a: {type: agent, agentRef: x}")]).await;
    let id = e.start(&ctx(), NS, "r", json!(null), None).await.unwrap().run_id;
    let t = e.lease(NS, "agent/x", "w", TTL).await.unwrap().unwrap();
    e.fail(NS, id, &t.task, t.epoch, "flaky", true).await.unwrap();
    assert!(e.lease(NS, "agent/x", "w", TTL).await.unwrap().is_none(), "retry must wait out its backoff");
    sqlx::query("update tasks set not_before = now() where task = 'a#1.2'").execute(s.pool()).await.unwrap();
    let t = e.lease(NS, "agent/x", "w", TTL).await.unwrap().unwrap();
    assert_eq!(t.attempt, 2);
    e.fail(NS, id, &t.task, t.epoch, "still flaky", true).await.unwrap();
    let run = e.get(NS, id).await.unwrap();
    assert_eq!(run.status, RunStatus::Failed);
    e.verify(NS, id).await.unwrap();
}

#[tokio::test]
async fn idempotent_start_timers_cancel_and_namespace_isolation() {
    let (_pg, s, e) = setup(&[("t", "entrypoint: wait\nnodes:\n  wait: {type: timer, duration: 1h, next: go}\n  go: {type: agent, agentRef: g}")]).await;

    let a = e.start(&ctx(), NS, "t", json!(1), Some("req-42")).await.unwrap();
    let b = e.start(&ctx(), NS, "t", json!(2), Some("req-42")).await.unwrap();
    assert_eq!(a.run_id, b.run_id, "same idempotency key, same run (CL-35-02)");
    let id = a.run_id;

    assert_eq!(e.tick().await.unwrap().timers_fired, 0, "not due yet");
    sqlx::query("update timers set due_at = now()").execute(s.pool()).await.unwrap();
    assert_eq!(e.tick().await.unwrap().timers_fired, 1);
    assert_eq!(e.tick().await.unwrap().timers_fired, 0, "fires once");
    let t = e.lease(NS, "agent/g", "w", TTL).await.unwrap().unwrap();

    // other namespaces can't see or touch the run, even with the right id
    assert_eq!(e.get("other", id).await.unwrap_err().code, ErrorCode::NotFound);
    assert_eq!(e.complete("other", id, &t.task, t.epoch, json!(null)).await.unwrap_err().code, ErrorCode::NotFound);
    assert!(e.lease("other", "agent/g", "w", TTL).await.unwrap().is_none());

    e.cancel(&ctx(), NS, id, "operator").await.unwrap();
    assert_eq!(e.complete(NS, id, &t.task, t.epoch, json!(null)).await.unwrap_err().code, ErrorCode::StaleLease);
    assert_eq!(e.get(NS, id).await.unwrap().status, RunStatus::Cancelled);
    e.verify(NS, id).await.unwrap();
    assert_eq!(s.verify_audit_chain().await.unwrap(), None);
}

/// Many workers racing on one queue: every task is delivered exactly once while leases are live.
#[tokio::test]
async fn concurrent_workers_never_share_a_task() {
    let branches: Vec<String> = (0..20).map(|i| format!("b{i}")).collect();
    let mut yaml = format!("entrypoint: fan\nnodes:\n  fan: {{type: agent, agentRef: p, next: [{}]}}\n", branches.join(", "));
    for b in &branches {
        yaml += &format!("  {b}: {{type: agent, agentRef: w, next: j}}\n");
    }
    yaml += "  j: {type: join}\n";
    let (_pg, _s, e) = setup(&[("fan", &yaml)]).await;
    let id = e.start(&ctx(), NS, "fan", json!(null), None).await.unwrap().run_id;
    let t = e.lease(NS, "agent/p", "w", TTL).await.unwrap().unwrap();
    e.complete(NS, id, &t.task, t.epoch, json!(null)).await.unwrap();

    let workers: Vec<_> = (0..6)
        .map(|w| {
            let e = e.clone();
            tokio::spawn(async move {
                let mut got = vec![];
                while let Some(t) = e.lease(NS, "agent/w", &format!("w{w}"), TTL).await.unwrap() {
                    e.complete(NS, id, &t.task, t.epoch, json!(w)).await.unwrap();
                    got.push(t.task);
                }
                got
            })
        })
        .collect();
    let mut all = vec![];
    for w in workers {
        all.extend(w.await.unwrap());
    }
    assert_eq!(all.len(), 20);
    assert_eq!(all.iter().collect::<BTreeSet<_>>().len(), 20);
    assert_eq!(e.get(NS, id).await.unwrap().status, RunStatus::Succeeded);
    e.verify(NS, id).await.unwrap();
}

#[tokio::test]
async fn invalid_workflow_is_rejected_at_start_with_field_path() {
    let (_pg, _s, e) = setup(&[("bad", "entrypoint: a\nnodes:\n  a: {type: agent, agentRef: x, next: ghost}")]).await;
    let err = e.start(&ctx(), NS, "bad", json!(null), None).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidSpec);
    assert_eq!(err.field.as_deref(), Some("spec.nodes.a.next[0]"));
}

/// CL-22-07/08, E2E-03: an unsafe task whose worker vanishes is not blindly redelivered.
#[tokio::test]
async fn unsafe_task_with_lapsed_lease_waits_for_resolution() {
    let wf = "entrypoint: pay\nnodes:\n  pay: {type: activity, toolRef: payments, sideEffects: unsafe, next: done}\n  done: {type: agent, agentRef: d}";
    let (_pg, s, e) = setup(&[("p", wf)]).await;
    let expire = || sqlx::query("update tasks set lease_expires = now() - interval '1 second' where state = 'leased'").execute(s.pool());

    // 1) operator resolves "retry": a fresh attempt is scheduled; same idempotency key
    let id = e.start(&ctx(), NS, "p", json!(null), None).await.unwrap().run_id;
    let t1 = e.lease(NS, "tool/payments", "w", TTL).await.unwrap().unwrap();
    expire().await.unwrap();
    let r = e.tick().await.unwrap();
    assert_eq!((r.leases_expired, r.outcomes_unknown), (0, 1));
    assert!(e.lease(NS, "tool/payments", "w", TTL).await.unwrap().is_none(), "not redelivered");
    let run = e.get(NS, id).await.unwrap();
    assert!(run.state.unknown.contains_key("pay#1.1") && run.state.waits.contains_key("resolve.pay#1.1"));
    e.signal(&ctx(), NS, id, "resolve.pay#1.1", json!({"outcome": "retry"})).await.unwrap();
    let t2 = e.lease(NS, "tool/payments", "w", TTL).await.unwrap().unwrap();
    assert_eq!((t2.task.as_str(), t2.idempotency_key.as_str()), ("pay#1.2", t1.idempotency_key.as_str()));
    e.complete(NS, id, &t2.task, t2.epoch, json!({"charged": true})).await.unwrap();
    assert_eq!(e.get(NS, id).await.unwrap().state.data["pay"]["charged"], true);
    e.verify(NS, id).await.unwrap();

    // 2) the original worker reports back late: its answer settles it, no operator needed
    let id = e.start(&ctx(), NS, "p", json!(null), None).await.unwrap().run_id;
    let t = e.lease(NS, "tool/payments", "w", TTL).await.unwrap().unwrap();
    expire().await.unwrap();
    e.tick().await.unwrap();
    e.complete(NS, id, &t.task, t.epoch, json!({"charged": "late"})).await.unwrap();
    let run = e.get(NS, id).await.unwrap();
    assert!(run.state.unknown.is_empty());
    assert_eq!(run.state.data["pay"]["charged"], "late");
    assert_eq!(e.lease(NS, "agent/d", "w", TTL).await.unwrap().unwrap().task, "done#1.1");
    e.verify(NS, id).await.unwrap();

    // 3) resolved as failed: run fails with the operator's reason
    let id = e.start(&ctx(), NS, "p", json!(null), None).await.unwrap().run_id;
    e.lease(NS, "tool/payments", "w", TTL).await.unwrap().unwrap();
    expire().await.unwrap();
    e.tick().await.unwrap();
    e.signal(&ctx(), NS, id, "resolve.pay#1.1", json!({"outcome": "failed", "error": "bank says no charge"})).await.unwrap();
    let run = e.get(NS, id).await.unwrap();
    assert_eq!(run.status, RunStatus::Failed);
    assert!(run.state.error.unwrap().contains("bank says no charge"));
    e.verify(NS, id).await.unwrap();
}
