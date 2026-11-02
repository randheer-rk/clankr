//! Tool executors that run code: one fresh kennel container per call (no network, read-only
//! root, resource-capped), and MCP servers, over streamable HTTP or over stdio inside that
//! same sandbox.

use std::path::PathBuf;
use std::process::Stdio;

use clankr_types::agent::{ImagePolicySpec, Runtime, SandboxClass, image_allowed};
use clankr_types::{ApiError, ErrorCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

const MCP_PROTOCOL: &str = "2025-06-18";
/// Ceiling on what we read from a container or MCP server, whatever the tool's own limit.
const READ_CAP: usize = 16 << 20;

/// Where the gateway runs sandboxed tools: the kennel binary and its image store.
#[derive(Debug, Clone)]
pub struct Sandbox {
    pub kennel: PathBuf,
    pub root: Option<PathBuf>,
}

/// A container to run per call. The image must be pinned by digest and pass the namespace's
/// ImagePolicies (signatures verified by kennel at launch, like agents on nodes).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Container {
    pub image: String,
    /// Appended to the image entrypoint.
    #[serde(default)]
    pub command: Vec<String>,
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
    #[serde(default = "default_memory", rename = "memoryMiB")]
    pub memory_mib: u64,
    #[serde(default = "default_cpus")]
    pub cpus: f64,
    #[serde(default = "default_pids")]
    pub pids: i64,
    /// kennel runtime class: container (default), gvisor or microvm.
    #[serde(default, skip_serializing_if = "SandboxClass::is_container")]
    pub runtime: SandboxClass,
}

fn default_memory() -> u64 {
    256
}
fn default_cpus() -> f64 {
    1.0
}
fn default_pids() -> i64 {
    64
}

pub fn admit_container(c: &Container) -> Result<(), String> {
    if !c.image.contains("@sha256:") {
        return Err("image must be pinned by digest (name@sha256:…)".into());
    }
    if !(16..=16384).contains(&c.memory_mib) || !(0.05..=16.0).contains(&c.cpus) || !(8..=4096).contains(&c.pids) {
        return Err("memoryMiB 16..16384, cpus 0.05..16, pids 8..4096".into());
    }
    if c.env.keys().any(|k| k.is_empty() || k.contains('=')) {
        return Err("env names can't be empty or contain '='".into());
    }
    Ok(())
}

fn tool_err(msg: impl Into<String>) -> ApiError {
    ApiError::new(ErrorCode::ProviderUnavailable, msg)
}

