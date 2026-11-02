//! Provider adapters. Each maps a provider-neutral request onto one HTTP call and maps the reply
//! (and errors) back. Credentials go in headers only and never appear in errors.

use clankr_types::{ApiError, ErrorCode};
use serde_json::{Value, json};

use crate::{GenerateRequest, Provider, ProviderConnectionSpec, Usage};

pub(crate) struct Reply {
    pub text: String,
    pub stop_reason: Option<String>,
    pub usage: Usage,
}

pub(crate) async fn call(
    http: &reqwest::Client,
    conn: &ProviderConnectionSpec,
    model: &str,
    key: Option<&str>,
    req: &GenerateRequest,
    max_out: u32,
) -> Result<Reply, ApiError> {
    match conn.provider {
        Provider::Mock => Ok(mock(model, req)),
        Provider::Replay => replay(conn, req),
        Provider::OpenaiCompatible => openai(http, conn, model, key.unwrap_or_default(), req, max_out).await,
        Provider::Anthropic => anthropic(http, conn, model, key.unwrap_or_default(), req, max_out).await,
    }
}

fn tokens(s: &str) -> i64 {
    (s.len() as i64 / 4).max(1)
}

fn mock(model: &str, req: &GenerateRequest) -> Reply {
    let last = req.messages.iter().rev().find(|m| m.role == "user").map_or("", |m| m.content.as_str());
    let text = format!("mock({model}): {last}");
    let input = req.messages.iter().map(|m| tokens(&m.content)).sum::<i64>() + req.system.as_deref().map_or(0, tokens);
    Reply { usage: Usage { input_tokens: input, output_tokens: tokens(&text) }, text, stop_reason: Some("end_turn".into()) }
}

fn replay(conn: &ProviderConnectionSpec, req: &GenerateRequest) -> Result<Reply, ApiError> {
    let hash = crate::prompt_sha256(req);
    let text: String = req.system.iter().chain(req.messages.iter().map(|m| &m.content)).cloned().collect::<Vec<_>>().join("\n");
    let e = conn
        .script
        .iter()
        .find(|e| e.prompt_sha256.as_deref() == Some(hash.as_str()))
        .or_else(|| conn.script.iter().find(|e| e.prompt_sha256.is_none() && e.contains.as_deref().is_some_and(|c| text.contains(c))))
        .ok_or_else(|| ApiError::new(ErrorCode::InvalidSpec, format!("replay: no recorded response for this prompt (promptSha256 {hash}); the agent's prompt changed or the trace is incomplete")))?;
    Ok(Reply {
        text: e.reply.clone(),
        stop_reason: e.stop_reason.clone().or_else(|| Some("end_turn".into())),
        usage: Usage { input_tokens: if e.input_tokens > 0 { e.input_tokens } else { tokens(&text) }, output_tokens: if e.output_tokens > 0 { e.output_tokens } else { tokens(&e.reply) } },
    })
}

fn endpoint(conn: &ProviderConnectionSpec, default: &str) -> String {
    let e = if conn.endpoint.is_empty() { default } else { &conn.endpoint };
    e.trim_end_matches('/').to_owned()
}

async fn send(rb: reqwest::RequestBuilder, provider: &str) -> Result<Value, ApiError> {
    let resp = rb.send().await.map_err(|e| {
        if e.is_timeout() {
            // the provider may have done (and billed) the work; we can't know (PRV-08)
            ApiError::new(ErrorCode::OutcomeUnknown, format!("{provider}: request timed out"))
        } else {
            ApiError::new(ErrorCode::ProviderUnavailable, format!("{provider}: {}", e.without_url()))
        }
    })?;
    let status = resp.status();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    if status.is_success() {
        return Ok(body);
    }
    // both providers put a human-readable message at error.message
    let msg: String = body["error"]["message"].as_str().unwrap_or("no error message").chars().take(300).collect();
    let code = match status.as_u16() {
        401 | 403 => ErrorCode::Forbidden,
        400 | 404 | 413 | 422 => ErrorCode::InvalidSpec,
        429 => ErrorCode::QuotaExceeded,
        _ => ErrorCode::ProviderUnavailable,
    };
    Err(ApiError::new(code, format!("{provider} returned {status}: {msg}")))
}

async fn openai(http: &reqwest::Client, conn: &ProviderConnectionSpec, model: &str, key: &str, req: &GenerateRequest, max_out: u32) -> Result<Reply, ApiError> {
    let mut messages: Vec<Value> = req.system.iter().map(|s| json!({"role": "system", "content": s})).collect();
    messages.extend(req.messages.iter().map(|m| json!({"role": m.role, "content": m.content})));
    let body = json!({"model": model, "messages": messages, "max_tokens": max_out});
    let url = format!("{}/chat/completions", endpoint(conn, "https://api.openai.com/v1"));
    let r = send(http.post(url).bearer_auth(key).json(&body), "openai-compatible").await?;
    let choice = &r["choices"][0];
    Ok(Reply {
        text: choice["message"]["content"].as_str().unwrap_or_default().to_owned(),
        stop_reason: choice["finish_reason"].as_str().map(str::to_owned),
        usage: Usage {
            input_tokens: r["usage"]["prompt_tokens"].as_i64().unwrap_or(0),
            output_tokens: r["usage"]["completion_tokens"].as_i64().unwrap_or(0),
        },
    })
}

/// Anthropic Messages API: `POST /v1/messages`, `x-api-key` + `anthropic-version` headers.
/// System text goes in the top-level `system` field, not in `messages`.
async fn anthropic(http: &reqwest::Client, conn: &ProviderConnectionSpec, model: &str, key: &str, req: &GenerateRequest, max_out: u32) -> Result<Reply, ApiError> {
    let mut system: Vec<&str> = req.system.iter().map(String::as_str).collect();
    system.extend(req.messages.iter().filter(|m| m.role == "system").map(|m| m.content.as_str()));
    let messages: Vec<Value> = req.messages.iter().filter(|m| m.role != "system").map(|m| json!({"role": m.role, "content": m.content})).collect();
    let mut body = json!({"model": model, "max_tokens": max_out, "messages": messages});
    if !system.is_empty() {
        body["system"] = system.join("\n\n").into();
    }
    let url = format!("{}/v1/messages", endpoint(conn, "https://api.anthropic.com"));
    let r = send(http.post(url).header("x-api-key", key).header("anthropic-version", "2023-06-01").json(&body), "anthropic").await?;
    // content is a list of blocks; concatenate the text ones. stop_reason may be "refusal".
    let text = r["content"].as_array().into_iter().flatten().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect::<String>();
    Ok(Reply {
        text,
        stop_reason: r["stop_reason"].as_str().map(str::to_owned),
        usage: Usage { input_tokens: r["usage"]["input_tokens"].as_i64().unwrap_or(0), output_tokens: r["usage"]["output_tokens"].as_i64().unwrap_or(0) },
    })
}
