//! Secrets (write-only) and the model gateway.
//!
//!   PUT    /v1/namespaces/{ns}/secrets/{name}   {data: {key: value}}  -> metadata (never values)
//!   GET    /v1/namespaces/{ns}/secrets[/{name}]                      -> metadata
//!   DELETE /v1/namespaces/{ns}/secrets/{name}
//!   POST   /v1/namespaces/{ns}/models/{claim}/generate                -> text, usage, provenance

use std::collections::BTreeMap;

use axum::extract::{Extension, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use clankr_gateway::{GenerateRequest, GenerateResponse};
use clankr_store::secrets::SecretMeta;
use clankr_types::RequestCtx;
use serde::Deserialize;

use clankr_gateway::tools::{Outcome, Placement};
use crate::contexts::writer;
use crate::{ApiResult, AppState};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/namespaces/{ns}/secrets", get(list))
        .route("/v1/namespaces/{ns}/secrets/{name}", get(meta).put(put).delete(remove))
        .route("/v1/namespaces/{ns}/models/{claim}/generate", post(generate))
        .route("/v1/namespaces/{ns}/toolclaims/{claim}/call", post(call_tool))
        .route("/v1/namespaces/{ns}/toolcalls/{id}/complete", post(complete_tool))
}

#[derive(Deserialize)]
struct PutBody {
    data: BTreeMap<String, String>,
}

async fn put(State(s): State<AppState>, Extension(ctx): Extension<RequestCtx>, Path((ns, name)): Path<(String, String)>, Json(b): Json<PutBody>) -> ApiResult<Json<SecretMeta>> {
    Ok(Json(s.store.secret_put(&ctx, &s.master, &ns, &name, &b.data).await?))
}

async fn meta(State(s): State<AppState>, Path((ns, name)): Path<(String, String)>) -> ApiResult<Json<SecretMeta>> {
    Ok(Json(s.store.secret_meta(&ns, &name).await?))
}

async fn list(State(s): State<AppState>, Path(ns): Path<String>) -> ApiResult<Json<Vec<SecretMeta>>> {
    Ok(Json(s.store.secret_list(&ns).await?))
}

async fn remove(State(s): State<AppState>, Extension(ctx): Extension<RequestCtx>, Path((ns, name)): Path<(String, String)>) -> ApiResult<StatusCode> {
    s.store.secret_delete(&ctx, &ns, &name).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn generate(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    headers: HeaderMap,
    Path((ns, claim)): Path<(String, String)>,
    Json(req): Json<GenerateRequest>,
) -> ApiResult<Json<GenerateResponse>> {
    let claims = crate::workload(&s, &ctx, &headers, &ns, None).await?;
    let ctx = RequestCtx { actor: writer(&ctx, &headers, claims.as_ref()), ..ctx };
    Ok(Json(s.gateway.generate(&ctx, &ns, &claim, req).await?))
}

async fn call_tool(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    headers: HeaderMap,
    Path((ns, claim)): Path<(String, String)>,
    Json(call): Json<clankr_gateway::tools::ToolCall>,
) -> ApiResult<axum::response::Response> {
    use axum::response::IntoResponse;
    let claims = crate::workload(&s, &ctx, &headers, &ns, None).await?;
    // the node already checks this; re-check so a compromised node can't borrow another agent's tools
    if let Some(w) = &claims {
        let agent = clankr_types::agent::admit_agent(&s.store.get(&ns, "Agent", &w.agent).await?.spec)?;
        if !agent.tools.contains(&claim) {
            return Err(clankr_types::ApiError::new(clankr_types::ErrorCode::Forbidden, format!("agent {} is not bound to ToolClaim {claim:?}", w.agent)).into());
        }
    }
    // a node that can run containers takes container tools itself (ADR 0028)
    // only trusted executors may (their reported results are recorded as the outcome): nodes
    // under a workload identity, or a standalone node running with the break-glass owner token
    let trusted = claims.is_some() || ctx.actor == "owner";
    let at = if trusted && headers.get("x-clankr-executor").is_some_and(|v| v == "node") { Placement::Caller } else { Placement::Here };
    let ctx = RequestCtx { actor: writer(&ctx, &headers, claims.as_ref()), ..ctx };
    Ok(match s.gateway.call_tool_at(&ctx, &ns, &claim, call, at).await? {
        Outcome::Done(r) => Json(r).into_response(),
        Outcome::Delegated(d) => (axum::http::StatusCode::ACCEPTED, Json(serde_json::json!({"delegate": d}))).into_response(),
    })
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompleteBody {
    #[serde(default)]
    output: Option<serde_json::Value>,
    #[serde(default)]
    error: Option<String>,
    duration_ms: i64,
}

/// The node reporting a tool call it ran for one of its agents.
async fn complete_tool(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    headers: HeaderMap,
    Path((ns, id)): Path<(String, i64)>,
    Json(b): Json<CompleteBody>,
) -> ApiResult<Json<clankr_gateway::tools::ToolResult>> {
    let claims = crate::workload(&s, &ctx, &headers, &ns, None).await?;
    let ctx = RequestCtx { actor: writer(&ctx, &headers, claims.as_ref()), ..ctx };
    let result = match b.error {
        Some(e) => Err(e),
        None => Ok(b.output.unwrap_or(serde_json::Value::Null)),
    };
    Ok(Json(s.gateway.complete_delegated(&ctx, &ns, id, result, b.duration_ms).await?))
}
