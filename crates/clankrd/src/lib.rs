//! Management REST API (PRD §35): resource CRUD, status subresource, SSE watch, health.
//!
//!   GET    /v1/{plural}                               list across namespaces
//!   GET    /v1/namespaces/{ns}/{plural}               list (?labelSelector=k=v,..  ?watch=true&cursor=N)
//!   POST   /v1/namespaces/{ns}/{plural}               create (?dryRun=true)
//!   GET    /v1/namespaces/{ns}/{plural}/{name}
//!   PUT    /v1/namespaces/{ns}/{plural}/{name}        CAS on metadata.resourceVersion (?dryRun=true)
//!   PUT    /v1/namespaces/{ns}/{plural}/{name}/status
//!   DELETE /v1/namespaces/{ns}/{plural}/{name}        (?resourceVersion=N)
//!   GET    /v1/audit                                  (?after=SEQ&limit=N)
//!
//! Every response carries `x-request-id` (client-supplied if well-formed, else generated).

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Extension, Path, Query, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, put};
use axum::{Json, Router};
use clankr_store::Store;
use clankr_types::{ApiError, ErrorCode, RequestCtx, Resource, WatchEvent, WatchEventType, kind_for, validate_envelope};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::watch;

mod contexts;
pub mod identity;
pub mod telemetry;
pub mod tls;
mod mailboxes;
mod models;
mod nodes;
mod runs;
pub mod scheduler;

#[derive(Clone)]
pub struct AppState {
    pub store: Store,
    pub engine: clankr_engine::Engine,
    pub gateway: clankr_gateway::Gateway,
    pub master: clankr_store::secrets::MasterKey,
    /// Resource change notifications (watch).
    pub changes: watch::Receiver<i64>,
    /// Task-ready notifications (lease long-poll).
    pub tasks: watch::Receiver<i64>,
    /// Mailbox send notifications (receive long-poll).
    pub mailbox_events: watch::Receiver<i64>,
    pub token: String,
    /// Shared secret nodes present to join the cluster.
    pub join_token: String,
    /// Cluster CA (signs node certificates). None when serving plain HTTP: nodes can't join.
    pub ca: Option<Arc<clankr_tls::Ca>>,
    pub signer: identity::Signer,
}

impl AppState {
    pub async fn new(store: Store, token: String, master: clankr_store::secrets::MasterKey) -> anyhow::Result<Self> {
        Self::with_join_token(store, token, master, String::new()).await
    }

    pub async fn with_join_token(store: Store, token: String, master: clankr_store::secrets::MasterKey, join_token: String) -> anyhow::Result<Self> {
        Ok(Self {
            ca: None,
            signer: identity::Signer::derive(&master.to_hex()),
            engine: clankr_engine::Engine::new(store.clone()),
            gateway: clankr_gateway::Gateway::new(store.clone(), master.clone()),
            master,
            changes: store.change_notifier().await?,
            tasks: store.notifier(clankr_engine::TASKS_CHANNEL).await?,
            mailbox_events: store.notifier("clankr_mailbox").await?,
            store,
            token,
            join_token,
        })
    }
}

/// Lease expiry and timers. Runs in every clankrd; concurrent ticks are safe.
pub fn spawn_ticker(engine: clankr_engine::Engine) {
    let engine_store = engine.store().clone();
    tokio::spawn(async move {
        let mut every = tokio::time::interval(Duration::from_secs(1));
        loop {
            every.tick().await;
            if let Err(e) = engine_store.mailbox_tick().await {
                tracing::warn!(error = %e, "mailbox tick failed");
            }
            match engine.tick().await {
                Ok(r) if r != Default::default() => tracing::info!(leases_expired = r.leases_expired, outcomes_unknown = r.outcomes_unknown, timers_fired = r.timers_fired, "tick"),
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "tick failed"),
            }
        }
    });
}

pub fn router(state: AppState) -> Router {
    let v1 = Router::new()
        .route("/v1/audit", get(audit))
        .route("/v1/{plural}", get(list_all))
        .route("/v1/namespaces/{ns}/{plural}", get(list).post(create))
        .route("/v1/namespaces/{ns}/{plural}/{name}", get(read).put(update).delete(delete))
        .route("/v1/namespaces/{ns}/{plural}/{name}/status", put(update_status))
        .merge(runs::routes())
        .merge(contexts::routes())
        .merge(models::routes())
        .merge(mailboxes::routes())
        .merge(nodes::routes())
        .route_layer(middleware::from_fn_with_state(state.clone(), auth));
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(readyz))
        .merge(nodes::join_route())
        .merge(v1)
        .layer(middleware::from_fn(telemetry::http_span))
        .with_state(state)
}

/// Error body is always an ApiError; the HTTP status is derived from its code.
pub struct HttpError(pub ApiError);

