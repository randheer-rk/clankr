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
        res("Agent", "writer", json!({"runtime": {"type": "oci", "image": pinned}, "models": ["default"], "egress": [format!("127.0.0.2:{up_port}")], "taskTtlSeconds": 30, "resources": {"memoryMiB": 256, "cpus": 0.5, "pids": 64}})),
        res("Workflow", "wf", json!({"entrypoint": "write", "nodes": {"write": {"type": "agent", "agentRef": "writer"}}})),
    ] {
        store.create(&rctx, r).await.unwrap();
    }
    let (stop, rx) = tokio::sync::watch::channel(false);
    let cfg = Config {
        agent: "writer".into(),
        namespace: NS.into(),
        work_dir: tmp.path().join("node"),
        instance: format!("writer-{}", std::process::id()),
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
    stop.send(true).unwrap();
    assert_eq!(run.status, clankr_engine::RunStatus::Succeeded, "{:?}", run.state.error);
    let out = &run.state.data["write"];
    assert_eq!(out["summary"], "mock(mock-1): base images");
    assert_eq!(out["root"], "ro", "{out}");
    assert_eq!(out["network"], "down", "{out}");
    assert_eq!(out["uid"], 65534, "{out}");
    assert!(out["node"].as_str().unwrap().starts_with("v24."), "{out}");
    assert_eq!(out["allowed"], "hello, agent!", "listed host reached through the node's proxy: {out}");
    // Node tunnels every proxied request with CONNECT, so the proxy's 403 surfaces as a failed tunnel
    assert!(out["refused"].as_str().is_some_and(|r| r.starts_with("error")), "unlisted host refused by the proxy: {out}");
}
