//! Agent messaging (PRD §30) end to end on a fleet node: an AgentService answered by a push
//! handler, request/reply with a timeout and a late reply that never resurfaces, topic fan-out
//! into durable subscriptions, and authorization on every send.

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use clankr_node::fleet::{FleetConfig, run_fleet};
use clankr_types::{RequestCtx, Resource};
use serde_json::{Value, json};

const NS: &str = "msg";

/// Serves "echo"; a payload with `sleep` takes that long first.
const ECHO: &str = r#"
import asyncio
from clankr import AgentApp, Permanent
app = AgentApp()
@app.service("echo")
async def echo(msg, ctx):
    if msg.payload.get("bad"):
        raise Permanent("bad request")
    await asyncio.sleep(msg.payload.get("sleep", 0))
    return {"echo": msg.payload, "by": ctx.agent, "sender": msg.sender}
app.run()
"#;

/// The same service in TypeScript (SDK path substituted at runtime).
const ECHO_TS: &str = r#"
import { AgentApp, Permanent } from "SDK_TS"
new AgentApp()
  .service("echo-ts", async (msg, ctx) => {
    if (msg.payload.bad) throw new Permanent("bad request")
    return { echo: msg.payload, by: ctx.agent, sender: msg.sender }
  })
  .run()
"#;

/// Calls the service and publishes, per workflow node.
const CALLER: &str = r#"
from clankr import AgentApp, ClankrError
app = AgentApp()
@app.task("call")
async def call(task, ctx):
    return await ctx.services.call("echo", {"n": 1})
@app.task("call-ts")
async def call_ts(task, ctx):
    return {"ok": await ctx.services.call("echo-ts", {"n": 3}), "bad": await ctx.services.call("echo-ts", {"bad": True})}
@app.task("slow")
async def slow(task, ctx):
    try:
        await ctx.services.call("echo", {"sleep": 3}, timeout_seconds=1)
        return {"timedOut": False}
    except ClankrError as e:
        after = await ctx.services.call("echo", {"n": 2})
        return {"timedOut": e.code, "after": after}
@app.task("bad")
async def bad(task, ctx):
    return await ctx.services.call("echo", {"bad": True})
@app.task("publish")
async def publish(task, ctx):
    return await ctx.publish("orders.created", {"order": 42}, dedup_key="order-42")
@app.task("unlisted")
async def unlisted(task, ctx):
    out = {}
    for name, fn in (("service", lambda: ctx.services.call("secret", {})), ("topic", lambda: ctx.publish("billing.paid", {}))):
        try:
            await fn()
            out[name] = "allowed"
        except ClankrError as e:
            out[name] = e.code
    return out
app.run()
"#;

/// Records every message from its subscribed mailbox into context.
const SUB: &str = r#"
from clankr import AgentApp
app = AgentApp()
@app.subscribe("orders")
async def on_order(msg, ctx):
    await ctx.context("seen").put(f"order-{msg.payload['order']}", {"sender": msg.sender})
app.run()
"#;

fn ctx() -> RequestCtx {
    RequestCtx { actor: "owner".into(), request_id: "msg".into() }
}