impl From<ApiError> for HttpError {
    fn from(e: ApiError) -> Self {
        Self(e)
    }
}

impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        use ErrorCode::*;
        let status = match self.0.code {
            InvalidSpec | UnsupportedCapability => StatusCode::UNPROCESSABLE_ENTITY,
            NotFound => StatusCode::NOT_FOUND,
            AlreadyExists | Conflict | StaleLease => StatusCode::CONFLICT,
            Forbidden => StatusCode::FORBIDDEN,
            Unauthenticated => StatusCode::UNAUTHORIZED,
            QuotaExceeded => StatusCode::TOO_MANY_REQUESTS,
            HistoryExpired => StatusCode::GONE,
            StorageUnavailable => StatusCode::SERVICE_UNAVAILABLE,
            ProviderUnavailable => StatusCode::BAD_GATEWAY,
            OutcomeUnknown => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, Json(self.0)).into_response()
    }
}

pub(crate) type ApiResult<T> = Result<T, HttpError>;

const REQUEST_ID: &str = "x-request-id";

/// Authenticates, attaches a RequestCtx for handlers, audits denials, echoes the request id.
async fn auth(State(s): State<AppState>, mut req: Request, next: Next) -> Response {
    let request_id = req
        .headers()
        .get(REQUEST_ID)
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty() && v.len() <= 64 && v.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))
        .map_or_else(|| uuid::Uuid::new_v4().to_string(), str::to_owned);
    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    // nodes authenticate with their client certificate (mTLS); the fingerprint must be the
    // one we last issued to that node, so re-joining revokes older certificates
    let peer = req.extensions().get::<Option<tls::PeerCert>>().cloned().flatten();
    let node = match &peer {
        Some(p) => s.store.node_for_credential(&p.fingerprint).await.ok().flatten(),
        None => None,
    };
    let mut resp = match (presented, node) {
        // ponytail: single owner identity until RBAC (M4)
        (Some(t), _) if ct_eq(t.as_bytes(), s.token.as_bytes()) => {
            tracing::Span::current().record("actor", "owner");
            req.extensions_mut().insert(RequestCtx { actor: "owner".into(), request_id: request_id.clone() });
            next.run(req).await
        }
        // node credentials reach only the node surface; per-agent checks happen in handlers
        (_, Some(name)) => {
            let ctx = RequestCtx { actor: format!("{NODE_PREFIX}{name}"), request_id: request_id.clone() };
            tracing::Span::current().record("actor", ctx.actor.as_str());
            if node_route_allowed(&name, req.method(), req.uri().path()) {
                req.extensions_mut().insert(ctx);
                next.run(req).await
            } else {
                let action = format!("{} {}", req.method(), req.uri().path());
                if let Err(e) = s.store.audit_deny(&ctx, &action, "node credential used outside the node API").await {
                    return HttpError(e).into_response();
                }
                HttpError(ApiError::new(ErrorCode::Forbidden, "node credentials may only use the node API")).into_response()
            }
        }
        _ => {
            // ponytail: unauthenticated requests each write an audit row; add per-IP rate limiting before exposing beyond localhost
            let ctx = RequestCtx { actor: "anonymous".into(), request_id: request_id.clone() };
            let action = format!("{} {}", req.method(), req.uri().path());
            if let Err(e) = s.store.audit_deny(&ctx, &action, "missing or invalid bearer token").await {
                return HttpError(e).into_response(); // fail closed if the denial can't be recorded
            }
            HttpError(ApiError::new(ErrorCode::Unauthenticated, "missing or invalid bearer token")).into_response()
        }
    };
    resp.headers_mut().insert(REQUEST_ID, HeaderValue::from_str(&request_id).expect("validated ascii"));
    resp
}

pub const NODE_PREFIX: &str = "node:";

/// The node a request comes from, if it used a node credential.
pub fn node_of(ctx: &RequestCtx) -> Option<&str> {
    ctx.actor.strip_prefix(NODE_PREFIX)
}

/// Paths a node credential may call (CL-31-03, least privilege): its own heartbeat, reading the
/// Agents it runs, and brokering tasks/models/contexts for them.
fn node_route_allowed(node: &str, method: &axum::http::Method, path: &str) -> bool {
    let seg: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    match seg.as_slice() {
        ["v1", "nodes", n, "heartbeat" | "renew"] => *n == node && method == axum::http::Method::POST,
        ["v1", "namespaces", _, "agents", _] => method == axum::http::Method::GET,
        ["v1", "namespaces", _, "tasks", ..] | ["v1", "namespaces", _, "models", ..] | ["v1", "namespaces", _, "contexts", ..] => true,
        ["v1", "namespaces", _, "mailboxes", _, "messages" | "receive" | "ack" | "nack"] => method == axum::http::Method::POST,
        _ => false,
    }
}