impl Sandbox {
    /// `kennel run` for `c`, stdin/stdout piped. Killed (container and all) when dropped.
    async fn spawn(&self, c: &Container, policies: &[(String, ImagePolicySpec)]) -> Result<(tokio::process::Child, tempdir::Dir), ApiError> {
        let keys = image_allowed(policies, &Runtime::Oci { image: c.image.clone(), class: c.runtime }).map_err(|e| ApiError::new(ErrorCode::Forbidden, format!("refusing to run: {e}")))?;
        let dir = tempdir::Dir::new().map_err(|e| tool_err(format!("sandbox: {e}")))?;
        let mut cmd = tokio::process::Command::new(&self.kennel);
        if let Some(root) = &self.root {
            cmd.arg("--root").arg(root);
        }
        cmd.args(["run", "--runtime", c.runtime.kennel_arg(), "--network", "none", "--name", &format!("tool-{}", uuid::Uuid::new_v4().simple())])
            .args(["--memory", &c.memory_mib.to_string(), "--cpus", &c.cpus.to_string(), "--pids", &c.pids.to_string()]);
        for (k, v) in &c.env {
            cmd.arg("-e").arg(format!("{k}={v}"));
        }
        for (i, pem) in keys.iter().enumerate() {
            let p = dir.0.join(format!("trusted-{i}.pub"));
            std::fs::write(&p, pem).map_err(|e| tool_err(format!("sandbox: {e}")))?;
            cmd.arg("--verify-key").arg(p);
        }
        cmd.arg(&c.image);
        if !c.command.is_empty() {
            cmd.arg("--").args(&c.command);
        }
        let child = cmd
            // the container sees only -e values; kennel itself needs the session env (cgroups via
            // systemd), so just keep the control plane's own secrets away from it
            .env_remove("DATABASE_URL")
            .env_remove("CLANKR_TOKEN")
            .env_remove("CLANKR_JOIN_TOKEN")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| tool_err(format!("sandbox: can't start {}: {e}", self.kennel.display())))?;
        Ok((child, dir))
    }

    /// Runs one call: the request as JSON on stdin; stdout is the result (JSON if it parses,
    /// else text); a non-zero exit fails the call with the end of stderr.
    pub async fn run(&self, c: &Container, policies: &[(String, ImagePolicySpec)], request: &Value) -> Result<Value, ApiError> {
        let (mut child, _dir) = self.spawn(c, policies).await?;
        let mut stdin = child.stdin.take().expect("piped");
        let body = serde_json::to_vec(request).expect("json");
        // write and read concurrently: a tool may answer before it has read everything
        let write = async move {
            let _ = stdin.write_all(&body).await;
            drop(stdin);
        };
        let mut out = child.stdout.take().expect("piped");
        let mut err = child.stderr.take().expect("piped");
        let (mut o, mut e) = (vec![], vec![]);
        let read_out = async { (&mut out).take(READ_CAP as u64).read_to_end(&mut o).await };
        let read_err = async { (&mut err).take(64 << 10).read_to_end(&mut e).await };
        let ((), r1, r2, status) = tokio::join!(write, read_out, read_err, child.wait());
        r1.and(r2).map_err(|x| tool_err(format!("sandbox: {x}")))?;
        let status = status.map_err(|x| tool_err(format!("sandbox: {x}")))?;
        if !status.success() {
            let tail: String = String::from_utf8_lossy(&e).chars().rev().take(500).collect::<Vec<_>>().into_iter().rev().collect();
            return Err(tool_err(format!("tool exited with {status}: {}", tail.trim())));
        }
        let text = String::from_utf8_lossy(&o);
        Ok(serde_json::from_str(text.trim()).unwrap_or_else(|_| Value::String(text.into_owned())))
    }

    /// One MCP `tools/call` against a stdio server started in a fresh container.
    pub async fn mcp(&self, c: &Container, policies: &[(String, ImagePolicySpec)], tool: &str, arguments: &Value) -> Result<Value, ApiError> {
        let (mut child, _dir) = self.spawn(c, policies).await?;
        let mut stdin = child.stdin.take().expect("piped");
        let mut lines = BufReader::new(child.stdout.take().expect("piped").take(READ_CAP as u64)).lines();
        let send = |m: Value| format!("{m}\n");
        stdin.write_all(send(initialize(1)).as_bytes()).await.map_err(|e| tool_err(format!("mcp: {e}")))?;
        rpc_result(next_response(&mut lines, 1).await?)?;
        let rest = send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"})) + &send(call(2, tool, arguments));
        stdin.write_all(rest.as_bytes()).await.map_err(|e| tool_err(format!("mcp: {e}")))?;
        let r = next_response(&mut lines, 2).await;
        drop(stdin);
        let _ = child.kill().await;
        call_output(rpc_result(r?)?)
    }
}

async fn next_response<R: tokio::io::AsyncBufRead + Unpin>(lines: &mut tokio::io::Lines<R>, id: i64) -> Result<Value, ApiError> {
    while let Some(l) = lines.next_line().await.map_err(|e| tool_err(format!("mcp: {e}")))? {
        // servers may log or send notifications and requests of their own; skip those
        if let Ok(v) = serde_json::from_str::<Value>(&l)
            && v["id"].as_i64() == Some(id)
            && (v.get("result").is_some() || v.get("error").is_some())
        {
            return Ok(v);
        }
    }
    Err(tool_err("mcp server exited without answering"))
}

fn initialize(id: i64) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": "initialize", "params": {"protocolVersion": MCP_PROTOCOL, "capabilities": {}, "clientInfo": {"name": "clankr", "version": env!("CARGO_PKG_VERSION")}}})
}

fn call(id: i64, tool: &str, arguments: &Value) -> Value {
    let args = if arguments.is_null() { json!({}) } else { arguments.clone() };
    json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": tool, "arguments": args}})
}

fn rpc_result(v: Value) -> Result<Value, ApiError> {
    if let Some(e) = v.get("error") {
        let msg: String = e["message"].as_str().unwrap_or("error").chars().take(300).collect();
        return Err(ApiError::new(ErrorCode::InvalidSpec, format!("mcp error {}: {msg}", e["code"])));
    }
    Ok(v["result"].clone())
}

/// `structuredContent` if the server sent it; else the text (parsed as JSON when it is), or
/// the raw content blocks. `isError` fails the call.
fn call_output(r: Value) -> Result<Value, ApiError> {
    let texts: Vec<&str> = r["content"].as_array().into_iter().flatten().filter_map(|b| (b["type"] == "text").then(|| b["text"].as_str()).flatten()).collect();
    if r["isError"] == true {
        let msg: String = texts.join("\n").chars().take(500).collect();
        return Err(tool_err(if msg.is_empty() { "tool reported an error".into() } else { msg }));
    }
    if let Some(s) = r.get("structuredContent") {
        return Ok(s.clone());
    }
    match (texts.as_slice(), r["content"].as_array().map_or(0, Vec::len)) {
        ([t], 1) => Ok(serde_json::from_str(t).unwrap_or_else(|_| Value::String((*t).to_owned()))),
        _ => Ok(r["content"].clone()),
    }
}

