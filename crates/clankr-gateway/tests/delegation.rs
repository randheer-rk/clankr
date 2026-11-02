//! Container tools delegated to the caller's node (ADR 0028): the gateway does every check and
//! records the call; only the node it was handed to may report it, once; late or missing reports
//! settle like timeouts.

use clankr_gateway::Gateway;
use clankr_gateway::tools::{Outcome, Placement, ToolCall, Work};
use clankr_store::Store;
use clankr_store::embedded::DevPostgres;
use clankr_store::secrets::MasterKey;
use clankr_types::{ErrorCode, RequestCtx, Resource};
use serde_json::{Value, json};

const NS: &str = "dlg";

fn ctx(actor: &str) -> RequestCtx {
    RequestCtx { actor: actor.into(), request_id: "t".into() }
}

fn res(kind: &str, name: &str, spec: Value) -> Resource {
    serde_json::from_value(json!({"apiVersion": "clankr.io/v1alpha1", "kind": kind, "metadata": {"name": name, "namespace": NS}, "spec": spec})).unwrap()
}

fn call(method: &str, key: &str) -> ToolCall {
    serde_json::from_value(json!({"tool": "sh", "method": method, "input": {"cmd": "ls"}, "idempotencyKey": key})).unwrap()
}

#[tokio::test]
async fn delegated_calls_are_checked_here_and_settled_once() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let pg = DevPostgres::start(None).await.unwrap();
    let s = Store::connect(&pg.database("clankr").await.unwrap()).await.unwrap();
    let image = format!("r.example/sh@sha256:{}", "a".repeat(64));
    for r in [
        res("Tool", "sh", json!({"version": "1", "timeoutSeconds": 5, "executor": {"type": "sandbox", "container": {"image": image}},
            "methods": {"run": {"sideEffects": "unsafe"}, "peek": {"sideEffects": "readOnly"}}})),
        res("ToolClaim", "c", json!({"allow": [{"tool": "sh"}]})),
    ] {
        s.create(&ctx("owner"), r).await.unwrap();
    }
    let gw = Gateway::new(s.clone(), MasterKey::generate());
    let node = ctx("node:n1/agent/a/i1");

    let Outcome::Delegated(d) = gw.call_tool_at(&node, NS, "c", call("run", "k1"), Placement::Caller).await.unwrap() else { panic!("container tools go to the node") };
    let Work::Sandbox { request, .. } = &d.work else { panic!() };
    assert_eq!((request["method"].as_str(), request["input"]["cmd"].as_str(), d.timeout_seconds), (Some("run"), Some("ls"), 5));

    let other = gw.complete_delegated(&ctx("node:n2/agent/a/i9"), NS, d.call_id, Ok(json!({})), 1).await.unwrap_err();
    assert_eq!(other.code, ErrorCode::Forbidden, "only the node it was handed to reports it");
    let r = gw.complete_delegated(&node, NS, d.call_id, Ok(json!({"files": ["a"]})), 42).await.unwrap();
    assert_eq!((r.output.clone(), r.duration_ms, r.replayed), (json!({"files": ["a"]}), 42, false));
    assert_eq!(gw.complete_delegated(&node, NS, d.call_id, Ok(json!({})), 1).await.unwrap_err().code, ErrorCode::Conflict, "once");
    let Outcome::Done(again) = gw.call_tool_at(&node, NS, "c", call("run", "k1"), Placement::Caller).await.unwrap() else { panic!("a known key replays, never re-delegates") };
    assert!(again.replayed && again.output == r.output);

    // a failure frees the key for a retry, like a local failure
    let Outcome::Delegated(d) = gw.call_tool_at(&node, NS, "c", call("run", "k2"), Placement::Caller).await.unwrap() else { panic!() };
    assert!(gw.complete_delegated(&node, NS, d.call_id, Err("exit 3".into()), 5).await.unwrap_err().message.contains("exit 3"));
    assert!(matches!(gw.call_tool_at(&node, NS, "c", call("run", "k2"), Placement::Caller).await.unwrap(), Outcome::Delegated(_)));

    // too late: an unsafe call becomes OutcomeUnknown; a vanished node's calls get swept
    let Outcome::Delegated(late) = gw.call_tool_at(&node, NS, "c", call("run", "k3"), Placement::Caller).await.unwrap() else { panic!() };
    sqlx::query("update tool_calls set delegated_at = now() - interval '1 minute' where id = $1").bind(late.call_id).execute(s.pool()).await.unwrap();
    assert_eq!(gw.complete_delegated(&node, NS, late.call_id, Ok(json!({})), 1).await.unwrap_err().code, ErrorCode::OutcomeUnknown);
    let Outcome::Delegated(lost_unsafe) = gw.call_tool_at(&node, NS, "c", call("run", "k4"), Placement::Caller).await.unwrap() else { panic!() };
    let Outcome::Delegated(lost_safe) = gw.call_tool_at(&node, NS, "c", call("peek", "k5"), Placement::Caller).await.unwrap() else { panic!() };
    sqlx::query("update tool_calls set delegated_at = now() - interval '5 minutes' where id = any($1)").bind(vec![lost_unsafe.call_id, lost_safe.call_id]).execute(s.pool()).await.unwrap();
    assert_eq!(gw.expire_delegated().await.unwrap(), 2, "k4 and k5 are stale; k2's retry is fresh and k3 is settled");
    let state = |k: &'static str| {
        let s = s.clone();
        async move { sqlx::query_scalar::<_, String>("select state from tool_calls where idempotency_key = $1").bind(k).fetch_optional(s.pool()).await.unwrap() }
    };
    assert_eq!(state("k4").await.as_deref(), Some("unknown"));
    assert_eq!(state("k5").await, None, "a read-only call is freed for a retry");

    // Placement::Here still runs locally (here: no sandbox on this gateway, so it says so)
    let e = gw.call_tool_at(&node, NS, "c", call("peek", "k6"), Placement::Here).await.err().unwrap();
    assert!(e.message.contains("--tool-kennel"), "{e:?}");
}
