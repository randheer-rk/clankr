//! Sandboxed and MCP tool executors: a container per call via kennel (skipped when kennel or
//! the busybox image isn't available), stdio MCP servers in that sandbox, and MCP over
//! streamable HTTP with sessions and SSE responses.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::post;
use clankr_gateway::Gateway;
use clankr_gateway::sandbox::Sandbox;
use clankr_gateway::tools::ToolCall;
use clankr_store::Store;
use clankr_store::embedded::DevPostgres;
use clankr_store::secrets::MasterKey;
use clankr_types::{ErrorCode, RequestCtx, Resource};
use serde_json::{Value, json};

const NS: &str = "sbx";
const BUSYBOX: &str = "docker.io/library/busybox:1.37";

fn ctx(actor: &str) -> RequestCtx {
    RequestCtx { actor: actor.into(), request_id: "t".into() }
}

fn res(kind: &str, name: &str, spec: Value) -> Resource {
    serde_json::from_value(json!({"apiVersion": "clankr.io/v1alpha1", "kind": kind, "metadata": {"name": name, "namespace": NS}, "spec": spec})).unwrap()
}

fn call(tool: &str, method: &str, input: Value, key: &str) -> ToolCall {
    serde_json::from_value(json!({"tool": tool, "method": method, "input": input, "idempotencyKey": key})).unwrap()
}

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// kennel and a digest-pinned busybox in the shared test store, or None (skip).
fn busybox() -> Option<(PathBuf, PathBuf, String)> {
    let kbin = std::env::var_os("KENNEL").map(PathBuf::from).unwrap_or_else(|| repo().join("../kennel/target/debug/kennel"));
    if !kbin.exists() {
        return None;
    }
    let root = repo().join("target/kennel-test-store");
    let run = |args: &[&str]| std::process::Command::new(&kbin).arg("--root").arg(&root).args(args).output().ok().filter(|o| o.status.success());
    let out = run(&["inspect", BUSYBOX]).or_else(|| run(&["pull", BUSYBOX]).and_then(|_| run(&["inspect", BUSYBOX])))?;
    let digest = serde_json::from_slice::<Value>(&out.stdout).ok()?["digest"].as_str()?.to_owned();
    Some((kbin, root, format!("docker.io/library/busybox@{digest}")))
}

async fn setup() -> (DevPostgres, Store, MasterKey) {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let pg = DevPostgres::start(None).await.unwrap();
    let s = Store::connect(&pg.database("clankr").await.unwrap()).await.unwrap();
    (pg, s, MasterKey::generate())
}

#[tokio::test]
async fn sandbox_and_stdio_mcp_tools_run_in_containers() {
    let Some((kbin, root, image)) = busybox() else {
        eprintln!("SKIPPED: kennel or {BUSYBOX} unavailable (build ~/Dev/rust/kennel or set KENNEL)");
        return;
    };
    let (_pg, s, master) = setup().await;
    let container = |script: &str| json!({"image": image, "command": ["sh", "-c", script], "memoryMiB": 64, "cpus": 0.5});
    // an MCP server in 6 lines of sh: answers initialize, skips the notification, answers the call
    let mcp = r#"read l; echo '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{},"serverInfo":{"name":"sh","version":"1"}}}'
        read l; read l; echo "log line, not json"
        case "$l" in *'"name":"add"'*) echo '{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"{\"sum\":3}"}]}}';;
        *) echo '{"jsonrpc":"2.0","id":2,"error":{"code":-32602,"message":"unknown tool"}}';; esac"#;
    for r in [
        res("Tool", "shell", json!({"version": "1", "timeoutSeconds": 20, "executor": {"type": "sandbox", "container": container(
            r#"read -r req; echo "{\"echo\": $req, \"ifaces\": \"$(tail -n +3 /proc/net/dev | cut -d: -f1 | tr -d ' \n')\"}""#)},
            "methods": {"run": {"sideEffects": "idempotent"}}})),
        res("Tool", "boom", json!({"version": "1", "timeoutSeconds": 20, "executor": {"type": "sandbox", "container": container("echo nope >&2; exit 3")}, "methods": {"run": {"sideEffects": "idempotent"}}})),
        res("Tool", "calc", json!({"version": "1", "timeoutSeconds": 20, "executor": {"type": "mcpStdio", "container": container(mcp)}, "methods": {"add": {"sideEffects": "readOnly"}, "mul": {"sideEffects": "readOnly"}}})),
        res("ToolClaim", "c", json!({"allow": [{"tool": "shell"}, {"tool": "boom"}, {"tool": "calc"}]})),
    ] {
        s.create(&ctx("owner"), r).await.unwrap();
    }
    let agent = ctx("agent/a/i");

    let off = Gateway::new(s.clone(), master.clone());
    let e = off.call_tool(&agent, NS, "c", call("shell", "run", json!({}), "k0")).await.unwrap_err();
    assert!(e.message.contains("--tool-kennel"), "{e:?}");

    let gw = Gateway::new(s.clone(), master).with_sandbox(Sandbox { kennel: kbin, root: Some(root) });
    let r = gw.call_tool(&agent, NS, "c", call("shell", "run", json!({"cmd": "ls"}), "k1")).await.unwrap();
    assert_eq!(r.output["echo"]["input"], json!({"cmd": "ls"}), "{:?}", r.output);
    assert_eq!(r.output["echo"]["method"], "run");
    assert_eq!(r.output["ifaces"], "lo", "own network namespace with loopback only: {:?}", r.output);

    let e = gw.call_tool(&agent, NS, "c", call("boom", "run", json!({}), "k2")).await.unwrap_err();
    assert!(e.message.contains("exit status: 3") && e.message.contains("nope"), "{e:?}");

    let r = gw.call_tool(&agent, NS, "c", call("calc", "add", json!({"a": 1, "b": 2}), "k3")).await.unwrap();
    assert_eq!(r.output, json!({"sum": 3}));
    let e = gw.call_tool(&agent, NS, "c", call("calc", "mul", json!({}), "k4")).await.unwrap_err();
    assert!(e.message.contains("unknown tool"), "{e:?}");

    // ImagePolicy applies to tool images too
    s.create(&ctx("owner"), res("ImagePolicy", "only-ours", json!({"allowedRepositories": ["registry.example/"]}))).await.unwrap();
    let e = gw.call_tool(&agent, NS, "c", call("shell", "run", json!({}), "k5")).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::Forbidden, "{e:?}");
}