/// One MCP `tools/call` over streamable HTTP: initialize (keeping the session id), notify,
/// call, then end the session. Responses may be plain JSON or an SSE stream.
pub async fn mcp_http(http: &reqwest::Client, url: &str, auth: Option<(&str, &str)>, tool: &str, arguments: &Value) -> Result<Value, ApiError> {
    let post = |body: Value, session: Option<&str>| {
        let mut rb = http.post(url).header("accept", "application/json, text/event-stream").header("mcp-protocol-version", MCP_PROTOCOL).json(&body);
        if let Some(s) = session {
            rb = rb.header("mcp-session-id", s);
        }
        if let Some((h, v)) = auth {
            rb = rb.header(h, v);
        }
        rb.send()
    };
    let net = |e: reqwest::Error| tool_err(format!("mcp: {}", e.without_url()));
    let resp = post(initialize(1), None).await.map_err(net)?;
    let session = resp.headers().get("mcp-session-id").and_then(|v| v.to_str().ok()).map(str::to_owned);
    rpc_result(read_http(resp, 1).await?)?;
    post(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}), session.as_deref()).await.map_err(net)?;
    let r = read_http(post(call(2, tool, arguments), session.as_deref()).await.map_err(net)?, 2).await;
    if let Some(s) = &session {
        let _ = http.delete(url).header("mcp-session-id", s).send().await;
    }
    call_output(rpc_result(r?)?)
}

async fn read_http(resp: reqwest::Response, id: i64) -> Result<Value, ApiError> {
    let status = resp.status();
    let sse = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).is_some_and(|c| c.starts_with("text/event-stream"));
    if !status.is_success() {
        let body: String = resp.text().await.unwrap_or_default().chars().take(300).collect();
        let code = if status.is_client_error() { ErrorCode::InvalidSpec } else { ErrorCode::ProviderUnavailable };
        return Err(ApiError::new(code, format!("mcp server returned {status}: {body}")));
    }
    if !sse {
        let body = resp.bytes().await.map_err(|e| tool_err(format!("mcp: {}", e.without_url())))?;
        return serde_json::from_slice(&body).map_err(|e| tool_err(format!("mcp: bad response: {e}")));
    }
    // ponytail: buffers the stream until our response shows up; fine for a single call per session
    let mut buf = String::new();
    let mut resp = resp;
    while let Some(chunk) = resp.chunk().await.map_err(|e| tool_err(format!("mcp: {}", e.without_url())))? {
        buf.push_str(&String::from_utf8_lossy(&chunk));
        if buf.len() > READ_CAP {
            return Err(tool_err("mcp: response too large"));
        }
        for line in buf.lines() {
            if let Some(d) = line.strip_prefix("data:")
                && let Ok(v) = serde_json::from_str::<Value>(d.trim())
                && v["id"].as_i64() == Some(id)
                && (v.get("result").is_some() || v.get("error").is_some())
            {
                return Ok(v);
            }
        }
    }
    Err(tool_err("mcp: stream ended without a response"))
}

mod tempdir {
    /// A private directory for one call's key files, removed on drop.
    pub struct Dir(pub std::path::PathBuf);
    impl Dir {
        pub fn new() -> std::io::Result<Self> {
            let p = std::env::temp_dir().join(format!("clankr-tool-{}", uuid::Uuid::new_v4().simple()));
            std::fs::create_dir(&p)?;
            Ok(Self(p))
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_outputs() {
        assert_eq!(call_output(json!({"content": [{"type": "text", "text": "{\"a\":1}"}]})).unwrap(), json!({"a": 1}));
        assert_eq!(call_output(json!({"content": [{"type": "text", "text": "hi"}]})).unwrap(), json!("hi"));
        assert_eq!(call_output(json!({"content": [], "structuredContent": {"x": 2}})).unwrap(), json!({"x": 2}));
        assert!(call_output(json!({"content": [{"type": "text", "text": "nope"}], "isError": true})).unwrap_err().message.contains("nope"));
        assert!(rpc_result(json!({"error": {"code": -32602, "message": "unknown tool"}})).unwrap_err().message.contains("unknown tool"));
        assert!(admit_container(&serde_json::from_value(json!({"image": "busybox:1"})).unwrap()).is_err());
    }
}
