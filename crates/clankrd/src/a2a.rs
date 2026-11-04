//! A2A (Agent2Agent v1.0) server side (ADR 0038): every AgentService is an A2A agent.
//!
//!   GET  /a2a/{ns}/{svc}/.well-known/a2a/agent-card.json   the agent card
//!   POST /a2a/{ns}/{svc}                                   JSON-RPC: SendMessage, GetTask
//!
//! A message becomes a request to the service (sender `a2a:<principal>`); the reply comes
//! back as a Message. When the caller asks to return immediately, or the reply doesn't arrive
//! in time, the request is a Task whose id is the request id, and GetTask looks for its reply.
//! Both routes take a bearer token like the rest of the API: cards are not public.

use axum::extract::{Extension, Path, State};
use axum::http::HeaderMap;
use axum::routing::{get, post};
use axum::{Json, Router};
use clankr_gateway::a2a::{PROTOCOL_VERSION, data_of, parts_for, text_of};
use clankr_types::agent::admit_agent_service;
use clankr_types::{ApiError, ErrorCode, RequestCtx};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::services::{service_reply, service_request};
use crate::{ApiResult, AppState};

pub fn routes() -> Router<AppState> {
    Router::new().route("/a2a/{ns}/{svc}/.well-known/a2a/agent-card.json", get(card)).route("/a2a/{ns}/{svc}", post(rpc))
}

/// Where this control plane is reachable, as the caller reached it.
fn base_url(headers: &HeaderMap) -> String {
    if let Ok(u) = std::env::var("CLANKR_PUBLIC_URL") {
        return u.trim_end_matches('/').to_owned();
    }
    let scheme = headers.get("x-forwarded-proto").and_then(|v| v.to_str().ok()).unwrap_or("https");
    let host = headers.get("host").and_then(|v| v.to_str().ok()).unwrap_or("localhost");
    format!("{scheme}://{host}")
}

async fn card(State(s): State<AppState>, headers: HeaderMap, Path((ns, svc)): Path<(String, String)>) -> ApiResult<Json<Value>> {
    let r = s.store.get(&ns, "AgentService", &svc).await?;
    let spec = admit_agent_service(&r.spec)?;
    let url = format!("{}/a2a/{ns}/{svc}", base_url(&headers));
    let skills: Vec<Value> = if spec.skills.is_empty() {
        vec![json!({"id": svc, "name": svc, "description": spec.description, "tags": []})]
    } else {
        spec.skills.iter().map(|k| json!({"id": k.id, "name": k.name, "description": k.description, "tags": k.tags})).collect()
    };
    Ok(Json(json!({
        "name": svc,
        "description": spec.description,
        "url": url,
        "version": r.metadata.generation.to_string(),
        "protocolVersion": PROTOCOL_VERSION,
        "provider": {"name": "clankr", "url": base_url(&headers)},
        "capabilities": {"streaming": false, "pushNotifications": false, "extendedAgentCard": false},
        "defaultInputModes": ["text/plain", "application/json"],
        "defaultOutputModes": ["text/plain", "application/json"],
        "securitySchemes": {"bearer": {"type": "http", "scheme": "bearer"}},
        "security": [{"bearer": []}],
        "skills": skills,
        "supportedInterfaces": [{"protocol": "json-rpc", "url": url}],
    })))
}

fn rpc_error(id: &Value, code: i64, message: impl Into<String>) -> Json<Value> {
    Json(json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message.into()}}))
}

async fn rpc(State(s): State<AppState>, Extension(ctx): Extension<RequestCtx>, Path((ns, svc)): Path<(String, String)>, Json(req): Json<Value>) -> ApiResult<Json<Value>> {
    let id = req["id"].clone();
    let params = &req["params"];
    let result = match req["method"].as_str() {
        Some("SendMessage") => send_message(&s, &ctx, &ns, &svc, params).await,
        Some("GetTask") => get_task(&s, &ns, &svc, params).await,
        Some(m) => return Ok(rpc_error(&id, -32601, format!("method {m:?} isn't supported; SendMessage and GetTask are"))),
        None => return Ok(rpc_error(&id, -32600, "no method")),
    };
    Ok(match result {
        Ok(v) => Json(json!({"jsonrpc": "2.0", "id": id, "result": v})),
        Err(e) => match e.code {
            ErrorCode::NotFound => rpc_error(&id, -32001, e.message),
            ErrorCode::InvalidSpec => rpc_error(&id, -32602, e.message),
            ErrorCode::Forbidden => return Err(e.into()),
            _ => rpc_error(&id, -32603, e.message),
        },
    })
}

