//! Tool gateway (CL-28): grants, schemas, idempotent replay, approvals spent once, unknown
//! outcomes, budgets, bounded/sanitized output, and brokered credentials.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Json;
use axum::http::HeaderMap;
use axum::routing::post;
use clankr_engine::{Decider, Decision, Engine};
use clankr_gateway::Gateway;
use clankr_gateway::tools::ToolCall;
use clankr_store::Store;
use clankr_store::embedded::DevPostgres;
use clankr_store::secrets::MasterKey;
use clankr_types::{ErrorCode, RequestCtx, Resource};
use serde_json::{Value, json};

const NS: &str = "tools";

fn ctx(actor: &str) -> RequestCtx {
    RequestCtx { actor: actor.into(), request_id: "t".into() }
}

fn res(kind: &str, name: &str, spec: Value) -> Resource {
    serde_json::from_value(json!({"apiVersion": "clankr.io/v1alpha1", "kind": kind, "metadata": {"name": name, "namespace": NS}, "spec": spec})).unwrap()
}

fn call(tool: &str, method: &str, input: Value, key: &str) -> ToolCall {
    serde_json::from_value(json!({"tool": tool, "method": method, "input": input, "idempotencyKey": key})).unwrap()
}

/// A tool server that counts executions, records the auth header, and can be told to stall.
async fn server(stall: Arc<Mutex<bool>>) -> (String, Arc<Mutex<Vec<(String, Value)>>>) {
    let seen = Arc::new(Mutex::new(vec![]));
    let s = seen.clone();
    let app = axum::Router::new().route(
        "/run",
        post(move |h: HeaderMap, Json(b): Json<Value>| {
            let (s, stall) = (s.clone(), stall.clone());
            async move {
                s.lock().unwrap().push((h.get("x-api-key").map(|v| v.to_str().unwrap().to_owned()).unwrap_or_default(), b.clone()));
                if *stall.lock().unwrap() {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
                Json(json!({"published": b["input"]["title"], "noise": "ok\u{1b}[2J\u{7}done"}))
            }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://127.0.0.1:{}/run", l.local_addr().unwrap().port());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (url, seen)
}

#[tokio::test]
async fn tool_gateway_rules() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let pg = DevPostgres::start(None).await.unwrap();
    let s = Store::connect(&pg.database("clankr").await.unwrap()).await.unwrap();
    let master = MasterKey::generate();
    s.secret_put(&ctx("owner"), &master, NS, "cms", &BTreeMap::from([("key".into(), "cms-secret".into())])).await.unwrap();
    let gw = Gateway::new(s.clone(), master);
    let stall = Arc::new(Mutex::new(false));
    let (url, seen) = server(stall.clone()).await;
    let title_schema = json!({"type": "object", "required": ["title"], "additionalProperties": false, "properties": {"title": {"type": "string"}}});
    for r in [
        res("Tool", "cms", json!({"version": "2.1.0", "timeoutSeconds": 1, "maxOutputBytes": 300,
            "executor": {"type": "http", "url": url, "secretRef": {"name": "cms", "key": "key"}, "header": "x-api-key"},
            "methods": {"publish": {"input": title_schema, "risk": "high"}, "draft": {"input": title_schema, "sideEffects": "idempotent"}, "delete": {}}})),
        res("Tool", "search", json!({"version": "1.0.0", "maxOutputBytes": 300, "executor": {"type": "mock", "responses": {"query": {"hits": ["x".repeat(500)]}, "broken": {"$error": "index offline"}}},
            "methods": {"query": {"sideEffects": "readOnly"}, "broken": {"sideEffects": "readOnly"}}})),
        res("ToolClaim", "writer", json!({"allow": [{"tool": "cms", "versions": ["2.1.0"]}, {"tool": "search"}], "deny": [{"tool": "cms", "methods": ["delete"]}], "maxCallsPerRun": 3})),
        res("ToolClaim", "old", json!({"allow": [{"tool": "cms", "versions": ["1.0.0"]}]})),
        res("Workflow", "ship", json!({"entrypoint": "review", "nodes": {"review": {"type": "approval", "approverGroup": "eds", "next": "go"}, "go": {"type": "agent", "agentRef": "x"}}})),
    ] {
        s.create(&ctx("owner"), r).await.unwrap();
    }
    let code = |r: Result<_, clankr_types::ApiError>| r.map(|_| ()).unwrap_err().code;
    let agent = ctx("node:n1/agent");

    // grants: deny wins, unlisted methods and versions refused, discovery isn't permission
    assert_eq!(code(gw.call_tool(&agent, NS, "writer", call("cms", "delete", json!({}), "k0")).await), ErrorCode::Forbidden);
    assert_eq!(code(gw.call_tool(&agent, NS, "old", call("cms", "draft", json!({"title": "a"}), "k0")).await), ErrorCode::Forbidden);
    assert_eq!(code(gw.call_tool(&agent, NS, "writer", call("files", "read", json!({}), "k0")).await), ErrorCode::Forbidden);
    // schemas: undeclared inputs fail before anything runs
    let e = gw.call_tool(&agent, NS, "writer", call("cms", "draft", json!({"title": "a", "html": "<script>"}), "k1")).await.unwrap_err();
    assert!(e.message.contains("$.input.html: not allowed"), "{}", e.message);
    assert!(seen.lock().unwrap().is_empty());

    // idempotent replay: same key returns the recorded result without executing again
    let r1 = gw.call_tool(&agent, NS, "writer", call("cms", "draft", json!({"title": "hello"}), "k2")).await.unwrap();
    let r2 = gw.call_tool(&agent, NS, "writer", call("cms", "draft", json!({"title": "hello"}), "k2")).await.unwrap();
    assert_eq!((r1.replayed, r2.replayed, &r1.output, &r1.output_sha256), (false, true, &r2.output, &r2.output_sha256));
    assert_eq!(seen.lock().unwrap().len(), 1, "executed once");
    assert_eq!(seen.lock().unwrap()[0].0, "cms-secret", "credential brokered to the tool, not the agent");
    assert_eq!(r1.output["noise"], "ok[2Jdone", "terminal control characters stripped");
    assert_eq!(code(gw.call_tool(&agent, NS, "writer", call("cms", "draft", json!({"title": "other"}), "k2")).await), ErrorCode::Conflict, "key reuse for a different call");

    // high risk: no run, or a run without a granted approval -> refused
    assert_eq!(code(gw.call_tool(&agent, NS, "writer", call("cms", "publish", json!({"title": "p"}), "k3")).await), ErrorCode::Forbidden);
    let engine = Engine::new(s.clone());
    let run = engine.start(&ctx("serviceaccount:tools/ci"), NS, "ship", json!({}), None).await.unwrap().run_id;
    let in_run = |method: &str, key: &str| {
        let mut c = call("cms", method, json!({"title": "launch"}), key);
        c.run_id = Some(run);
        c
    };
    assert_eq!(code(gw.call_tool(&agent, NS, "writer", in_run("publish", "k4")).await), ErrorCode::Forbidden, "approval still pending");
    let a = &engine.approvals(NS, Some("pending"), 5).await.unwrap()[0];
    let ed = ctx("user:ed");
    let groups = vec!["eds".to_owned()];
    engine.decide_approval(&Decider { ctx: &ed, groups: &groups, break_glass: false }, NS, a.id, Decision::Approve, &a.digest, "ok").await.unwrap();
    let p = gw.call_tool(&agent, NS, "writer", in_run("publish", "k5")).await.unwrap();
    assert_eq!(p.approval, Some(a.id), "the run's approval was spent");
    let e = gw.call_tool(&agent, NS, "writer", in_run("publish", "k6")).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::Forbidden, "one approval, one high-risk call");
    let audit = s.audit_since(0, 1000).await.unwrap();
    assert!(audit.iter().any(|e| e.action == "tool cms.publish v2.1.0 done" && e.reason.as_deref() == Some(&format!("approval {}", a.id))), "high-risk calls are audited with their approval");

    // per-run budget: k5 plus failed k4/k6 (failed calls free their slot) -> 1 used of 3
    for (i, key) in ["k7", "k8"].iter().enumerate() {
        assert!(gw.call_tool(&agent, NS, "writer", in_run("draft", key)).await.is_ok(), "call {i}");
    }
    assert_eq!(code(gw.call_tool(&agent, NS, "writer", in_run("draft", "k9")).await), ErrorCode::QuotaExceeded);

    // an unsafe call that times out is "unknown": never silently re-run with the same key
    *stall.lock().unwrap() = true;
    // draft is idempotent: a timeout is a plain failure and the key can be retried
    assert_eq!(code(gw.call_tool(&agent, NS, "writer", call("cms", "draft", json!({"title": "slow"}), "k10")).await), ErrorCode::ProviderUnavailable);
    s.create(&ctx("owner"), res("Tool", "pay", json!({"version": "1", "timeoutSeconds": 1, "executor": {"type": "http", "url": url}, "methods": {"charge": {}}}))).await.unwrap();
    s.create(&ctx("owner"), res("ToolClaim", "payments", json!({"allow": [{"tool": "pay"}]}))).await.unwrap();
    assert_eq!(code(gw.call_tool(&agent, NS, "payments", call("pay", "charge", json!({}), "c1")).await), ErrorCode::OutcomeUnknown);
    assert_eq!(code(gw.call_tool(&agent, NS, "payments", call("pay", "charge", json!({}), "c1")).await), ErrorCode::OutcomeUnknown, "the retry is told, not re-charged");
    *stall.lock().unwrap() = false;

    // bounded output and mock executor errors
    let big = gw.call_tool(&agent, NS, "writer", call("search", "query", json!({}), "s1")).await.unwrap();
    assert!(big.truncated && big.output["truncated"] == true && big.output["preview"].as_str().unwrap().len() <= 300);
    assert_eq!(code(gw.call_tool(&agent, NS, "writer", call("search", "broken", json!({}), "s2")).await), ErrorCode::ProviderUnavailable);
}