fn res(kind: &str, name: &str, spec: Value) -> Resource {
    serde_json::from_value(json!({"apiVersion": "clankr.io/v1alpha1", "kind": kind, "metadata": {"name": name, "namespace": NS}, "spec": spec})).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn services_request_reply_and_topics() {
    let _ = tracing_subscriber::fmt().with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "warn".into())).try_init();
    let cp = common::start("owner", "join", Some(Duration::from_millis(300))).await;
    let s = cp.store.clone();
    let tmp = tempfile::tempdir().unwrap();
    let sdk = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../sdk/python").canonicalize().unwrap();
    let agent = |name: &str, code: &str, extra: Value| {
        let script = tmp.path().join(format!("{name}.py"));
        std::fs::write(&script, code).unwrap();
        let mut spec = json!({"runtime": {"type": "process", "command": ["python3", script], "env": {"PYTHONPATH": sdk}}, "taskTtlSeconds": 30, "resources": {"memoryMiB": 256, "cpus": 0.25}});
        spec.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        res("Agent", name, spec)
    };
    let nodes = |names: &[&str]| json!({"entrypoint": names[0], "nodes": names.iter().map(|n| (n.to_string(), json!({"type": "agent", "agentRef": "caller"}))).collect::<serde_json::Map<_, _>>()});
    for r in [
        agent("echo", ECHO, json!({})),
        agent("caller", CALLER, json!({"calls": ["echo", "echo-ts"], "publishes": ["orders.created"]})),
        {
            let script = tmp.path().join("echo.ts");
            let sdk_ts = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../sdk/typescript/src/index.ts").canonicalize().unwrap();
            std::fs::write(&script, ECHO_TS.replace("SDK_TS", sdk_ts.to_str().unwrap())).unwrap();
            res("Agent", "echo-ts", json!({"runtime": {"type": "process", "command": ["node", script]}, "taskTtlSeconds": 30, "resources": {"memoryMiB": 256, "cpus": 0.25}}))
        },
        res("AgentService", "echo-ts", json!({"agent": "echo-ts"})),
        agent("sub", SUB, json!({"mailboxes": ["orders"], "contexts": ["seen"]})),
        res("AgentService", "echo", json!({"agent": "echo", "retentionSeconds": 2})),
        res("AgentMailbox", "orders", json!({})),
        res("AgentMailbox", "orders-audit", json!({})),
        res("EventSubscription", "orders-to-sub", json!({"topic": "orders.created", "mailbox": "orders"})),
        res("EventSubscription", "orders-to-audit", json!({"topic": "orders.created", "mailbox": "orders-audit"})),
        res("ContextClaim", "seen", json!({"capabilities": ["keyValue"]})),
    ] {
        s.create(&ctx(), r).await.unwrap();
    }
    for (name, entry) in [("call", "call"), ("call-ts", "call-ts"), ("slow", "slow"), ("bad", "bad"), ("publish", "publish"), ("unlisted", "unlisted")] {
        s.create(&ctx(), res("Workflow", name, nodes(&[entry]))).await.unwrap();
    }
    for d in ["echo", "echo-ts", "caller", "sub"] {
        s.create(&ctx(), res("AgentDeployment", d, json!({"agent": d, "replicas": 1}))).await.unwrap();
    }
    let (stop, rx) = tokio::sync::watch::channel(false);
    let node = tokio::spawn(run_fleet(
        FleetConfig {
            server: cp.base.clone(),
            ca_pem: cp.ca_pem.clone(),
            name: "node-a".into(),
            join_token: "join".into(),
            capacity: json!({"memoryMiB": 2048, "cpus": 4.0, "runtimes": ["process"]}),
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

    let engine = clankr_engine::Engine::new(s.clone());
    let run = |wf: &'static str| {
        let engine = engine.clone();
        async move {
            let id = engine.start(&ctx(), NS, wf, json!({}), None).await.unwrap().run_id;
            let end = Instant::now() + Duration::from_secs(60);
            loop {
                let v = engine.get(NS, id).await.unwrap();
                if v.status.is_terminal() {
                    assert_eq!(v.status, clankr_engine::RunStatus::Succeeded, "{wf}: {:?}", v.state.error);
                    return v.state.data[wf].clone();
                }
                assert!(Instant::now() < end, "{wf} didn't finish");
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    };

    // request/reply by name; the reply says who served it and who asked (authenticated)
    let r = run("call").await;
    assert_eq!(r, json!({"echo": {"n": 1}, "by": "echo", "sender": "agent/caller"}), "{r}");

    // the same through the TypeScript SDK's push handler, including the error reply
    let r = run("call-ts").await;
    assert_eq!(r, json!({"ok": {"echo": {"n": 3}, "by": "echo-ts", "sender": "agent/caller"}, "bad": {"error": "bad request"}}), "{r}");

    // a call that times out reports OutcomeUnknown; the late reply never reaches a later call
    let r = run("slow").await;
    assert_eq!(r["timedOut"], "OutcomeUnknown", "{r}");
    assert_eq!(r["after"]["echo"], json!({"n": 2}), "the next call gets its own reply: {r}");

    // a Permanent failure in the handler comes back as an error reply, not a timeout
    assert_eq!(run("bad").await, json!({"error": "bad request"}));

    // only what the spec lists
    assert_eq!(run("unlisted").await, json!({"service": "Forbidden", "topic": "Forbidden"}));

    // a topic fans out into every subscribed mailbox; the subscriber's push handler sees it
    let r = run("publish").await;
    assert_eq!(r["delivered"], 2, "{r}");
    let seen = || {
        let s = s.clone();
        async move { s.context_get(NS, "seen", "order-42").await.ok().map(|o| o.value) }
    };
    let end = Instant::now() + Duration::from_secs(20);
    while seen().await.is_none() {
        assert!(Instant::now() < end, "subscriber never handled the event: {:?}", s.mailbox_list(NS, "orders", None, 10).await.unwrap().iter().map(|m| (&m.state, m.deliveries, &m.dead_reason)).collect::<Vec<_>>());
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert_eq!(seen().await.unwrap(), json!({"sender": "agent/caller"}));
    // the other subscription keeps its copy until someone reads it (durable catch-up)
    assert_eq!(s.mailbox_list(NS, "orders-audit", Some("ready"), 10).await.unwrap().len(), 1);
    // and a republish with the same dedup key doesn't deliver twice
    run("publish").await;
    assert_eq!(s.mailbox_list(NS, "orders-audit", None, 10).await.unwrap().len(), 1);

    // the timed-out call's late reply sits unread, then is dead-lettered at the service's retention
    let end = Instant::now() + Duration::from_secs(20);
    loop {
        let dead: i64 = sqlx::query_scalar("select count(*) from mailbox_messages where mailbox like 'reply:%' and state = 'dead'").fetch_one(s.pool()).await.unwrap();
        if dead == 1 {
            break;
        }
        assert!(Instant::now() < end, "late reply wasn't dead-lettered");
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    stop.send(true).unwrap();
    let _ = node.await;
}