/// For calls from a node: the verified workload identity they act under. The token must be
/// ours, unexpired, issued to this node for `ns` (and `agent`, if given), and the instance must
/// still be placed on this node at the token's epoch, so a node that lost an instance (moved
/// after a partition) is refused immediately. Owners get None: no workload scoping.
pub(crate) async fn workload(s: &AppState, ctx: &RequestCtx, headers: &axum::http::HeaderMap, ns: &str, agent: Option<&str>) -> Result<Option<identity::Claims>, ApiError> {
    let Some(node) = node_of(ctx) else { return Ok(None) };
    let token = headers
        .get(identity::HEADER)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| ApiError::new(ErrorCode::Forbidden, "node calls must carry a workload identity token"))?;
    let c = s.signer.verify(token)?;
    if c.node != node || c.ns != ns || agent.is_some_and(|a| a != c.agent) {
        return Err(ApiError::new(ErrorCode::Forbidden, format!("workload token for {}/{} on {} doesn't cover this call", c.ns, c.agent, c.node)));
    }
    match s.store.instance_placement(&c.instance).await? {
        Some(i) if i.node.as_deref() == Some(node) && i.epoch == c.epoch && i.namespace == c.ns && i.agent == c.agent => Ok(Some(c)),
        _ => Err(ApiError::new(ErrorCode::StaleLease, format!("instance {} is no longer placed on {node} at epoch {}", c.instance, c.epoch))),
    }
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn readyz(State(s): State<AppState>) -> ApiResult<&'static str> {
    s.store.ping().await?;
    Ok("ok")
}

fn kind(plural: &str) -> Result<&'static str, ApiError> {
    kind_for(plural).ok_or_else(|| ApiError::new(ErrorCode::NotFound, format!("unknown resource type {plural:?}")))
}

/// Body must agree with the URL; a mismatch is a client bug, never silently corrected.
fn check_target(r: &Resource, ns: &str, kind: &str, name: Option<&str>) -> Result<(), ApiError> {
    let bad = |field: &str, want: &str| ApiError::new(ErrorCode::InvalidSpec, format!("{field} does not match URL ({want})")).field(field);
    if r.kind != kind {
        return Err(bad("kind", kind));
    }
    if r.metadata.namespace != ns {
        return Err(bad("metadata.namespace", ns));
    }
    if let Some(n) = name.filter(|n| *n != r.metadata.name) {
        return Err(bad("metadata.name", n));
    }
    validate_envelope(r)?;
    admit_kind(r)
}

