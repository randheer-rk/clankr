//! Container tools run on the calling agent's node (ADR 0028), not on the control plane: the
//! control plane here has no sandbox at all, so a successful call proves the node ran it.
//! Skips when kennel or the busybox image isn't available.

use std::path::{Path, PathBuf};
use std::time::Duration;

use clankr_node::client::Client;
use clankr_node::daemon::{Config, run_instance};
use clankr_store::Store;
use clankr_store::embedded::DevPostgres;
use clankr_types::{RequestCtx, Resource};
use serde_json::{Value, json};

const TOKEN: &str = "tool-node-token";
const NS: &str = "tn";
const BUSYBOX: &str = "docker.io/library/busybox:1.37";

const AGENT: &str = r#"
from clankr import AgentApp
app = AgentApp()
@app.task()
async def work(task, ctx):
    r = await ctx.tools.call("t", "sh", "run", {"n": 2})
    return {"output": r["output"], "replayed": r["replayed"]}
app.run()
"#;

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_tools_run_on_the_agents_node() {
    run("container").await;
}

/// The same tool in a Firecracker microVM: the request reaches it on stdin over vsock.
#[tokio::test(flavor = "multi_thread")]
async fn sandbox_tools_run_in_a_microvm_too() {
    let kernel = std::env::var_os("KENNEL_VM_KERNEL").map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share/kennel-vmlinuz"))).filter(|k| k.is_file());
    let Some(kernel) = kernel else {
        eprintln!("SKIPPED: no guest kernel (KENNEL_VM_KERNEL or ~/.local/share/kennel-vmlinuz)");
        return;
    };
    unsafe { std::env::set_var("KENNEL_VM_KERNEL", kernel) };
    run("microvm").await;
}

async fn run(class: &str) {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
    let kbin = std::env::var_os("KENNEL").map(PathBuf::from).unwrap_or_else(|| repo.join("../kennel/target/debug/kennel"));
    let kroot = repo.join("target/kennel-test-store");
    let k = |args: &[&str]| std::process::Command::new(&kbin).arg("--root").arg(&kroot).args(args).output().ok().filter(|o| o.status.success());
    let Some(inspect) = kbin.exists().then(|| k(&["inspect", BUSYBOX]).or_else(|| k(&["pull", BUSYBOX]).and_then(|_| k(&["inspect", BUSYBOX])))).flatten() else {
        eprintln!("SKIPPED: kennel or {BUSYBOX} unavailable");
        return;
    };
    let digest = serde_json::from_slice::<Value>(&inspect.stdout).unwrap()["digest"].as_str().unwrap().to_owned();

    let _ = rustls::crypto::ring::default_provider().install_default();
    let pg = DevPostgres::start(None).await.unwrap();
    let store = Store::connect(&pg.database("clankr").await.unwrap()).await.unwrap();
    let state = clankrd::AppState::new(store.clone(), TOKEN.into(), clankr_store::secrets::MasterKey::generate()).await.unwrap();
    clankrd::spawn_ticker(state.engine.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, clankrd::router(state)).await.unwrap() });

    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("agent.py"), AGENT).unwrap();
    let sdk = repo.join("sdk/python");
    let rctx = RequestCtx { actor: "owner".into(), request_id: "tn".into() };
    let res = |kind: &str, name: &str, spec: Value| serde_json::from_value::<Resource>(json!({"apiVersion": "clankr.io/v1alpha1", "kind": kind, "metadata": {"name": name, "namespace": NS}, "spec": spec})).unwrap();
    let script = r#"read -r req; n=$(echo "$req" | sed 's/.*"n":\([0-9]*\).*/\1/'); echo "{\"double\": $((n * 2)), \"kernel\": \"$(uname -s)\"}""#;
    for r in [
        res("Tool", "sh", json!({"version": "1", "timeoutSeconds": 20, "executor": {"type": "sandbox", "container": {"image": format!("docker.io/library/busybox@{digest}"), "command": ["sh", "-c", script], "runtime": class}},
            "methods": {"run": {"sideEffects": "idempotent"}}})),
        res("ToolClaim", "t", json!({"allow": [{"tool": "sh"}]})),
        res("Agent", "a", json!({"runtime": {"type": "process", "command": ["python3", tmp.path().join("agent.py")], "env": {"PYTHONPATH": sdk}}, "tools": ["t"], "taskTtlSeconds": 30})),
        res("Workflow", "wf", json!({"entrypoint": "w", "nodes": {"w": {"type": "agent", "agentRef": "a"}}})),
    ] {
        store.create(&rctx, r).await.unwrap();
    }
    let (stop, rx) = tokio::sync::watch::channel(false);
    let cfg = Config {
        agent: "a".into(),
        namespace: NS.into(),
        work_dir: tmp.path().join("node"),
        instance: "a-i1".into(),
        kennel: kbin.clone(),
        kennel_root: Some(kroot.clone()),
        ready: None,
        spec: None,
        hibernate: None,
        checkpoint: None,
    };
    tokio::spawn(run_instance(Client::new(&base, TOKEN, NS), cfg, rx));

    let engine = clankr_engine::Engine::new(store.clone());
    let id = engine.start(&rctx, NS, "wf", json!({}), None).await.unwrap().run_id;
    let mut run = engine.get(NS, id).await.unwrap();
    for _ in 0..300 {
        if run.status.is_terminal() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        run = engine.get(NS, id).await.unwrap();
    }
    stop.send(true).unwrap();
    assert_eq!(run.status, clankr_engine::RunStatus::Succeeded, "{:?}", run.state.error);
    assert_eq!(run.state.data["w"]["output"], json!({"double": 4, "kernel": "Linux"}), "{}", run.state.data["w"]);
    let (to, state): (Option<String>, String) = sqlx::query_as("select delegated_to, state from tool_calls where namespace = $1").bind(NS).fetch_one(store.pool()).await.unwrap();
    assert_eq!(state, "done");
    assert!(to.unwrap().contains("agent/a/a-i1"), "recorded as run by the agent's node");
}
