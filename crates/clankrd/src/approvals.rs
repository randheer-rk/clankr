//! Approval inbox API (HUM-02).
//!
//! `mine=true` lists what the caller can actually decide: their groups, not runs they started.

use axum::extract::{Extension, Path, Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use clankr_engine::{Approval, Decider, Decision};
use clankr_types::rbac::Principal;
use clankr_types::{ApiError, ErrorCode, RequestCtx};
use serde::Deserialize;
use uuid::Uuid;

use crate::{ApiResult, AppState};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/namespaces/{ns}/approvals", get(list))
        .route("/v1/namespaces/{ns}/approvals/{id}", get(read))
        .route("/v1/namespaces/{ns}/approvals/{id}/{decision}", post(decide))
}

fn approval_id(s: &str) -> Result<Uuid, ApiError> {
    s.parse().map_err(|_| ApiError::new(ErrorCode::NotFound, format!("approval {s:?} not found")))
}

#[derive(Deserialize)]
struct ListQuery {
    state: Option<String>,
    #[serde(default)]
    mine: bool,
    limit: Option<i64>,
}

async fn list(State(s): State<AppState>, Extension(ctx): Extension<RequestCtx>, Extension(p): Extension<Principal>, Path(ns): Path<String>, Query(q): Query<ListQuery>) -> ApiResult<Json<Vec<Approval>>> {
    let mut items = s.engine.approvals(&ns, q.state.as_deref(), q.limit.unwrap_or(100).clamp(1, 1000)).await?;
    if q.mine {
        let owner = p == Principal::Owner;
        items.retain(|a| owner || (p.groups().contains(&a.approver_group) && a.requested_by.as_deref() != Some(ctx.actor.as_str())));
    }
    Ok(Json(items))
}

async fn read(State(s): State<AppState>, Path((ns, id)): Path<(String, String)>) -> ApiResult<Json<Approval>> {
    Ok(Json(s.engine.approval(&ns, approval_id(&id)?).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DecideBody {
    digest: String,
    #[serde(default)]
    comment: String,
}

async fn decide(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Extension(p): Extension<Principal>,
    Path((ns, id, decision)): Path<(String, String, String)>,
    Json(b): Json<DecideBody>,
) -> ApiResult<Json<Approval>> {
    let decision = match decision.as_str() {
        "approve" => Decision::Approve,
        "reject" => Decision::Reject,
        other => return Err(ApiError::new(ErrorCode::NotFound, format!("unknown approval action {other:?}; approve or reject")).into()),
    };
    let who = Decider { ctx: &ctx, groups: p.groups(), break_glass: p == Principal::Owner };
    Ok(Json(s.engine.decide_approval(&who, &ns, approval_id(&id)?, decision, &b.digest, &b.comment).await?))
}