/// Kind-specific spec validation at admission (CL-19-06).
fn admit_kind(r: &Resource) -> Result<(), ApiError> {
    match r.kind.as_str() {
        "Workflow" => clankr_engine::admit_workflow(&r.spec).map(drop),
        "ProviderConnection" => clankr_gateway::admit_connection(&r.spec).map(drop),
        "ModelClaim" => clankr_gateway::admit_claim(&r.spec).map(drop),
        "Agent" => clankr_types::agent::admit_agent(&r.spec).map(drop),
        "AgentDeployment" => clankr_types::agent::admit_deployment(&r.spec).map(drop),
        "VolumeClaim" => clankr_types::agent::admit_volume_claim(&r.spec).map(drop),
        "AgentMailbox" => clankr_types::agent::admit_mailbox(&r.spec).map(drop),
        _ => Ok(()),
    }
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct ListQuery {
    label_selector: Option<String>,
    #[serde(default)]
    watch: bool,
    cursor: Option<i64>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct WriteQuery {
    #[serde(default)]
    dry_run: bool,
    resource_version: Option<i64>,
}

fn parse_selector(sel: Option<&str>) -> Result<BTreeMap<String, String>, ApiError> {
    sel.unwrap_or("")
        .split(',')
        .filter(|p| !p.is_empty())
        .map(|p| {
            p.split_once('=')
                .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
                .ok_or_else(|| ApiError::new(ErrorCode::InvalidSpec, format!("bad label selector term {p:?}, want k=v")).field("labelSelector"))
        })
        .collect()
}

async fn list_all(state: State<AppState>, Path(plural): Path<String>, q: Query<ListQuery>) -> ApiResult<Response> {
    list_impl(state, None, plural, q.0).await
}

async fn list(state: State<AppState>, Path((ns, plural)): Path<(String, String)>, q: Query<ListQuery>) -> ApiResult<Response> {
    list_impl(state, Some(ns), plural, q.0).await
}

async fn list_impl(State(s): State<AppState>, ns: Option<String>, plural: String, q: ListQuery) -> ApiResult<Response> {
    let kind = kind(&plural)?;
    let labels = parse_selector(q.label_selector.as_deref())?;
    if q.watch {
        return Ok(watch_stream(s, kind, ns, labels, q.cursor).await?.into_response());
    }
    let (items, rev) = s.store.list(kind, ns.as_deref(), &labels).await?;
    Ok(Json(serde_json::json!({ "items": items, "resourceVersion": rev })).into_response())
}

/// SSE stream of WatchEvents (event id = revision, CL-35-03). Without a cursor it starts with
/// the current list as Added events. An expired cursor yields one `error` event (HistoryExpired).
async fn watch_stream(
    s: AppState,
    kind: &'static str,
    ns: Option<String>,
    labels: BTreeMap<String, String>,
    cursor: Option<i64>,
) -> Result<Sse<impl futures_core::Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let (initial, start) = match cursor {
        Some(c) => (vec![], c),
        None => s.store.list(kind, ns.as_deref(), &labels).await?,
    };
    let mut changes = s.changes.clone();
    let stream = async_stream::stream! {
        for object in initial {
            let rv = object.metadata.resource_version;
            yield Ok(sse(rv, &WatchEvent { kind: WatchEventType::Added, object }));
        }
        // resume point after the initial list; clients persist the last SSE id they saw
        yield Ok(Event::default().event("bookmark").id(start.to_string()).data(""));
        let mut cursor = start;
        loop {
            changes.borrow_and_update();
            match s.store.changes_since(cursor, Some(kind), ns.as_deref(), 500).await {
                Err(e) => {
                    yield Ok(Event::default().event("error").json_data(&e).unwrap());
                    break;
                }
                Ok(batch) if batch.is_empty() => {
                    let _ = tokio::time::timeout(Duration::from_secs(15), changes.changed()).await;
                }
                Ok(batch) => {
                    for (rev, ev) in batch {
                        cursor = rev;
                        if labels.iter().all(|(k, v)| ev.object.metadata.labels.get(k) == Some(v)) {
                            yield Ok(sse(rev, &ev));
                        }
                    }
                }
            }
        }
    };
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

fn sse(rev: i64, ev: &WatchEvent) -> Event {
    Event::default().id(rev.to_string()).json_data(ev).expect("WatchEvent serializes")
}

async fn create(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Path((ns, plural)): Path<(String, String)>,
    q: Query<WriteQuery>,
    Json(r): Json<Resource>,
) -> ApiResult<(StatusCode, Json<Resource>)> {
    check_target(&r, &ns, kind(&plural)?, None)?;
    if q.dry_run {
        return Ok((StatusCode::OK, Json(r)));
    }
    Ok((StatusCode::CREATED, Json(s.store.create(&ctx, r).await?)))
}

async fn read(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    headers: axum::http::HeaderMap,
    Path((ns, plural, name)): Path<(String, String, String)>,
) -> ApiResult<Json<Resource>> {
    let kind = kind(&plural)?;
    if node_of(&ctx).is_some() {
        if kind != "Agent" {
            return Err(ApiError::new(ErrorCode::Forbidden, "nodes may only read Agents").into());
        }
        workload(&s, &ctx, &headers, &ns, Some(&name)).await?;
    }
    Ok(Json(s.store.get(&ns, kind, &name).await?))
}

async fn update(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Path((ns, plural, name)): Path<(String, String, String)>,
    q: Query<WriteQuery>,
    Json(r): Json<Resource>,
) -> ApiResult<Json<Resource>> {
    check_target(&r, &ns, kind(&plural)?, Some(&name))?;
    if q.dry_run {
        return Ok(Json(r));
    }
    Ok(Json(s.store.update(&ctx, r).await?))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StatusBody {
    resource_version: i64,
    status: Value,
}

async fn update_status(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Path((ns, plural, name)): Path<(String, String, String)>,
    Json(b): Json<StatusBody>,
) -> ApiResult<Json<Resource>> {
    Ok(Json(s.store.update_status(&ctx, &ns, kind(&plural)?, &name, b.resource_version, b.status).await?))
}

async fn delete(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Path((ns, plural, name)): Path<(String, String, String)>,
    q: Query<WriteQuery>,
) -> ApiResult<Json<Resource>> {
    Ok(Json(s.store.delete(&ctx, &ns, kind(&plural)?, &name, q.resource_version).await?))
}

#[derive(Deserialize)]
struct AuditQuery {
    #[serde(default)]
    after: i64,
    limit: Option<i64>,
}

async fn audit(State(s): State<AppState>, Query(q): Query<AuditQuery>) -> ApiResult<Json<Vec<clankr_store::AuditEvent>>> {
    Ok(Json(s.store.audit_since(q.after, q.limit.unwrap_or(500).clamp(1, 5000)).await?))
}
