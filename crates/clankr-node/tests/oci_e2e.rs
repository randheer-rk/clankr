//! The node runs a Python agent as a kennel container (ADR 0007): image built from the Clankr
//! base image, sandboxed (read-only root, no network), talking to the node over the bind-mounted
//! socket. Skips when kennel isn't built or the base image can't be pulled (offline).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use clankr_node::client::Client;
use clankr_node::daemon::{Config, run_instance};
use clankr_store::Store;
use clankr_store::embedded::DevPostgres;
use clankr_types::{RequestCtx, Resource};
use serde_json::{Value, json};

const TOKEN: &str = "oci-test-token";
const NS: &str = "research";
const PYTHON_BASE: &str = "docker.io/library/python:3.12-slim";

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

/// The kennel binary from $KENNEL or a sibling checkout (~/Dev/rust/kennel).
fn kennel_bin() -> Option<PathBuf> {
    let p = std::env::var_os("KENNEL").map(PathBuf::from).unwrap_or_else(|| repo().join("../kennel/target/debug/kennel"));
    p.exists().then_some(p)
}

fn kennel(bin: &Path, root: &Path, args: &[&str]) -> anyhow::Result<String> {
    let out = Command::new(bin).arg("--root").arg(root).args(args).output()?;
    anyhow::ensure!(out.status.success(), "kennel {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    Ok(String::from_utf8(out.stdout)?.trim().to_owned())
}

const AGENT: &str = r#"
import os, socket
from clankr import AgentApp

app = AgentApp()

@app.task("research")
async def research(task, ctx):
    reply = await ctx.models.generate("default", [{"role": "user", "content": task.input["input"]["topic"]}])
    saved = await ctx.context("memory").put("progress", {"summary": reply["text"]})
    try:
        open("/app/agent.py", "a").close(); root = "rw"
    except OSError:
        root = "ro"
    try:
        socket.create_connection(("1.1.1.1", 53), timeout=1).close(); net = "up"
    except OSError:
        net = "down"
    status = dict(l.split(":", 1) for l in open("/proc/self/status").read().splitlines() if ":" in l)
    cg = lambda f: open("/sys/fs/cgroup/" + f).read().strip()
    return {"summary": reply["text"], "contextVersion": saved["version"], "root": root, "network": net,
            "token": os.environ.get("CLANKR_TOKEN"), "pid1": open("/proc/1/cmdline").read().split("\0")[0],
            "uid": os.getuid(), "groups": os.getgroups(), "capEff": status["CapEff"].strip(), "seccomp": status["Seccomp"].strip(),
            "memoryMax": cg("memory.max"), "pidsMax": cg("pids.max"), "cpuMax": cg("cpu.max")}

app.run()
"#;

#[tokio::test(flavor = "multi_thread")]
async fn python_agent_runs_sandboxed_in_a_kennel_container() {
    let Some(kbin) = kennel_bin() else {
        eprintln!("SKIPPED: kennel binary not found (build ~/Dev/rust/kennel or set KENNEL)");
        return;
    };
    // persistent store under target/ so the python base is pulled once
    let kroot = repo().join("target/kennel-test-store");
    if kennel(&kbin, &kroot, &["inspect", PYTHON_BASE]).is_err() && kennel(&kbin, &kroot, &["pull", PYTHON_BASE]).is_err() {
        eprintln!("SKIPPED: can't pull {PYTHON_BASE} (offline?)");
        return;
    }
    let base_ref = kennel(&kbin, &kroot, &["build", "-t", "clankr/agent-python:test", repo().join("sdk/python").to_str().unwrap()]).unwrap();
    let base_digest = base_ref.rsplit_once('@').unwrap().1.to_owned();
    let tmp = tempfile::tempdir().unwrap();
    let ctx_dir = tmp.path().join("agent");
    std::fs::create_dir_all(&ctx_dir).unwrap();
    std::fs::write(ctx_dir.join("agent.py"), AGENT).unwrap();
    std::fs::write(
        ctx_dir.join("Kennelfile"),
        format!("FROM clankr/agent-python@{base_digest}\nCOPY agent.py /app/agent.py\nAGENT_ENTRYPOINT [\"python3\", \"/app/agent.py\"]\n"),
    )
    .unwrap();
    let image = kennel(&kbin, &kroot, &["build", "-t", "research/researcher:test", ctx_dir.to_str().unwrap()]).unwrap();
    let pinned = format!("research/researcher@{}", image.rsplit_once('@').unwrap().1);

    let _ = rustls::crypto::ring::default_provider().install_default();
    let pg = DevPostgres::start(None).await.unwrap();
    let store = Store::connect(&pg.database("clankr").await.unwrap()).await.unwrap();
    let state = clankrd::AppState::new(store.clone(), TOKEN.into(), clankr_store::secrets::MasterKey::generate()).await.unwrap();
    clankrd::spawn_ticker(state.engine.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, clankrd::router(state)).await.unwrap() });

    let rctx = RequestCtx { actor: "owner".into(), request_id: "oci".into() };
    let apply = |kind: &str, name: &str, spec: Value| {
        serde_json::from_value::<Resource>(json!({"apiVersion": "clankr.io/v1alpha1", "kind": kind, "metadata": {"name": name, "namespace": NS}, "spec": spec})).unwrap()
    };
    for r in [
        apply("ProviderConnection", "mock", json!({"provider": "mock", "allowedModels": ["mock-1"]})),
        apply("ModelClaim", "default", json!({"providerConnection": "mock", "model": "mock-1"})),
        apply("ContextClaim", "memory", json!({})),
        apply("Agent", "researcher", json!({"runtime": {"type": "oci", "image": pinned}, "models": ["default"], "contexts": ["memory"], "taskTtlSeconds": 30,
            "resources": {"memoryMiB": 256, "cpus": 0.5, "pids": 64}})),
        apply("Workflow", "wf", json!({"entrypoint": "research", "nodes": {"research": {"type": "agent", "agentRef": "researcher"}}})),
    ] {
        store.create(&rctx, r).await.unwrap();
    }

    let (stop, rx) = tokio::sync::watch::channel(false);
    let cfg = Config {
        agent: "researcher".into(),
        namespace: NS.into(),
        work_dir: tmp.path().join("node"),
        instance: format!("researcher-{}", std::process::id()),
        kennel: kbin.clone(),
        kennel_root: Some(kroot.clone()),
        ready: None,
        spec: None,
        hibernate: None,
        checkpoint: None,
    };
    tokio::spawn(run_instance(Client::new(&base, TOKEN, NS), cfg, rx));

    let engine = clankr_engine::Engine::new(store.clone());
    let id = engine.start(&rctx, NS, "wf", json!({"topic": "sandboxes"}), None).await.unwrap().run_id;
    let mut run = engine.get(NS, id).await.unwrap();
    for _ in 0..300 {
        if run.status.is_terminal() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        run = engine.get(NS, id).await.unwrap();
    }
    stop.send(true).unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;

    assert_eq!(run.status, clankr_engine::RunStatus::Succeeded, "{:?}", run.state.error);
    engine.verify(NS, id).await.unwrap();
    let out = &run.state.data["research"];
    assert_eq!(out["summary"], "mock(mock-1): sandboxes");
    assert_eq!(out["contextVersion"], 1);
    assert_eq!(out["root"], "ro", "container root is read-only");
    assert_eq!(out["network"], "down", "agent container has no network");
    assert_eq!(out["token"], Value::Null);
    assert_eq!(out["pid1"], "/.kennel-init", "kennel-init is PID 1");
    assert_eq!(out["uid"], 65534, "agent runs as nobody (base image USER)");
    assert_eq!(out["groups"], json!([]), "no supplementary groups: the host user's are dropped");
    assert_eq!(out["capEff"], "0000000000000000", "no capabilities");
    assert_eq!(out["seccomp"], "2", "seccomp filter active");
    assert_eq!((out["memoryMax"].as_str(), out["pidsMax"].as_str(), out["cpuMax"].as_str()), (Some("268435456"), Some("64"), Some("50000 100000")), "Agent.spec.resources enforced");
}
