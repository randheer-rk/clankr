//! Agent services, request/reply and topics (PRD §30), on the mailbox machinery: every message
//! is durable, delivered at least once, fenced by epoch and dead-lettered by policy.
//!
//!   POST /v1/namespaces/{ns}/services/{svc}/call     {payload, timeoutSeconds?, dedupKey?} -> {reply, requestId}
//!   POST /v1/namespaces/{ns}/services/{svc}/reply    {requestId, payload}   (the serving agent)
//!   POST /v1/namespaces/{ns}/topics/{topic}/publish  {payload, dedupKey?}  -> {delivered}
//!
//! A service's requests wait in queue `svc:<name>`; its agent's instances receive, ack and nack
//! them through the mailbox API. A call's request expires at the call's timeout, so nobody picks
//! up a request whose caller has given up; a late reply lands in `reply:<id>`, nobody reads
//! it, and it's dead-lettered at the service's retention.

use std::time::Duration;

use axum::extract::{Extension, Path, State};
use axum::http::HeaderMap;
use axum::routing::post;
use axum::{Json, Router};
use clankr_store::mailbox::SendOpts;
use clankr_types::agent::{admit_agent, admit_event_subscription, admit_mailbox, valid_topic};
use clankr_types::{ApiError, ErrorCode, RequestCtx};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::mailboxes::authorize;
use crate::{ApiResult, AppState, workload};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/namespaces/{ns}/services/{svc}/call", post(call))
        .route("/v1/namespaces/{ns}/services/{svc}/reply", post(reply))
        .route("/v1/namespaces/{ns}/topics/{topic}/publish", post(publish))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CallBody {
    payload: Value,
    timeout_seconds: Option<u64>,
    dedup_key: Option<String>,
}

async fn call(State(s): State<AppState>, Extension(ctx): Extension<RequestCtx>, headers: HeaderMap, Path((ns, svc)): Path<(String, String)>, Json(b): Json<CallBody>) -> ApiResult<Json<Value>> {
    let queue = format!("svc:{svc}");
    let (spec, sender) = authorize(&s, &ctx, &headers, &ns, &queue, true).await?;
    let timeout = b.timeout_seconds.unwrap_or(30).clamp(1, 300);
    let id = Uuid::new_v4().to_string();
    let reply_q = format!("reply:{id}");
    let opts = SendOpts { sender: &sender, correlation_id: Some(&id), dedup_key: b.dedup_key.as_deref(), ttl_secs: timeout as i64, max_deliveries: spec.max_deliveries, reply_to: Some(&reply_q) };
    // a retried call with the same dedup key waits on the original request's reply
    let (req, _) = s.store.mailbox_send(&ns, &queue, &b.payload, opts).await?;
    let reply_q = req.reply_to.clone().unwrap_or(reply_q);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout);
    let mut wake = s.mailbox_events.clone();
    loop {
        wake.borrow_and_update();
        if let Some(m) = s.store.mailbox_receive(&ns, &reply_q, "caller", 1, 60.0).await?.pop() {
            s.store.mailbox_ack(&ns, &reply_q, m.id, m.lease_epoch).await?;
            return Ok(Json(json!({"reply": m.payload, "requestId": req.id, "from": m.sender})));
        }
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return Err(ApiError::new(
                ErrorCode::OutcomeUnknown,
                format!("service {svc} didn't reply within {timeout}s; an untaken request expires unprocessed, but one already being handled may still finish"),
            )
            .into());
        }
        let _ = tokio::time::timeout(left.min(Duration::from_secs(1)), wake.changed()).await;
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReplyBody {
    request_id: Uuid,
    payload: Value,
}

async fn reply(State(s): State<AppState>, Extension(ctx): Extension<RequestCtx>, headers: HeaderMap, Path((ns, svc)): Path<(String, String)>, Json(b): Json<ReplyBody>) -> ApiResult<Json<Value>> {
    let queue = format!("svc:{svc}");
    let (spec, sender) = authorize(&s, &ctx, &headers, &ns, &queue, false).await?;
    let req = s.store.mailbox_get(&ns, &queue, b.request_id).await?.ok_or_else(|| ApiError::new(ErrorCode::NotFound, format!("no request {} for service {svc}", b.request_id)))?;
    let to = req.reply_to.ok_or_else(|| ApiError::new(ErrorCode::InvalidSpec, "that message was one-way; it takes no reply").field("requestId"))?;
    // one reply per request: a handler retried after replying doesn't reply twice
    let opts = SendOpts { sender: &sender, correlation_id: req.correlation_id.as_deref(), dedup_key: Some("reply"), ttl_secs: spec.retention_seconds, max_deliveries: 1, reply_to: None };
    let (_, duplicate) = s.store.mailbox_send(&ns, &to, &b.payload, opts).await?;
    Ok(Json(json!({"duplicate": duplicate})))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PublishBody {
    payload: Value,
    dedup_key: Option<String>,
}

async fn publish(State(s): State<AppState>, Extension(ctx): Extension<RequestCtx>, headers: HeaderMap, Path((ns, topic)): Path<(String, String)>, Json(b): Json<PublishBody>) -> ApiResult<Json<Value>> {
    if !valid_topic(&topic) {
        return Err(ApiError::new(ErrorCode::InvalidSpec, "topic: lowercase letters, digits, '.', '-', '_'").into());
    }
    let sender = match workload(&s, &ctx, &headers, &ns, None).await? {
        Some(c) => {
            if !admit_agent(&s.store.get(&ns, "Agent", &c.agent).await?.spec)?.publishes.iter().any(|t| *t == topic) {
                return Err(ApiError::new(ErrorCode::Forbidden, format!("agent {}/{} doesn't list topic {topic} in publishes", c.ns, c.agent)).into());
            }
            format!("agent/{}", c.agent)
        }
        None => crate::contexts::writer(&ctx, &headers, None),
    };
    // a copy per subscription, each in its own durable mailbox
    let (subs, _) = s.store.list("EventSubscription", Some(&ns), &Default::default()).await?;
    let mut delivered = vec![];
    for sub in subs {
        let Ok(spec) = admit_event_subscription(&sub.spec) else { continue };
        if spec.topic != topic {
            continue;
        }
        let mb = admit_mailbox(&s.store.get(&ns, "AgentMailbox", &spec.mailbox).await?.spec)?;
        let opts = SendOpts { sender: &sender, correlation_id: None, dedup_key: b.dedup_key.as_deref(), ttl_secs: mb.retention_seconds, max_deliveries: mb.max_deliveries, reply_to: None };
        s.store.mailbox_send(&ns, &spec.mailbox, &b.payload, opts).await?;
        delivered.push(sub.metadata.name);
    }
    Ok(Json(json!({"delivered": delivered.len(), "subscriptions": delivered})))
}