fn task(id: &str, state: &str, message: Option<Value>) -> Value {
    let mut t = json!({"id": id, "status": {"state": state, "timestamp": chrono::Utc::now().to_rfc3339()}});
    if let Some(m) = message {
        t["status"]["message"] = m;
    }
    t
}

fn agent_message(reply: &Value, in_reply_to: &Value) -> Value {
    let mut m = json!({"messageId": Uuid::new_v4().to_string(), "role": "ROLE_AGENT", "parts": parts_for(reply)});
    if let Some(c) = in_reply_to["contextId"].as_str() {
        m["contextId"] = json!(c);
    }
    m
}

async fn send_message(s: &AppState, ctx: &RequestCtx, ns: &str, svc: &str, params: &Value) -> Result<Value, ApiError> {
    let msg = &params["message"];
    if !msg["parts"].is_array() {
        return Err(ApiError::new(ErrorCode::InvalidSpec, "params.message.parts is required"));
    }
    let payload = json!({
        "text": text_of(msg), "data": data_of(msg), "from": format!("a2a:{}", ctx.actor), "platform": "a2a",
        "messageId": msg["messageId"], "contextId": msg["contextId"],
    });
    let sender = format!("a2a:{}", ctx.actor);
    let timeout = 300;
    let dedup = msg["messageId"].as_str().map(|m| format!("a2a:{}:{m}", ctx.actor));
    let req = service_request(s, ns, svc, &sender, &payload, timeout, dedup.as_deref()).await?;
    let id = req.id.to_string();
    if params["configuration"]["returnImmediately"] == true {
        return Ok(task(&id, "TASK_STATE_SUBMITTED", None));
    }
    // a bounded wait keeps the HTTP call short; the Task carries on in the queue
    match service_reply(s, ns, svc, req, 60).await {
        Ok(r) => Ok(agent_message(&r["reply"], msg)),
        Err(e) if e.code == ErrorCode::OutcomeUnknown => Ok(task(&id, "TASK_STATE_WORKING", None)),
        Err(e) => Err(e),
    }
}

async fn get_task(s: &AppState, ns: &str, svc: &str, params: &Value) -> Result<Value, ApiError> {
    let id: Uuid = params["id"].as_str().and_then(|i| i.parse().ok()).ok_or_else(|| ApiError::new(ErrorCode::NotFound, "task not found"))?;
    let req = s.store.mailbox_get(ns, &format!("svc:{svc}"), id).await?.ok_or_else(|| ApiError::new(ErrorCode::NotFound, "task not found"))?;
    // the reply, if it's there
    if let Some(q) = &req.reply_to
        && let Some(m) = s.store.mailbox_receive(ns, q, "a2a", 1, 60.0).await?.pop()
    {
        s.store.mailbox_ack(ns, q, m.id, m.lease_epoch).await?;
        return Ok(task(&id.to_string(), "TASK_STATE_COMPLETED", Some(agent_message(&m.payload, &Value::Null))));
    }
    // else the request's own state says what's happening
    Ok(match req.state.as_str() {
        "dead" => task(&id.to_string(), "TASK_STATE_FAILED", Some(json!({"role": "ROLE_AGENT", "parts": [{"text": req.dead_reason.unwrap_or_else(|| "not handled".into())}]}))),
        "leased" | "acked" => task(&id.to_string(), "TASK_STATE_WORKING", None),
        _ => task(&id.to_string(), "TASK_STATE_SUBMITTED", None),
    })
}
