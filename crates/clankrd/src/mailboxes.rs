//! Mailbox API (PRD §30).
//!
//!   POST /v1/namespaces/{ns}/mailboxes/{mb}/messages              send {payload, correlationId?, dedupKey?, ttlSeconds?}
//!   GET  /v1/namespaces/{ns}/mailboxes/{mb}/messages?state=&limit= (owner) inspect, incl. dead letters
//!   POST /v1/namespaces/{ns}/mailboxes/{mb}/receive               {owner, max?, visibilitySeconds?, waitSeconds?}
//!   POST /v1/namespaces/{ns}/mailboxes/{mb}/ack                   {id, epoch}
//!   POST /v1/namespaces/{ns}/mailboxes/{mb}/nack                  {id, epoch, delaySeconds?, reason?}
//!   POST /v1/namespaces/{ns}/mailboxes/{mb}/messages/{id}/requeue (owner) dead letter -> ready

use std::time::Duration;

use axum::extract::{Extension, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use clankr_store::mailbox::{Message, SendOpts};
use clankr_types::agent::{MailboxSpec, admit_agent, admit_mailbox};
use clankr_types::{ApiError, ErrorCode, RequestCtx};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{ApiResult, AppState, node_of, workload};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/namespaces/{ns}/mailboxes/{mb}/messages", post(send).get(list))
        .route("/v1/namespaces/{ns}/mailboxes/{mb}/receive", post(receive))
        .route("/v1/namespaces/{ns}/mailboxes/{mb}/ack", post(ack))
        .route("/v1/namespaces/{ns}/mailboxes/{mb}/nack", post(nack))
        .route("/v1/namespaces/{ns}/mailboxes/{mb}/messages/{id}/requeue", post(requeue))
}

/// The mailbox must exist; a node may only use it for an agent whose spec lists it.
/// Returns the mailbox spec and the authenticated sender name (CL-30-10).
async fn authorize(s: &AppState, ctx: &RequestCtx, headers: &HeaderMap, ns: &str, mb: &str) -> Result<(MailboxSpec, String), ApiError> {
    let spec = admit_mailbox(&s.store.get(ns, "AgentMailbox", mb).await?.spec)?;
    match workload(s, ctx, headers, ns, None).await? {
        Some(c) => {
            let agent = admit_agent(&s.store.get(ns, "Agent", &c.agent).await?.spec)?;
            if !agent.mailboxes.iter().any(|m| m == mb) {
                return Err(ApiError::new(ErrorCode::Forbidden, format!("agent {}/{} isn't bound to mailbox {mb}", c.ns, c.agent)));
            }
            Ok((spec, format!("agent/{}", c.agent)))
        }
        None => Ok((spec, crate::contexts::writer(ctx, headers, None))),
    }
}

fn owner_only(ctx: &RequestCtx) -> Result<(), ApiError> {
    if node_of(ctx).is_some() { Err(ApiError::new(ErrorCode::Forbidden, "owner only")) } else { Ok(()) }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SendBody {
    payload: Value,
    correlation_id: Option<String>,
    dedup_key: Option<String>,
    ttl_seconds: Option<i64>,
}

async fn send(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    headers: HeaderMap,
    Path((ns, mb)): Path<(String, String)>,
    Json(b): Json<SendBody>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let (spec, sender) = authorize(&s, &ctx, &headers, &ns, &mb).await?;
    let ttl = b.ttl_seconds.unwrap_or(spec.retention_seconds).clamp(1, spec.retention_seconds);
    let opts = SendOpts { sender: &sender, correlation_id: b.correlation_id.as_deref(), dedup_key: b.dedup_key.as_deref(), ttl_secs: ttl, max_deliveries: spec.max_deliveries };
    let (m, duplicate) = s.store.mailbox_send(&ns, &mb, &b.payload, opts).await?;
    let code = if duplicate { StatusCode::OK } else { StatusCode::CREATED };
    Ok((code, Json(json!({ "message": m, "duplicate": duplicate }))))
}

#[derive(Deserialize)]
struct ListQuery {
    state: Option<String>,
    limit: Option<i64>,
}

async fn list(State(s): State<AppState>, Extension(ctx): Extension<RequestCtx>, Path((ns, mb)): Path<(String, String)>, Query(q): Query<ListQuery>) -> ApiResult<Json<Vec<Message>>> {
    owner_only(&ctx)?;
    s.store.get(&ns, "AgentMailbox", &mb).await?;
    Ok(Json(s.store.mailbox_list(&ns, &mb, q.state.as_deref(), q.limit.unwrap_or(100).clamp(1, 1000)).await?))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReceiveBody {
    owner: String,
    max: Option<i64>,
    visibility_seconds: Option<u64>,
    /// Long-poll up to this long (max 30s) when nothing is ready.
    wait_seconds: Option<u64>,
}

async fn receive(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    headers: HeaderMap,
    Path((ns, mb)): Path<(String, String)>,
    Json(b): Json<ReceiveBody>,
) -> ApiResult<Json<Vec<Message>>> {
    authorize(&s, &ctx, &headers, &ns, &mb).await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(b.wait_seconds.unwrap_or(0).min(30));
    let mut wake = s.mailbox_events.clone();
    loop {
        wake.borrow_and_update();
        let got = s.store.mailbox_receive(&ns, &mb, &b.owner, b.max.unwrap_or(10).clamp(1, 100), b.visibility_seconds.unwrap_or(60).clamp(1, 3600) as f64).await?;
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if !got.is_empty() || left.is_zero() {
            return Ok(Json(got));
        }
        let _ = tokio::time::timeout(left.min(Duration::from_secs(1)), wake.changed()).await;
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AckBody {
    id: Uuid,
    epoch: i64,
    #[serde(default)]
    delay_seconds: f64,
    #[serde(default)]
    reason: String,
}

async fn ack(State(s): State<AppState>, Extension(ctx): Extension<RequestCtx>, headers: HeaderMap, Path((ns, mb)): Path<(String, String)>, Json(b): Json<AckBody>) -> ApiResult<StatusCode> {
    authorize(&s, &ctx, &headers, &ns, &mb).await?;
    s.store.mailbox_ack(&ns, &mb, b.id, b.epoch).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn nack(State(s): State<AppState>, Extension(ctx): Extension<RequestCtx>, headers: HeaderMap, Path((ns, mb)): Path<(String, String)>, Json(b): Json<AckBody>) -> ApiResult<Json<Message>> {
    authorize(&s, &ctx, &headers, &ns, &mb).await?;
    Ok(Json(s.store.mailbox_nack(&ns, &mb, b.id, b.epoch, b.delay_seconds.clamp(0.0, 86_400.0), &b.reason).await?))
}

async fn requeue(State(s): State<AppState>, Extension(ctx): Extension<RequestCtx>, Path((ns, mb, id)): Path<(String, String, Uuid)>) -> ApiResult<Json<Message>> {
    owner_only(&ctx)?;
    let spec = admit_mailbox(&s.store.get(&ns, "AgentMailbox", &mb).await?.spec)?;
    let m = s.store.mailbox_requeue(&ns, &mb, id, spec.retention_seconds).await?;
    s.store.audit_in_tx_simple(&ctx, &format!("mailbox requeue {mb}"), &id.to_string()).await?;
    Ok(Json(m))
}
