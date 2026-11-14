//! Node daemon + Python SDK against a real control plane: handshake, delivery, brokered model
//! and context calls, capability denial, credential isolation, crash redelivery.

use std::path::{Path, PathBuf};
use std::time::Duration;

use clankr_node::client::Client;
use clankr_node::daemon::{Config, run_instance};
use clankr_store::Store;
use clankr_store::embedded::DevPostgres;
use clankr_types::{RequestCtx, Resource};
use serde_json::{Value, json};

const TOKEN: &str = "node-test-token";
const NS: &str = "research";

struct Env {
    _pg: DevPostgres,
    store: Store,
    base: String,
    tmp: tempfile::TempDir,
}

fn ctx() -> RequestCtx {
    RequestCtx { actor: "owner".into(), request_id: "e2e".into() }
}

fn sdk_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../sdk/python").canonicalize().unwrap()
}

async fn env() -> Env {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let pg = DevPostgres::start(None).await.unwrap();
    let store = Store::connect(&pg.database("clankr").await.unwrap()).await.unwrap();
    let state = clankrd::AppState::new(store.clone(), TOKEN.into(), clankr_store::secrets::MasterKey::generate()).await.unwrap();
    clankrd::spawn_ticker(state.engine.clone());
    let app = clankrd::router(state);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    Env { _pg: pg, store, base, tmp: tempfile::tempdir().unwrap() }
}

impl Env {
    async fn apply(&self, kind: &str, name: &str, spec: Value) {
        let r: Resource = serde_json::from_value(json!({"apiVersion": "clankr.io/v1alpha1", "kind": kind, "metadata": {"name": name, "namespace": NS}, "spec": spec})).unwrap();
        self.store.create(&ctx(), r).await.unwrap();
    }

    /// Writes `script` and registers an Agent that runs it with the SDK on PYTHONPATH.
    async fn agent(&self, name: &str, script: &str, extra: Value) {
        let path = self.tmp.path().join(format!("{name}.py"));
        std::fs::write(&path, script).unwrap();
        let mut spec = json!({
            "runtime": {"type": "process", "command": ["python3", path], "env": {"PYTHONPATH": sdk_path(), "CLANKR_TOKEN": "spec-tries-to-inject"}},
            "taskTtlSeconds": 5,
        });
        spec.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        self.apply("Agent", name, spec).await;
    }

    fn node(&self, agent: &str) -> tokio::sync::watch::Sender<bool> {
        let (tx, rx) = tokio::sync::watch::channel(false);
        let cfg = Config {
            agent: agent.into(),
            namespace: NS.into(),
            work_dir: self.tmp.path().join("node"),
            instance: format!("{agent}-i1"),
            kennel: "kennel".into(),
            kennel_root: None,
            ready: None,
            spec: None,
            hibernate: None,
            checkpoint: None,
        };
        tokio::spawn(run_instance(Client::new(&self.base, TOKEN, NS), cfg, rx));
        tx
    }

