//! A2A (Agent2Agent, v1.0) over JSON-RPC: the client side, used by the `a2a` tool executor to
//! call agents outside the cluster, and the part shapes shared with clankrd's server side.

use clankr_types::{ApiError, ErrorCode};
use serde_json::{Value, json};

pub const PROTOCOL_VERSION: &str = "1.0";

/// The text of a message: its text parts joined, in order.
pub fn text_of(message: &Value) -> String {
    message["parts"].as_array().into_iter().flatten().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("\n")
}

/// The first data part's payload, if any.
pub fn data_of(message: &Value) -> Option<Value> {
    message["parts"].as_array().into_iter().flatten().find_map(|p| p.get("data").cloned())
}

/// Parts for a reply: a string is one text part; `{text, …}` is a text part plus the rest as data; anything else is one data part.
pub fn parts_for(reply: &Value) -> Vec<Value> {
    match reply {
        Value::String(s) => vec![json!({"text": s})],
        Value::Object(o) if o.get("text").is_some_and(Value::is_string) => {
            let mut rest = o.clone();
            let text = rest.remove("text").unwrap();
            let mut parts = vec![json!({"text": text})];
            if !rest.is_empty() {
                parts.push(json!({"data": Value::Object(rest)}));
            }
            parts
        }
        v => vec![json!({"data": v})],
    }
}

/// One JSON-RPC call; JSON-RPC errors become ProviderUnavailable with the server's message.
pub async fn rpc(http: &reqwest::Client, url: &str, auth: Option<(&str, &str)>, method: &str, params: Value) -> Result<Value, ApiError> {
    let mut rb = http.post(url).json(&json!({"jsonrpc": "2.0", "id": uuid::Uuid::new_v4().to_string(), "method": method, "params": params}));
    if let Some((h, v)) = auth {
        rb = rb.header(h, v);
    }
    let resp = rb.send().await.map_err(|e| ApiError::new(ErrorCode::ProviderUnavailable, format!("a2a: {}", e.without_url())))?;
    let status = resp.status();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    if !status.is_success() && body.get("error").is_none() {
        return Err(ApiError::new(if status.as_u16() == 401 || status.as_u16() == 403 { ErrorCode::Forbidden } else { ErrorCode::ProviderUnavailable }, format!("a2a: {status}")));
    }
    if let Some(e) = body.get("error") {
        let msg: String = e["message"].as_str().unwrap_or("error").chars().take(300).collect();
        return Err(ApiError::new(ErrorCode::ProviderUnavailable, format!("a2a: {msg} ({})", e["code"])));
    }
    Ok(body["result"].clone())
}

/// Sends one message and waits for the answer: a Message, or a Task polled until it's done or `deadline` passes.
pub async fn send_and_wait(http: &reqwest::Client, url: &str, auth: Option<(&str, &str)>, text: Option<&str>, data: Option<&Value>, deadline: std::time::Instant) -> Result<Value, ApiError> {
    let mut parts = vec![];
    if let Some(t) = text {
        parts.push(json!({"text": t}));
    }
    if let Some(d) = data {
        parts.push(json!({"data": d}));
    }
    let msg = json!({"messageId": uuid::Uuid::new_v4().to_string(), "role": "ROLE_USER", "parts": parts});
    let mut r = rpc(http, url, auth, "SendMessage", json!({"message": msg})).await?;
    // a Task has a status; a Message has parts
    while r.get("status").is_some() {
        let state = r["status"]["state"].as_str().unwrap_or_default();
        match state {
            "TASK_STATE_COMPLETED" => return Ok(if r["status"]["message"].is_object() { r["status"]["message"].clone() } else { last_artifact(&r) }),
            "TASK_STATE_FAILED" | "TASK_STATE_CANCELED" | "TASK_STATE_REJECTED" => {
                return Err(ApiError::new(ErrorCode::ProviderUnavailable, format!("a2a task {}: {}", r["id"], text_of(&r["status"]["message"]))));
            }
            "TASK_STATE_INPUT_REQUIRED" | "TASK_STATE_AUTH_REQUIRED" => {
                return Err(ApiError::new(ErrorCode::UnsupportedCapability, format!("a2a task {} needs more input, which a tool call can't give", r["id"])));
            }
            _ => {}
        }
        if std::time::Instant::now() >= deadline {
            return Err(ApiError::new(ErrorCode::OutcomeUnknown, format!("a2a task {} is still {state}", r["id"])));
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        r = rpc(http, url, auth, "GetTask", json!({"id": r["id"]})).await?;
    }
    Ok(r)
}

/// A completed task without a status message: its last artifact's parts stand in.
fn last_artifact(task: &Value) -> Value {
    let parts = task["artifacts"].as_array().and_then(|a| a.last()).map(|a| a["parts"].clone()).unwrap_or(json!([]));
    json!({"role": "ROLE_AGENT", "parts": parts})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parts_round_trip() {
        assert_eq!(parts_for(&json!("hi")), json!([{"text": "hi"}]).as_array().unwrap().clone());
        assert_eq!(parts_for(&json!({"text": "hi", "n": 1})), vec![json!({"text": "hi"}), json!({"data": {"n": 1}})]);
        assert_eq!(parts_for(&json!([1])), vec![json!({"data": [1]})]);
        let m = json!({"parts": [{"text": "a"}, {"data": {"k": 1}}, {"text": "b"}]});
        assert_eq!(text_of(&m), "a\nb");
        assert_eq!(data_of(&m), Some(json!({"k": 1})));
    }
}
