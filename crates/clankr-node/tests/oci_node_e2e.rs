//! A TypeScript agent built FROM the clankr/agent-node base image runs as a kennel container:
//! `@clankr/sdk` resolves inside the image, the root is read-only, there's no network, and it
//! runs as nobody. Skips when kennel isn't built or node:24-slim can't be pulled (offline).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use clankr_node::client::Client;
use clankr_node::daemon::{Config, run_instance};
use clankr_store::Store;
use clankr_store::embedded::DevPostgres;
use clankr_types::{RequestCtx, Resource};
use serde_json::{Value, json};

const TOKEN: &str = "oci-node-test-token";
const NS: &str = "ts";
const NODE_BASE: &str = "docker.io/library/node@sha256:a43f7fc3e52e87d12a905f46eea0275def84ca8f2981a5b53fed44d7fb398ee0";

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

fn kennel(bin: &Path, root: &Path, args: &[&str]) -> anyhow::Result<String> {
    let out = Command::new(bin).arg("--root").arg(root).args(args).output()?;
    anyhow::ensure!(out.status.success(), "kennel {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    Ok(String::from_utf8(out.stdout)?.trim().to_owned())
}

const AGENT: &str = r#"
import { AgentApp } from "@clankr/sdk"
import { writeFileSync } from "node:fs"
import { connect } from "node:net"

const app = new AgentApp()
app.task("write", async (task, ctx) => {
  const reply = await ctx.models.generate("default", [{ role: "user", content: task.input.input.topic }])
  let root = "rw"
  try { writeFileSync("/app/x", "") } catch { root = "ro" }
  const network = await new Promise(ok => {
    const s = connect({ host: "1.1.1.1", port: 53, timeout: 1000 }, () => { s.destroy(); ok("up") })
    s.on("error", () => ok("down")).on("timeout", () => { s.destroy(); ok("down") })
  })
  // egress only through the node's proxy, only to listed hosts
  const allowed = await fetch(`http://127.0.0.2:${process.env.UPSTREAM_PORT}/hello`).then(r => r.text()).catch(e => `error: ${e.cause?.code ?? e.message}`)
  const refused = await fetch("http://example.com/").then(r => r.status).catch(e => `error: ${e.cause?.code ?? e.message}`)
  return { summary: reply.text, root, network, uid: process.getuid!(), node: process.version, allowed, refused }
})
app.run()
"#;

#[tokio::test(flavor = "multi_thread")]
async fn typescript_agent_runs_from_the_node_base_image() {
    run_agent("container").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_same_agent_runs_under_gvisor() {
    if !std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join("runsc").is_file())) {
        eprintln!("SKIPPED: runsc (gVisor) not installed");
        return;
    }
    run_agent("gvisor").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_same_agent_runs_in_a_microvm() {
    let kernel = std::env::var_os("KENNEL_VM_KERNEL").map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share/kennel-vmlinuz"))).filter(|k| k.is_file());
    let fc = std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join("firecracker").is_file()));
    let (Some(kernel), true) = (kernel, fc) else {
        eprintln!("SKIPPED: needs firecracker and a guest kernel (KENNEL_VM_KERNEL or ~/.local/share/kennel-vmlinuz)");
        return;
    };
    // kennel (started by the node) picks the kernel up from the environment
    unsafe { std::env::set_var("KENNEL_VM_KERNEL", kernel) };
    run_agent("microvm").await;
}

/// Direct network under gVisor: refused without a NetworkPolicy, then public hosts are
/// reachable without the proxy while private ones aren't.
#[tokio::test(flavor = "multi_thread")]
async fn direct_network_agent_under_gvisor() {
    let online = std::net::TcpStream::connect_timeout(&"1.1.1.1:80".parse().unwrap(), Duration::from_secs(3)).is_ok();
    let runsc = std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join("runsc").is_file()));
    if !online || !runsc {
        eprintln!("SKIPPED: needs the network and runsc");
        return;
    }
    run_agent_with("gvisor", true).await;
}

