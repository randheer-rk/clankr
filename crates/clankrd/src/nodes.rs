//! Node API (PRD §23).
//!
//!   POST /v1/nodes/join              {name, joinToken, capacity, labels} -> {credential}   (no bearer)
//!   POST /v1/nodes/{name}/heartbeat  (node credential) -> {assignments: [...]}
//!   POST /v1/nodes/{name}/renew      (node credential) {csr} -> {certificate}: rotates the node cert
//!   GET  /v1/nodes                   (owner) nodes with readiness and their instances
//!   POST /v1/nodes/{name}/cordon|uncordon|drain   (owner) maintenance

use axum::extract::{Extension, Path, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use clankr_types::agent::NodeCapacity;
use clankr_types::{ApiError, ErrorCode, RequestCtx};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{ApiResult, AppState, ct_eq, node_of};

/// Join is authenticated by the join token in the body, not by a bearer credential.
pub fn join_route() -> Router<AppState> {
    Router::new().route("/v1/nodes/join", post(join))
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/nodes", get(list))
        .route("/v1/nodes/{name}/heartbeat", post(heartbeat))
        .route("/v1/nodes/{name}/renew", post(renew))
        .route("/v1/nodes/{name}/{action}", post(maintenance))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct JoinBody {
    name: String,
    join_token: String,
    /// PEM CSR from a key the node generated and keeps.
    csr: String,
    capacity: NodeCapacity,
    #[serde(default)]
    labels: std::collections::BTreeMap<String, String>,
}

async fn join(State(s): State<AppState>, Json(b): Json<JoinBody>) -> ApiResult<Json<Value>> {
    let ctx = RequestCtx { actor: format!("node-join:{}", b.name), request_id: uuid::Uuid::new_v4().to_string() };
    if s.join_token.is_empty() || !ct_eq(b.join_token.as_bytes(), s.join_token.as_bytes()) {
        s.store.audit_deny(&ctx, "node join", "bad join token").await?;
        return Err(ApiError::new(ErrorCode::Unauthenticated, "invalid join token").into());
    }
    let valid = !b.name.is_empty() && b.name.len() <= 63 && b.name.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-');
    if !valid {
        return Err(ApiError::new(ErrorCode::InvalidSpec, "node name must be a DNS label").field("name").into());
    }
    let Some(ca) = &s.ca else {
        return Err(ApiError::new(ErrorCode::UnsupportedCapability, "this clankrd serves plain HTTP; nodes need TLS to join").into());
    };
    let cert = ca.sign_node_csr(&b.csr, &b.name).map_err(|e| ApiError::new(ErrorCode::InvalidSpec, format!("{e:#}")).field("csr"))?;
    let fp = clankr_tls::fingerprint_pem(&cert).map_err(|e| ApiError::new(ErrorCode::InvalidSpec, e.to_string()))?;
    // only this certificate authenticates the node from now on (re-join revokes the previous one)
    s.store.register_node(&b.name, &fp, &json!(b.labels), &serde_json::to_value(&b.capacity).unwrap()).await?;
    s.store.audit_in_tx_simple(&ctx, "node join", &b.name).await?;
    tracing::info!(node = %b.name, "node joined");
    Ok(Json(json!({ "certificate": cert, "ca": ca.cert_pem })))
}

async fn heartbeat(State(s): State<AppState>, Extension(ctx): Extension<RequestCtx>, Path(name): Path<String>, body: axum::body::Bytes) -> ApiResult<Json<Value>> {
    if node_of(&ctx) != Some(name.as_str()) {
        return Err(ApiError::new(ErrorCode::Forbidden, "heartbeat is node-only").into());
    }
    // optional readiness report: {"instances": [{id, epoch, ready}]} (older nodes send nothing)
    if !body.is_empty() {
        let v: Value = serde_json::from_slice(&body).map_err(|e| ApiError::new(ErrorCode::InvalidSpec, format!("heartbeat body: {e}")))?;
        let reports: Vec<(String, i64, bool)> = v["instances"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|i| Some((i["id"].as_str()?.to_owned(), i["epoch"].as_i64()?, i["ready"].as_bool().unwrap_or(false))))
            .collect();
        s.store.report_readiness(&name, &reports).await?;
    }
    let exp = chrono::Utc::now().timestamp() + crate::identity::TTL_SECS;
    let assignments: Vec<Value> = s
        .store
        .node_heartbeat(&name)
        .await?
        .into_iter()
        .map(|i| {
            let claims = crate::identity::Claims { ns: i.namespace.clone(), agent: i.agent.clone(), instance: i.id.clone(), node: name.clone(), epoch: i.epoch, exp };
            json!({ "id": i.id, "namespace": i.namespace, "agent": i.agent, "epoch": i.epoch, "identity": s.signer.issue(&claims) })
        })
        .collect();
    Ok(Json(json!({ "assignments": assignments })))
}

async fn list(State(s): State<AppState>, Extension(ctx): Extension<RequestCtx>) -> ApiResult<Json<Value>> {
    if node_of(&ctx).is_some() {
        return Err(ApiError::new(ErrorCode::Forbidden, "owner only").into());
    }
    Ok(Json(json!({ "nodes": s.store.nodes().await?, "instances": s.store.instances().await? })))
}

async fn maintenance(State(s): State<AppState>, Extension(ctx): Extension<RequestCtx>, Path((name, action)): Path<(String, String)>) -> ApiResult<Json<Value>> {
    if node_of(&ctx).is_some() {
        return Err(ApiError::new(ErrorCode::Forbidden, "owner only").into());
    }
    let (cordoned, draining) = match action.as_str() {
        "cordon" => (true, false),
        "uncordon" => (false, false),
        "drain" => (true, true),
        other => return Err(ApiError::new(ErrorCode::NotFound, format!("unknown node action {other:?}")).into()),
    };
    s.store.set_node_maintenance(&name, cordoned, draining).await?;
    s.store.audit_in_tx_simple(&ctx, &format!("node {action}"), &name).await?;
    Ok(Json(json!({ "node": name, "cordoned": cordoned, "draining": draining })))
}

#[derive(Deserialize)]
struct RenewBody {
    csr: String,
}

/// Certificate rotation: authenticated by the node's current certificate, signs a CSR for a fresh
/// key. The new fingerprint becomes current; the previous one is honoured for 5 minutes.
async fn renew(State(s): State<AppState>, Extension(ctx): Extension<RequestCtx>, Path(name): Path<String>, Json(b): Json<RenewBody>) -> ApiResult<Json<Value>> {
    if node_of(&ctx) != Some(name.as_str()) {
        return Err(ApiError::new(ErrorCode::Forbidden, "a node can only renew its own certificate").into());
    }
    let Some(ca) = &s.ca else { return Err(ApiError::new(ErrorCode::UnsupportedCapability, "TLS disabled").into()) };
    let cert = ca.sign_node_csr(&b.csr, &name).map_err(|e| ApiError::new(ErrorCode::InvalidSpec, format!("{e:#}")).field("csr"))?;
    let fp = clankr_tls::fingerprint_pem(&cert).map_err(|e| ApiError::new(ErrorCode::InvalidSpec, e.to_string()))?;
    // the old cert keeps working briefly so in-flight requests on pooled connections finish
    s.store.rotate_node_credential(&name, &fp, 300.0).await?;
    s.store.audit_in_tx_simple(&ctx, "node renew", &name).await?;
    Ok(Json(json!({ "certificate": cert })))
}
