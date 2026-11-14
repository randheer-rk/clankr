//! Tool executors that run code. Containers and stdio MCP servers live in `clankr-sandbox`
//! (shared with nodes); MCP over streamable HTTP runs here, where the credential is brokered.

pub use clankr_sandbox::*;
use clankr_types::{ApiError, ErrorCode};
use serde_json::{Value, json};

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

async fn read_http(mut resp: reqwest::Response, id: i64) -> Result<Value, ApiError> {
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