#[tokio::test]
async fn mcp_over_streamable_http() {
    let (_pg, s, master) = setup().await;
    s.secret_put(&ctx("owner"), &master, NS, "mcp", &std::collections::BTreeMap::from([("token".into(), "Bearer sekret".into())])).await.unwrap();
    let seen: Arc<Mutex<Vec<(String, String, String)>>> = Arc::default();
    let log = seen.clone();
    let app = axum::Router::new().route(
        "/mcp",
        post(move |h: HeaderMap, body: String| {
            let log = log.clone();
            async move {
                let v: Value = serde_json::from_str(&body).unwrap();
                let hdr = |k: &str| h.get(k).map(|x| x.to_str().unwrap().to_owned()).unwrap_or_default();
                log.lock().unwrap().push((v["method"].as_str().unwrap_or_default().to_owned(), hdr("mcp-session-id"), hdr("authorization")));
                match v["method"].as_str().unwrap() {
                    "initialize" => ([("mcp-session-id", "s-1")], axum::Json(json!({"jsonrpc": "2.0", "id": v["id"], "result": {"protocolVersion": "2025-06-18", "capabilities": {"tools": {}}}}))).into_response(),
                    "notifications/initialized" => StatusCode::ACCEPTED.into_response(),
                    _ if hdr("mcp-session-id") != "s-1" => StatusCode::NOT_FOUND.into_response(),
                    _ => {
                        let result = match v["params"]["name"].as_str().unwrap() {
                            "search" => json!({"content": [{"type": "text", "text": "ignored"}], "structuredContent": {"hits": [v["params"]["arguments"]["q"]]}}),
                            _ => json!({"content": [{"type": "text", "text": "rate limited"}], "isError": true}),
                        };
                        let note = json!({"jsonrpc": "2.0", "method": "notifications/progress", "params": {}});
                        let sse = format!("event: message\ndata: {note}\n\nevent: message\ndata: {}\n\n", json!({"jsonrpc": "2.0", "id": v["id"], "result": result}));
                        ([("content-type", "text/event-stream")], sse).into_response()
                    }
                }
            }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://127.0.0.1:{}/mcp", l.local_addr().unwrap().port());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    for r in [
        res("Tool", "docs", json!({"version": "1", "executor": {"type": "mcpHttp", "url": url, "secretRef": {"name": "mcp", "key": "token"}}, "methods": {"search": {"sideEffects": "readOnly"}, "flaky": {"sideEffects": "readOnly"}}})),
        res("ToolClaim", "c", json!({"allow": [{"tool": "docs"}]})),
    ] {
        s.create(&ctx("owner"), r).await.unwrap();
    }
    let gw = Gateway::new(s.clone(), master);
    let agent = ctx("agent/a/i");
    let r = gw.call_tool(&agent, NS, "c", call("docs", "search", json!({"q": "canary"}), "h1")).await.unwrap();
    assert_eq!(r.output, json!({"hits": ["canary"]}));
    let e = gw.call_tool(&agent, NS, "c", call("docs", "flaky", json!({}), "h2")).await.unwrap_err();
    assert!(e.message.contains("rate limited"), "{e:?}");
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen[..3].iter().map(|x| x.0.as_str()).collect::<Vec<_>>(), ["initialize", "notifications/initialized", "tools/call"]);
    assert!(seen.iter().all(|x| x.2 == "Bearer sekret"), "credential brokered on every request");
    assert_eq!(seen[2].1, "s-1", "the call carries the session");

    let unpinned = json!({"version": "1", "executor": {"type": "sandbox", "container": {"image": "busybox:latest"}}, "methods": {"x": {}}});
    assert!(clankr_gateway::tools::admit_tool(&unpinned).unwrap_err().message.contains("pinned"));
}