/// The agent under kennel runtime class `class`; everything it checks must hold for each.
async fn run_agent(class: &str) {
    run_agent_with(class, false).await
}

async fn run_agent_with(class: &str, direct: bool) {
    let kbin = std::env::var_os("KENNEL").map(PathBuf::from).unwrap_or_else(|| repo().join("../kennel/target/debug/kennel"));
    if !kbin.exists() {
        eprintln!("SKIPPED: kennel binary not found (build ~/Dev/rust/kennel or set KENNEL)");
        return;
    }
    let kroot = repo().join("target/kennel-test-store");
    if kennel(&kbin, &kroot, &["inspect", NODE_BASE]).is_err() && kennel(&kbin, &kroot, &["pull", NODE_BASE]).is_err() {
        eprintln!("SKIPPED: can't pull node:24-slim (offline?)");
        return;
    }
    // the upstream lives on the host's loopback, so let the proxy reach a private address (dev knob)
    unsafe { std::env::set_var("CLANKR_EGRESS_ALLOW_PRIVATE", "1") };
    let up = tokio::net::TcpListener::bind("127.0.0.2:0").await.unwrap();
    let up_port = up.local_addr().unwrap().port();
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        while let Ok((mut c, _)) = up.accept().await {
            let mut b = [0u8; 2048];
            let _ = c.read(&mut b).await;
            let _ = c.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 13\r\nconnection: close\r\n\r\nhello, agent!").await;
        }
    });
    let base = kennel(&kbin, &kroot, &["build", "-t", "clankr/agent-node:test", repo().join("sdk/typescript").to_str().unwrap()]).unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let ctx_dir = tmp.path().join("agent");
    std::fs::create_dir_all(&ctx_dir).unwrap();
    std::fs::write(ctx_dir.join("agent.ts"), AGENT).unwrap();
    std::fs::write(
        ctx_dir.join("Kennelfile"),
        format!("FROM clankr/agent-node@{}\nCOPY agent.ts /app/agent.ts\nENV UPSTREAM_PORT={up_port}\nAGENT_ENTRYPOINT [\"node\", \"/app/agent.ts\"]\n", base.rsplit_once('@').unwrap().1),
    )
    .unwrap();
    let image = kennel(&kbin, &kroot, &["build", "-t", "ts/writer:test", ctx_dir.to_str().unwrap()]).unwrap();
    let pinned = format!("ts/writer@{}", image.rsplit_once('@').unwrap().1);

    let _ = rustls::crypto::ring::default_provider().install_default();
    let pg = DevPostgres::start(None).await.unwrap();
    let store = Store::connect(&pg.database("clankr").await.unwrap()).await.unwrap();
    let state = clankrd::AppState::new(store.clone(), TOKEN.into(), clankr_store::secrets::MasterKey::generate()).await.unwrap();
    clankrd::spawn_ticker(state.engine.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, clankrd::router(state)).await.unwrap() });
    let rctx = RequestCtx { actor: "owner".into(), request_id: "ts".into() };
    let res = |kind: &str, name: &str, spec: Value| serde_json::from_value::<Resource>(json!({"apiVersion": "clankr.io/v1alpha1", "kind": kind, "metadata": {"name": name, "namespace": NS}, "spec": spec})).unwrap();
    for r in [
        res("ProviderConnection", "mock", json!({"provider": "mock", "allowedModels": ["mock-1"]})),
        res("ModelClaim", "default", json!({"providerConnection": "mock", "model": "mock-1"})),
        res("Workflow", "wf", json!({"entrypoint": "write", "nodes": {"write": {"type": "agent", "agentRef": "writer"}}})),
    ] {
        store.create(&rctx, r).await.unwrap();
    }
    let mut agent = json!({"runtime": {"type": "oci", "image": pinned, "class": class}, "models": ["default"], "egress": [format!("127.0.0.2:{up_port}")], "taskTtlSeconds": 30, "resources": {"memoryMiB": 256, "cpus": 0.5, "pids": 64}});
    if direct {
        agent["network"] = json!("direct");
    }
    // through the API, where policies are enforced
    let post_agent = || {
        let body = json!({"apiVersion": "clankr.io/v1alpha1", "kind": "Agent", "metadata": {"name": "writer", "namespace": NS}, "spec": agent.clone()});
        let url = format!("{addr}/v1/namespaces/{NS}/agents");
        async move { reqwest::Client::new().post(url).bearer_auth(TOKEN).json(&body).send().await.unwrap() }
    };
    if direct {
        let r = post_agent().await;
        assert_eq!(r.status(), 403, "direct network without a NetworkPolicy is refused");
        store.create(&rctx, res("NetworkPolicy", "open", json!({"allowDirect": true}))).await.unwrap();
    }
    let r = post_agent().await;
    assert_eq!(r.status(), 201, "{}", r.text().await.unwrap_or_default());
    let (stop, rx) = tokio::sync::watch::channel(false);
    let cfg = Config {
        agent: "writer".into(),
        namespace: NS.into(),
        work_dir: tmp.path().join("node"),
        instance: format!("writer-{class}-{}", std::process::id()),
        kennel: kbin.clone(),
        kennel_root: Some(kroot.clone()),
        ready: None,
        spec: None,
        hibernate: None,
        checkpoint: None,
    };
    tokio::spawn(run_instance(Client::new(&addr, TOKEN, NS), cfg, rx));

    let engine = clankr_engine::Engine::new(store.clone());
    let id = engine.start(&rctx, NS, "wf", json!({"topic": "base images"}), None).await.unwrap().run_id;
    let mut run = engine.get(NS, id).await.unwrap();
    for _ in 0..300 {
        if run.status.is_terminal() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        run = engine.get(NS, id).await.unwrap();
    }
    // direct mode: the node's per-connection log (kennel --nat-log), found by instance name
    let nat_log = std::env::var_os("XDG_RUNTIME_DIR").and_then(|r| {
        std::fs::read_dir(r).ok()?.filter_map(|e| e.ok()).map(|e| e.path().join(format!("writer-{class}-{}", std::process::id())).join("nat.log")).find(|p| p.exists())
    });
    stop.send(true).unwrap();
    assert_eq!(run.status, clankr_engine::RunStatus::Succeeded, "{:?}", run.state.error);
    let out = &run.state.data["write"];
    assert_eq!(out["summary"], "mock(mock-1): base images");
    assert_eq!(out["root"], "ro", "{out}");
    if direct {
        assert_eq!(out["network"], "up", "direct: its own outbound network: {out}");
        assert!(out["allowed"].as_str().is_some_and(|a| a.starts_with("error")), "direct: no proxy, and private hosts are dropped: {out}");
        assert_eq!(out["refused"], 200, "direct: public hosts reachable without the allowlist: {out}");
        let logged = nat_log.map(|p| std::fs::read_to_string(p).unwrap_or_default()).unwrap_or_default();
        assert!(logged.contains("tcp 1.1.1.1:53 accept"), "direct: connections logged with their verdict: {logged:?}");
        stop.send(true).unwrap();
        return;
    }
    assert_eq!(out["network"], "down", "{out}");
    assert_eq!(out["uid"], 65534, "{out}");
    assert!(out["node"].as_str().unwrap().starts_with("v24."), "{out}");
    assert_eq!(out["allowed"], "hello, agent!", "listed host reached through the node's proxy: {out}");
    // Node tunnels every proxied request with CONNECT, so the proxy's 403 surfaces as a failed tunnel
    assert!(out["refused"].as_str().is_some_and(|r| r.starts_with("error")), "unlisted host refused by the proxy: {out}");
}