    async fn wait_terminal(&self, run_id: uuid::Uuid) -> clankr_engine::RunView {
        let engine = clankr_engine::Engine::new(self.store.clone());
        for _ in 0..300 {
            let r = engine.get(NS, run_id).await.unwrap();
            if r.status.is_terminal() {
                engine.verify(NS, run_id).await.unwrap();
                return r;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("run {run_id} did not finish");
    }

    async fn start(&self, workflow: &str, input: Value) -> uuid::Uuid {
        clankr_engine::Engine::new(self.store.clone()).start(&ctx(), NS, workflow, input, None).await.unwrap().run_id
    }
}

const RESEARCHER: &str = r#"
import os
from clankr import AgentApp, ClankrError

app = AgentApp()

@app.task("research")
async def research(task, ctx):
    mem = ctx.context("memory")
    prev = await mem.get("progress")
    reply = await ctx.models.generate("default", [{"role": "user", "content": task.input["input"]["topic"]}])
    saved = await mem.put("progress", {"summary": reply["text"]}, if_version=prev["version"] if prev else 0)
    try:
        await ctx.models.generate("not-bound", [{"role": "user", "content": "x"}])
        denied = None
    except ClankrError as e:
        denied = e.code
    return {
        "summary": reply["text"],
        "usage": reply["usage"],
        "contextVersion": saved["version"],
        "denied": denied,
        "tokenVisible": os.environ.get("CLANKR_TOKEN"),
        "idempotencyKey": task.idempotency_key,
    }

app.run()
"#;

#[tokio::test(flavor = "multi_thread")]
async fn python_agent_runs_a_workflow_through_the_node() {
    let e = env().await;
    e.apply("ProviderConnection", "mock", json!({"provider": "mock", "allowedModels": ["mock-1"]})).await;
    e.apply("ModelClaim", "default", json!({"providerConnection": "mock", "model": "mock-1"})).await;
    e.apply("ModelClaim", "not-bound", json!({"providerConnection": "mock", "model": "mock-1"})).await;
    e.apply("ContextClaim", "memory", json!({})).await;
    e.agent("researcher", RESEARCHER, json!({"models": ["default"], "contexts": ["memory"]})).await;
    e.apply("Workflow", "wf", json!({"entrypoint": "research", "nodes": {"research": {"type": "agent", "agentRef": "researcher"}}})).await;

    let stop = e.node("researcher");
    let id = e.start("wf", json!({"topic": "durable agents"})).await;
    let run = e.wait_terminal(id).await;
    stop.send(true).unwrap();

    assert_eq!(run.status, clankr_engine::RunStatus::Succeeded, "{:?}", run.state.error);
    let out = &run.state.data["research"];
    assert_eq!(out["summary"], "mock(mock-1): durable agents");
    assert_eq!(out["contextVersion"], 1);
    assert_eq!(out["denied"], "Forbidden", "node refuses claims the Agent spec doesn't bind");
    assert_eq!(out["tokenVisible"], Value::Null, "node credential never reaches the agent");
    assert_eq!(out["idempotencyKey"], format!("{id}/research#1"));

    let mem = e.store.context_get(NS, "memory", "progress").await.unwrap();
    assert_eq!(mem.writer, "owner/agent/researcher/researcher-i1");
    let (run_attr, task_attr): (Option<uuid::Uuid>, Option<String>) =
        sqlx::query_as("select run_id, task from model_calls where outcome = 'ok'").fetch_one(e.store.pool()).await.unwrap();
    assert_eq!((run_attr, task_attr.as_deref()), (Some(id), Some("research#1.1")), "usage attributed to run and task");
}

#[tokio::test(flavor = "multi_thread")]
async fn incompatible_abi_is_rejected_at_handshake() {
    let e = env().await;
    let out = e.tmp.path().join("rejected.txt");
    let script = format!(
        r#"
import asyncio, json, os, struct
async def main():
    r, w = await asyncio.open_unix_connection(os.environ["CLANKR_SOCKET"])
    body = json.dumps({{"type": "hello", "abiMajor": 2, "abiMinor": 0, "sdk": "future/9"}}).encode()
    w.write(struct.pack(">I", len(body)) + body); await w.drain()
    (n,) = struct.unpack(">I", await r.readexactly(4))
    open({out:?}, "w").write((await r.readexactly(n)).decode())
asyncio.run(main())
"#
    );
    e.agent("future", &script, json!({})).await;
    let stop = e.node("future");
    for _ in 0..100 {
        if let Ok(s) = std::fs::read_to_string(&out) {
            let f: Value = serde_json::from_str(&s).unwrap();
            assert_eq!(f["type"], "reject");
            assert!(f["reason"].as_str().unwrap().contains("incompatible"));
            stop.send(true).unwrap();
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("agent never saw a Reject");
}

/// Worker-failure recovery with a real agent: the process dies mid-task, the lease lapses,
/// the engine redelivers, the restarted agent finishes the run.
#[tokio::test(flavor = "multi_thread")]
async fn agent_crash_mid_task_is_redelivered() {
    let e = env().await;
    let script = r#"
import os
from clankr import AgentApp
app = AgentApp()
@app.task()
async def work(task, ctx):
    if task.attempt == 1 and not os.path.exists(os.environ["MARK"]):
        open(os.environ["MARK"], "w").close()
        os._exit(1)          # crash with the task in flight
    return {"done": True}
app.run()
"#;
    let mark = e.tmp.path().join("crashed-once");
    e.agent("flaky", script, json!({})).await;
    let mut a = e.store.get(NS, "Agent", "flaky").await.unwrap();
    a.spec["runtime"]["env"]["MARK"] = json!(mark);
    e.store.update(&ctx(), a).await.unwrap();
    e.apply("Workflow", "wf", json!({"entrypoint": "work", "nodes": {"work": {"type": "agent", "agentRef": "flaky"}}})).await;

    let stop = e.node("flaky");
    let id = e.start("wf", json!(null)).await;
    let run = e.wait_terminal(id).await;
    stop.send(true).unwrap();
    assert!(mark.exists(), "first delivery crashed the agent");
    assert_eq!(run.status, clankr_engine::RunStatus::Succeeded);
    let deliveries: i32 = sqlx::query_scalar("select deliveries from tasks").fetch_one(e.store.pool()).await.unwrap();
    assert_eq!(deliveries, 2, "same task delivered again after the lease lapsed");
}

const PRODUCER: &str = r#"
from clankr import AgentApp, ClankrError
app = AgentApp()
@app.task("produce")
async def produce(task, ctx):
    box = ctx.mailbox("jobs")
    sent = []
    for i in range(3):
        r = await box.send({"job": i}, correlation_id=task.run_id, dedup_key=f"{task.idempotency_key}/{i}")
        sent.append(r["duplicate"])
    retry = await box.send({"job": 0}, dedup_key=f"{task.idempotency_key}/0")   # a retried send
    try:
        await ctx.mailbox("secret-box").send({"x": 1})
        denied = None
    except ClankrError as e:
        denied = e.code
    return {"duplicates": sent, "retryDuplicate": retry["duplicate"], "denied": denied}
app.run()
"#;

const CONSUMER: &str = r#"
from clankr import AgentApp
app = AgentApp()
@app.task("consume")
async def consume(task, ctx):
    got = []
    while len(got) < 3:
        for m in await ctx.mailbox("jobs").receive(max=2, wait_seconds=5):
            got.append({"job": m.payload["job"], "sender": m.sender, "corr": m.correlation_id})
            await m.ack()
    return {"got": got}
app.run()
"#;

#[tokio::test(flavor = "multi_thread")]
async fn agents_exchange_messages_through_a_durable_mailbox() {
    let e = env().await;
    e.apply("AgentMailbox", "jobs", json!({"maxDeliveries": 3})).await;
    e.apply("AgentMailbox", "secret-box", json!({})).await;
    e.agent("producer", PRODUCER, json!({"mailboxes": ["jobs"]})).await;
    e.agent("consumer", CONSUMER, json!({"mailboxes": ["jobs"]})).await;
    e.apply("Workflow", "pipe", json!({"entrypoint": "produce", "nodes": {
        "produce": {"type": "agent", "agentRef": "producer", "next": "consume"},
        "consume": {"type": "agent", "agentRef": "consumer"}}})).await;
    let p = e.node("producer");
    let c = e.node("consumer");
    let id = e.start("pipe", json!(null)).await;
    let run = e.wait_terminal(id).await;
    p.send(true).unwrap();
    c.send(true).unwrap();

    assert_eq!(run.status, clankr_engine::RunStatus::Succeeded, "{:?}", run.state.error);
    let prod = &run.state.data["produce"];
    assert_eq!(prod["duplicates"], json!([false, false, false]));
    assert_eq!(prod["retryDuplicate"], true, "a retried send with the same dedup key isn't delivered twice");
    assert_eq!(prod["denied"], "Forbidden", "mailboxes not in the Agent spec are refused");
    let got = run.state.data["consume"]["got"].as_array().unwrap().clone();
    assert_eq!(got.iter().map(|g| g["job"].as_i64().unwrap()).collect::<Vec<_>>(), [0, 1, 2], "in order, exactly once");
    assert!(got.iter().all(|g| g["corr"] == id.to_string()));
    assert!(got[0]["sender"].as_str().unwrap().ends_with("agent/producer/producer-i1"), "sender comes from the authenticated principal: {}", got[0]["sender"]);
    let left = e.store.mailbox_list(NS, "jobs", None, 10).await.unwrap();
    assert!(left.iter().all(|m| m.state == "acked"));
}

/// Tools through the SDK (CL-28-06): the default idempotency key is stable across attempts, so a
/// task that crashes after a tool call gets the recorded result on retry instead of repeating
/// the effect; a claim the Agent isn't bound to is refused by the node.
#[tokio::test(flavor = "multi_thread")]
async fn tool_calls_replay_across_task_retries() {
    let e = env().await;
    e.apply("Tool", "lookup", json!({"version": "1", "executor": {"type": "mock", "responses": {"query": {"answer": 42}}}, "methods": {"query": {}}})).await;
    e.apply("ToolClaim", "search", json!({"allow": [{"tool": "lookup"}]})).await;
    e.apply("ToolClaim", "forbidden", json!({"allow": [{"tool": "lookup"}]})).await;
    e.agent(
        "toolish",
        r#"
from clankr import AgentApp, ClankrError
app = AgentApp()
@app.task()
async def work(task, ctx):
    r = await ctx.tools.call("search", "lookup", "query", {"q": task.input["input"]["q"]})
    denied = None
    try:
        await ctx.tools.call("forbidden", "lookup", "query", {})
    except ClankrError as err:
        denied = err.code
    if task.attempt == 1:
        raise RuntimeError("crashed after the tool call")
    return {"output": r["output"], "replayed": r["replayed"], "denied": denied}
app.run()
"#,
        json!({"tools": ["search"]}),
    )
    .await;
    e.apply("Workflow", "w", json!({"entrypoint": "work", "nodes": {"work": {"type": "agent", "agentRef": "toolish"}}})).await;
    let stop = e.node("toolish");
    let engine = clankr_engine::Engine::new(e.store.clone());
    let run = engine.start(&ctx(), NS, "w", json!({"q": "life"}), None).await.unwrap().run_id;
    let v = e.wait_terminal(run).await;
    stop.send(true).unwrap();
    assert_eq!(v.status, clankr_engine::RunStatus::Succeeded, "{:?}", v.state.error);
    assert_eq!(v.state.data["work"], json!({"output": {"answer": 42}, "replayed": true, "denied": "Forbidden"}));
    let calls: Vec<(String, Option<uuid::Uuid>)> = sqlx::query_as("select idempotency_key, run_id from tool_calls").fetch_all(e.store.pool()).await.unwrap();
    assert_eq!(calls.len(), 1, "executed once across two attempts: {calls:?}");
    assert_eq!(calls[0].1, Some(run), "attributed to the run");
}
