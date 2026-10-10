//! Workflow execution and worker task API (PRD §35).
//!
//!   POST /v1/namespaces/{ns}/workflows/{name}/runs     start  {input, idempotencyKey?} (or Idempotency-Key header)
//!   GET  /v1/namespaces/{ns}/workflows/{name}/runs     list runs of one workflow
//!   GET  /v1/namespaces/{ns}/runs                      list runs
//!   GET  /v1/namespaces/{ns}/runs/{id}
//!   GET  /v1/namespaces/{ns}/runs/{id}/history
//!   POST /v1/namespaces/{ns}/runs/{id}/signal          {signal, payload}
//!   POST /v1/namespaces/{ns}/runs/{id}/cancel          {reason}
//!   POST /v1/namespaces/{ns}/tasks/lease               {queue, owner, ttlSeconds?, waitSeconds?} -> 200 task | 204
//!   POST /v1/namespaces/{ns}/tasks/heartbeat           {runId, task, epoch, ttlSeconds?}
//!   POST /v1/namespaces/{ns}/tasks/complete            {runId, task, epoch, result}
//!   POST /v1/namespaces/{ns}/tasks/fail                {runId, task, epoch, error, retryable}

use std::time::Duration;

use axum::extract::{Extension, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use clankr_engine::{HistoryEntry, RunView};
use clankr_types::{ApiError, ErrorCode, RequestCtx};
use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

use crate::{ApiResult, AppState, workload};

/// Nodes may only touch `agent/<name>` queues, under that agent's workload identity.
async fn authorize_queue(s: &AppState, ctx: &RequestCtx, headers: &HeaderMap, ns: &str, queue: &str) -> Result<(), ApiError> {
    if crate::node_of(ctx).is_none() {
        return Ok(());
    }
    match queue.strip_prefix("agent/") {
        Some(agent) => workload(s, ctx, headers, ns, Some(agent)).await.map(drop),
        None => Err(ApiError::new(ErrorCode::Forbidden, format!("nodes can't serve queue {queue}"))),
    }
}

async fn authorize_task(s: &AppState, ctx: &RequestCtx, headers: &HeaderMap, ns: &str, run_id: Uuid, task: &str) -> Result<(), ApiError> {
    if crate::node_of(ctx).is_none() {
        return Ok(());
    }
    let q = s.engine.task_queue(ns, run_id, task).await?;
    authorize_queue(s, ctx, headers, ns, &q).await
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/namespaces/{ns}/workflows/{name}/runs", post(start).get(list_for_workflow))
        .route("/v1/namespaces/{ns}/runs", get(list))
        .route("/v1/namespaces/{ns}/runs/{id}", get(read))
        .route("/v1/namespaces/{ns}/runs/{id}/history", get(history))
        .route("/v1/namespaces/{ns}/runs/{id}/signal", post(signal))
        .route("/v1/namespaces/{ns}/runs/{id}/cancel", post(cancel))
        .route("/v1/namespaces/{ns}/tasks/lease", post(lease))
        .route("/v1/namespaces/{ns}/tasks/heartbeat", post(heartbeat))
        .route("/v1/namespaces/{ns}/tasks/complete", post(complete))
        .route("/v1/namespaces/{ns}/tasks/fail", post(fail))
}

fn run_id(s: &str) -> Result<Uuid, ApiError> {
    s.parse().map_err(|_| ApiError::new(ErrorCode::NotFound, format!("run {s:?} not found")))
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct StartBody {
    #[serde(default)]
    input: Value,
    idempotency_key: Option<String>,
}

async fn start(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Path((ns, name)): Path<(String, String)>,
    headers: HeaderMap,
    Json(b): Json<StartBody>,
) -> ApiResult<(StatusCode, Json<RunView>)> {
    let key = b.idempotency_key.or_else(|| headers.get("idempotency-key").and_then(|v| v.to_str().ok()).map(str::to_owned));
    let run = s.engine.start(&ctx, &ns, &name, b.input, key.as_deref()).await?;
    Ok((StatusCode::CREATED, Json(run)))
}

#[derive(Deserialize)]
struct ListQuery {
    limit: Option<i64>,
}

async fn list(State(s): State<AppState>, Path(ns): Path<String>, Query(q): Query<ListQuery>) -> ApiResult<Json<Vec<RunView>>> {
    Ok(Json(s.engine.list(&ns, None, q.limit.unwrap_or(50).clamp(1, 500)).await?))
}

async fn list_for_workflow(State(s): State<AppState>, Path((ns, name)): Path<(String, String)>, Query(q): Query<ListQuery>) -> ApiResult<Json<Vec<RunView>>> {
    Ok(Json(s.engine.list(&ns, Some(&name), q.limit.unwrap_or(50).clamp(1, 500)).await?))
}

async fn read(State(s): State<AppState>, Path((ns, id)): Path<(String, String)>) -> ApiResult<Json<RunView>> {
    Ok(Json(s.engine.get(&ns, run_id(&id)?).await?))
}

async fn history(State(s): State<AppState>, Path((ns, id)): Path<(String, String)>) -> ApiResult<Json<Vec<HistoryEntry>>> {
    Ok(Json(s.engine.history(&ns, run_id(&id)?).await?))
}

#[derive(Deserialize)]
struct SignalBody {
    signal: String,
    #[serde(default)]
    payload: Value,
}

async fn signal(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Path((ns, id)): Path<(String, String)>,
    Json(b): Json<SignalBody>,
) -> ApiResult<Json<RunView>> {
    Ok(Json(s.engine.signal(&ctx, &ns, run_id(&id)?, &b.signal, b.payload).await?))
}

#[derive(Deserialize, Default)]
struct CancelBody {
    #[serde(default)]
    reason: String,
}

async fn cancel(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Path((ns, id)): Path<(String, String)>,
    Json(b): Json<CancelBody>,
) -> ApiResult<Json<RunView>> {
    let reason = if b.reason.is_empty() { format!("cancelled by {}", ctx.actor) } else { b.reason };
    Ok(Json(s.engine.cancel(&ctx, &ns, run_id(&id)?, &reason).await?))
}

fn ttl(secs: Option<u64>) -> Duration {
    Duration::from_secs(secs.unwrap_or(60).clamp(5, 3600))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LeaseBody {
    queue: String,
    owner: String,
    ttl_seconds: Option<u64>,
    /// Long-poll: wait up to this long for a task (max 30s).
    wait_seconds: Option<u64>,
}

async fn lease(State(s): State<AppState>, Extension(ctx): Extension<RequestCtx>, headers: HeaderMap, Path(ns): Path<String>, Json(b): Json<LeaseBody>) -> ApiResult<Response> {
    authorize_queue(&s, &ctx, &headers, &ns, &b.queue).await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(b.wait_seconds.unwrap_or(0).min(30));
    let mut wake = s.tasks.clone();
    loop {
        wake.borrow_and_update();
        if let Some(t) = s.engine.lease(&ns, &b.queue, &b.owner, ttl(b.ttl_seconds)).await? {
            return Ok(Json(t).into_response());
        }
        // a retry whose backoff elapses doesn't notify; re-poll at least every second
        let wait = deadline.saturating_duration_since(tokio::time::Instant::now()).min(Duration::from_secs(1));
        if wait.is_zero() {
            return Ok(StatusCode::NO_CONTENT.into_response());
        }
        let _ = tokio::time::timeout(wait, wake.changed()).await;
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TaskRef {
    run_id: Uuid,
    task: String,
    epoch: i64,
    ttl_seconds: Option<u64>,
    #[serde(default)]
    result: Value,
    #[serde(default)]
    error: String,
    #[serde(default)]
    retryable: bool,
}

async fn heartbeat(State(s): State<AppState>, Extension(ctx): Extension<RequestCtx>, headers: HeaderMap, Path(ns): Path<String>, Json(t): Json<TaskRef>) -> ApiResult<Json<Value>> {
    authorize_task(&s, &ctx, &headers, &ns, t.run_id, &t.task).await?;
    let exp = s.engine.heartbeat(&ns, t.run_id, &t.task, t.epoch, ttl(t.ttl_seconds)).await?;
    Ok(Json(serde_json::json!({ "leaseExpires": exp })))
}

async fn complete(State(s): State<AppState>, Extension(ctx): Extension<RequestCtx>, headers: HeaderMap, Path(ns): Path<String>, Json(t): Json<TaskRef>) -> ApiResult<StatusCode> {
    authorize_task(&s, &ctx, &headers, &ns, t.run_id, &t.task).await?;
    s.engine.complete(&ns, t.run_id, &t.task, t.epoch, t.result).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn fail(State(s): State<AppState>, Extension(ctx): Extension<RequestCtx>, headers: HeaderMap, Path(ns): Path<String>, Json(t): Json<TaskRef>) -> ApiResult<StatusCode> {
    authorize_task(&s, &ctx, &headers, &ns, t.run_id, &t.task).await?;
    s.engine.fail(&ns, t.run_id, &t.task, t.epoch, &t.error, t.retryable).await?;
    Ok(StatusCode::NO_CONTENT)
}
