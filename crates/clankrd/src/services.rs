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
    let (_, sender) = authorize(&s, &ctx, &headers, &ns, &format!("svc:{svc}"), true).await?;
    let timeout = b.timeout_seconds.unwrap_or(30).clamp(1, 300);
    let req = service_request(&s, &ns, &svc, &sender, &b.payload, timeout, b.dedup_key.as_deref()).await?;
    Ok(Json(service_reply(&s, &ns, &svc, req, timeout).await?))
}

/// A request in flight: where its reply will land.
pub(crate) struct Pending {
    pub id: Uuid,
    pub reply_queue: String,
}

/// Enqueues a request for `svc` as `sender` (already authorized). Shared by the API and by
/// chat connectors, which call as the chat they bridge.
pub(crate) async fn service_request(s: &AppState, ns: &str, svc: &str, sender: &str, payload: &Value, timeout: u64, dedup_key: Option<&str>) -> Result<Pending, ApiError> {
    let queue = format!("svc:{svc}");
    let spec = clankr_types::agent::admit_agent_service(&s.store.get(ns, "AgentService", svc).await?.spec)?;
    let id = Uuid::new_v4().to_string();
    let reply_q = format!("reply:{id}");
    let opts = SendOpts { sender, correlation_id: Some(&id), dedup_key, ttl_secs: timeout as i64, max_deliveries: spec.max_deliveries, reply_to: Some(&reply_q) };
    // a retried call with the same dedup key waits on the original request's reply
    let (req, _) = s.store.mailbox_send(ns, &queue, payload, opts).await?;
    Ok(Pending { reply_queue: req.reply_to.clone().unwrap_or(reply_q), id: req.id })
}

/// Waits up to `timeout` seconds for the reply to a request.
pub(crate) async fn service_reply(s: &AppState, ns: &str, svc: &str, req: Pending, timeout: u64) -> Result<Value, ApiError> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout);
    let mut wake = s.mailbox_events.clone();
    loop {
        wake.borrow_and_update();
        if let Some(m) = s.store.mailbox_receive(ns, &req.reply_queue, "caller", 1, 60.0).await?.pop() {
            s.store.mailbox_ack(ns, &req.reply_queue, m.id, m.lease_epoch).await?;
            return Ok(json!({"reply": m.payload, "requestId": req.id, "from": m.sender}));
        }
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return Err(ApiError::new(
                ErrorCode::OutcomeUnknown,
                format!("service {svc} didn't reply within {timeout}s; an untaken request expires unprocessed, but one already being handled may still finish"),
            ));
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
            if !admit_agent(&s.store.get(&ns, "Agent", &c.agent).await?.spec)?.publishes.contains(&topic) {
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
    // EventTriggers: a run per published message (a republish with the same dedup key starts none)
    let (triggers, _) = s.store.list("EventTrigger", Some(&ns), &Default::default()).await?;
    let event_key = b.dedup_key.clone().unwrap_or_else(|| Uuid::new_v4().to_string());
    let mut runs = vec![];
    for t in triggers {
        let Ok(spec) = clankr_types::schedule::admit_event_trigger(&t.spec) else { continue };
        if spec.topic != topic {
            continue;
        }
        let mut input = if spec.input.is_object() { spec.input.clone() } else { json!({}) };
        input["event"] = b.payload.clone();
        input["topic"] = json!(topic);
        let tctx = RequestCtx { actor: format!("eventtrigger/{ns}/{} for {sender}", t.metadata.name), request_id: ctx.request_id.clone() };
        let run = s.engine.start(&tctx, &ns, &spec.workflow, input, Some(&format!("trigger/{}/{event_key}", t.metadata.name))).await?;
        runs.push(json!({"trigger": t.metadata.name, "run": run.run_id}));
    }
    Ok(Json(json!({"delivered": delivered.len(), "subscriptions": delivered, "runs": runs})))
}
