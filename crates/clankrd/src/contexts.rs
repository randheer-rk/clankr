//! Context API over a bound ContextClaim (PRD §29).
//!
//!   GET    /v1/namespaces/{ns}/contexts/{claim}/keys?prefix=&limit=
//!   GET    /v1/namespaces/{ns}/contexts/{claim}/keys/{*key}
//!   PUT    /v1/namespaces/{ns}/contexts/{claim}/keys/{*key}   {value, ifVersion?}  (ifVersion 0 = create only)
//!   DELETE /v1/namespaces/{ns}/contexts/{claim}/keys/{*key}?ifVersion=

use axum::extract::{Extension, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::get;
use axum::{Json, Router};
use clankr_store::context::ContextObject;
use clankr_types::{ApiError, RequestCtx};
use serde::Deserialize;
use serde_json::Value;

use crate::{ApiResult, AppState};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/namespaces/{ns}/contexts/{claim}/keys", get(list))
        .route("/v1/namespaces/{ns}/contexts/{claim}/keys/{*key}", get(read).put(write).delete(remove))
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
struct ListQuery {
    #[serde(default)]
    prefix: String,
    limit: Option<i64>,
}

async fn list(State(s): State<AppState>, Extension(ctx): Extension<RequestCtx>, headers: HeaderMap, Path((ns, c)): Path<(String, String)>, Query(q): Query<ListQuery>) -> ApiResult<Json<Vec<ContextObject>>> {
    crate::workload(&s, &ctx, &headers, &ns, None).await?;
    claim(&s, &ns, &c).await?;
    Ok(Json(s.store.context_list(&ns, &c, &q.prefix, q.limit.unwrap_or(100).clamp(1, 1000)).await?))
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
