//! Context API over a bound ContextClaim (PRD §29).
//!
//!   GET    /v1/namespaces/{ns}/contexts/{claim}/keys?prefix=&limit=
//!   GET    /v1/namespaces/{ns}/contexts/{claim}/keys/{*key}
//!   PUT    /v1/namespaces/{ns}/contexts/{claim}/keys/{*key}   {value, ifVersion?}  (ifVersion 0 = create only)
//!   DELETE /v1/namespaces/{ns}/contexts/{claim}/keys/{*key}?ifVersion=
//!   GET    /v1/namespaces/{ns}/contexts/{claim}/keys?asOf=<rfc3339>|snapshot=<name>   context back then
//!   GET    /v1/namespaces/{ns}/contexts/{claim}/snapshots
//!   POST   /v1/namespaces/{ns}/contexts/{claim}/snapshots                {name}
//!   POST   /v1/namespaces/{ns}/contexts/{claim}/snapshots/{name}/restore

use axum::extract::{Extension, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::get;
use axum::{Json, Router};
use clankr_store::context::{AsOf, ContextObject, Snapshot};
use clankr_types::{ApiError, RequestCtx};
use serde::Deserialize;
use serde_json::Value;

use crate::{ApiResult, AppState};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/namespaces/{ns}/contexts/{claim}/keys", get(list))
        .route("/v1/namespaces/{ns}/contexts/{claim}/keys/{*key}", get(read).put(write).delete(remove))
        .route("/v1/namespaces/{ns}/contexts/{claim}/snapshots", get(snapshots).post(snapshot))
        .route("/v1/namespaces/{ns}/contexts/{claim}/snapshots/{name}/restore", axum::routing::post(restore))
}

/// The claim must exist in the namespace; keys of undeclared claims don't exist.
async fn claim(s: &AppState, ns: &str, claim: &str) -> Result<(), ApiError> {
    s.store.get(ns, "ContextClaim", claim).await.map(drop)
}

/// Writer recorded for provenance. A node daemon proxying for an agent names it in
/// `x-clankr-on-behalf-of`.
// ponytail: header is trusted because only the owner token exists; workload identity (M3) replaces it
pub(crate) fn writer(ctx: &RequestCtx, headers: &HeaderMap, claims: Option<&crate::identity::Claims>) -> String {
    if let Some(c) = claims {
        return format!("{}/agent/{}/{}", ctx.actor, c.agent, c.instance);
    }
    match headers.get("x-clankr-on-behalf-of").and_then(|v| v.to_str().ok()) {
        Some(agent) => format!("{}/{agent}", ctx.actor),
        None => ctx.actor.clone(),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListQuery {
    #[serde(default)]
    prefix: String,
    limit: Option<i64>,
    as_of: Option<chrono::DateTime<chrono::Utc>>,
    snapshot: Option<String>,
}

async fn list(State(s): State<AppState>, Extension(ctx): Extension<RequestCtx>, headers: HeaderMap, Path((ns, c)): Path<(String, String)>, Query(q): Query<ListQuery>) -> ApiResult<Json<Vec<ContextObject>>> {
    crate::workload(&s, &ctx, &headers, &ns, None).await?;
    claim(&s, &ns, &c).await?;
    let limit = q.limit.unwrap_or(100).clamp(1, 1000);
    let at = match (q.snapshot, q.as_of) {
        (Some(n), None) => Some(AsOf::Snapshot(n)),
        (None, Some(t)) => Some(AsOf::Time(t)),
        (None, None) => None,
        (Some(_), Some(_)) => return Err(ApiError::new(clankr_types::ErrorCode::InvalidSpec, "pass asOf or snapshot, not both").into()),
    };
    Ok(Json(match at {
        Some(at) => s.store.context_list_at(&ns, &c, &q.prefix, limit, &at).await?,
        None => s.store.context_list(&ns, &c, &q.prefix, limit).await?,
    }))
}

async fn read(State(s): State<AppState>, Extension(ctx): Extension<RequestCtx>, headers: HeaderMap, Path((ns, c, key)): Path<(String, String, String)>) -> ApiResult<Json<ContextObject>> {
    crate::workload(&s, &ctx, &headers, &ns, None).await?;
    claim(&s, &ns, &c).await?;
    Ok(Json(s.store.context_get(&ns, &c, &key).await?))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PutBody {
    value: Value,
    if_version: Option<i64>,
}

async fn write(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    headers: HeaderMap,
    Path((ns, c, key)): Path<(String, String, String)>,
    Json(b): Json<PutBody>,
) -> ApiResult<Json<ContextObject>> {
    let claims = crate::workload(&s, &ctx, &headers, &ns, None).await?;
    claim(&s, &ns, &c).await?;
    Ok(Json(s.store.context_put(&ns, &c, &key, &b.value, b.if_version, &writer(&ctx, &headers, claims.as_ref())).await?))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeleteQuery {
    if_version: Option<i64>,
}

async fn remove(State(s): State<AppState>, Extension(ctx): Extension<RequestCtx>, headers: HeaderMap, Path((ns, c, key)): Path<(String, String, String)>, Query(q): Query<DeleteQuery>) -> ApiResult<StatusCode> {
    crate::workload(&s, &ctx, &headers, &ns, None).await?;
    claim(&s, &ns, &c).await?;
    s.store.context_delete(&ns, &c, &key, q.if_version).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn snapshots(State(s): State<AppState>, Path((ns, c)): Path<(String, String)>) -> ApiResult<Json<Vec<Snapshot>>> {
    claim(&s, &ns, &c).await?;
    Ok(Json(s.store.context_snapshots(&ns, &c).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotBody {
    name: String,
}

async fn snapshot(State(s): State<AppState>, Extension(ctx): Extension<RequestCtx>, Path((ns, c)): Path<(String, String)>, Json(b): Json<SnapshotBody>) -> ApiResult<(StatusCode, Json<Snapshot>)> {
    claim(&s, &ns, &c).await?;
    Ok((StatusCode::CREATED, Json(s.store.context_snapshot(&ns, &c, &b.name, &ctx.actor).await?)))
}

async fn restore(State(s): State<AppState>, Extension(ctx): Extension<RequestCtx>, Path((ns, c, name)): Path<(String, String, String)>) -> ApiResult<Json<Value>> {
    claim(&s, &ns, &c).await?;
    let (written, deleted) = s.store.context_restore(&ctx, &ns, &c, &name).await?;
    Ok(Json(serde_json::json!({"written": written, "deleted": deleted})))
}
