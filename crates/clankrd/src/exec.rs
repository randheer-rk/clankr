//! `clank exec` (ADR 0030): an audited shell into a running agent instance. Nodes connect to
//! the control plane, never the other way round, so clankrd is a rendezvous: it holds each
//! session until the instance's node picks it up (from its heartbeat), then relays bytes. Both
//! sides speak kennel's exec frames, `[kind, len u32 BE, bytes]`, which clankrd passes through.
//!
//!   POST /v1/namespaces/{ns}/instances/{id}/exec  {command, tty, rows, cols} -> {session}
//!   GET  /v1/namespaces/{ns}/exec/{session}/output   frames from the process (ends at exit)
//!   POST /v1/namespaces/{ns}/exec/{session}/input    frames to it (streamed request body)
//!   GET  /v1/nodes/{name}/exec/{session}/input       (node) the user's input
//!   POST /v1/nodes/{name}/exec/{session}/output      (node) the process's output
//!   GET  /v1/namespaces/{ns}/instances               instances, to find one to exec into

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use clankr_types::{ApiError, ErrorCode, RequestCtx};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::{ApiResult, AppState};

/// Sessions the node hasn't picked up by then are dropped.
const CLAIM_TIMEOUT: Duration = Duration::from_secs(30);

pub struct Session {
    pub namespace: String,
    pub instance: String,
    pub node: String,
    actor: String,
    pub request: Value,
    created: Instant,
    claimed: Mutex<bool>,
    to_node: (mpsc::Sender<Bytes>, Mutex<Option<mpsc::Receiver<Bytes>>>),
    to_user: (mpsc::Sender<Bytes>, Mutex<Option<mpsc::Receiver<Bytes>>>),
}

/// Live exec sessions. ponytail: in this replica's memory, so the user and the node must reach
/// the same clankrd (one replica, or sticky routing); a shared relay can come with HA exec.
#[derive(Default, Clone)]
pub struct Sessions(Arc<Mutex<HashMap<Uuid, Arc<Session>>>>);

impl Sessions {
    fn get(&self, id: Uuid) -> Option<Arc<Session>> {
        let mut m = self.0.lock().unwrap();
        m.retain(|_, s| *s.claimed.lock().unwrap() || s.created.elapsed() < CLAIM_TIMEOUT);
        m.get(&id).cloned()
    }

    /// Unclaimed sessions for instances on `node`, for its heartbeat.
    pub fn pending_for(&self, node: &str) -> Vec<Value> {
        let mut m = self.0.lock().unwrap();
        m.retain(|_, s| *s.claimed.lock().unwrap() || s.created.elapsed() < CLAIM_TIMEOUT);
        m.iter()
            .filter(|(_, s)| s.node == node && !*s.claimed.lock().unwrap())
            .map(|(id, s)| json!({"session": id, "namespace": s.namespace, "instance": s.instance, "request": s.request}))
            .collect()
    }

    fn remove(&self, id: Uuid) {
        self.0.lock().unwrap().remove(&id);
    }
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/namespaces/{ns}/instances", get(list_instances))
        .route("/v1/namespaces/{ns}/instances/{id}/exec", post(start))
        .route("/v1/namespaces/{ns}/exec/{session}/output", get(user_output))
        .route("/v1/namespaces/{ns}/exec/{session}/input", post(user_input))
        .route("/v1/nodes/{name}/exec/{session}/input", get(node_input))
        .route("/v1/nodes/{name}/exec/{session}/output", post(node_output))
}

async fn list_instances(State(s): State<AppState>, Path(ns): Path<String>) -> ApiResult<Json<Value>> {
    let items: Vec<Value> = s
        .store
        .instances()
        .await?
        .into_iter()
        .filter(|i| i.namespace == ns)
        .map(|i| json!({"id": i.id, "deployment": i.deployment, "agent": i.agent, "node": i.node, "ready": i.ready_epoch == Some(i.epoch)}))
        .collect();
    Ok(Json(json!({"items": items})))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StartBody {
    command: Vec<String>,
    #[serde(default)]
    tty: bool,
    #[serde(default)]
    rows: Option<u16>,
    #[serde(default)]
    cols: Option<u16>,
}

async fn start(State(s): State<AppState>, Extension(ctx): Extension<RequestCtx>, Path((ns, id)): Path<(String, String)>, Json(b): Json<StartBody>) -> ApiResult<(StatusCode, Json<Value>)> {
    if b.command.is_empty() {
        return Err(ApiError::new(ErrorCode::InvalidSpec, "command is required").field("command").into());
    }
    let inst = s.store.instance_placement(&id).await?.filter(|i| i.namespace == ns).ok_or_else(|| ApiError::new(ErrorCode::NotFound, format!("no instance {id} in {ns}")))?;
    let node = inst.node.clone().ok_or_else(|| ApiError::new(ErrorCode::Conflict, format!("instance {id} isn't placed on a node")))?;
    let agent = clankr_types::agent::admit_agent(&s.store.get(&ns, "Agent", &inst.agent).await?.spec)?;
    if !matches!(agent.runtime, clankr_types::agent::Runtime::Oci { .. }) {
        return Err(ApiError::new(ErrorCode::UnsupportedCapability, "exec needs a sandboxed (oci) runtime; process-runtime agents are plain host processes").into());
    }
    let session = Uuid::new_v4();
    // the exec itself is the audited act: who, into what, running what
    let mut tx = s.store.pool().begin().await.map_err(clankr_store::db)?;
    let target = clankr_store::AuditTarget { kind: "Instance", namespace: &ns, name: &id, uid: Some(session.to_string()) };
    clankr_store::audit_in_tx(&mut tx, &ctx, &format!("exec {}{}", if b.tty { "-t " } else { "" }, shell_words(&b.command)), Some(target), true, None, None).await?;
    tx.commit().await.map_err(clankr_store::db)?;
    let (a, ar) = mpsc::channel(64);
    let (c, cr) = mpsc::channel(64);
    let request = json!({"command": b.command, "tty": b.tty, "rows": b.rows, "cols": b.cols});
    let sess = Session { namespace: ns, instance: id, node, actor: ctx.actor.clone(), request, created: Instant::now(), claimed: Mutex::new(false), to_node: (a, Mutex::new(Some(ar))), to_user: (c, Mutex::new(Some(cr))) };
    s.exec.0.lock().unwrap().insert(session, Arc::new(sess));
    Ok((StatusCode::CREATED, Json(json!({"session": session}))))
}

fn shell_words(cmd: &[String]) -> String {
    cmd.iter().map(|a| if a.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:".contains(c)) { a.clone() } else { format!("{a:?}") }).collect::<Vec<_>>().join(" ")
}

fn session_for_user(s: &AppState, ctx: &RequestCtx, ns: &str, id: &str) -> Result<(Uuid, Arc<Session>), ApiError> {
    let id: Uuid = id.parse().map_err(|_| ApiError::new(ErrorCode::NotFound, "no such exec session"))?;
    let sess = s.exec.get(id).filter(|x| x.namespace == ns).ok_or_else(|| ApiError::new(ErrorCode::NotFound, "no such exec session (expired?)"))?;
    if sess.actor != ctx.actor {
        return Err(ApiError::new(ErrorCode::Forbidden, "exec sessions belong to whoever started them"));
    }
    Ok((id, sess))
}

fn session_for_node(s: &AppState, ctx: &RequestCtx, name: &str, id: &str) -> Result<(Uuid, Arc<Session>), ApiError> {
    if crate::node_of(ctx) != Some(name) {
        return Err(ApiError::new(ErrorCode::Forbidden, "only the instance's node joins an exec session"));
    }
    let id: Uuid = id.parse().map_err(|_| ApiError::new(ErrorCode::NotFound, "no such exec session"))?;
    let sess = s.exec.get(id).filter(|x| x.node == name).ok_or_else(|| ApiError::new(ErrorCode::NotFound, "no such exec session"))?;
    Ok((id, sess))
}

/// A body that streams what arrives on `rx` (and drops the session when it ends).
fn stream_body(rx: mpsc::Receiver<Bytes>, done: Option<(Sessions, Uuid)>) -> Body {
    let s = futures_util::stream::unfold((rx, done), |(mut rx, done)| async move {
        match rx.recv().await {
            Some(b) => Some((Ok::<_, std::io::Error>(b), (rx, done))),
            None => {
                if let Some((sessions, id)) = done {
                    sessions.remove(id);
                }
                None
            }
        }
    });
    Body::from_stream(s)
}

/// Forwards a streamed request body into `tx`.
async fn pump(body: Body, tx: mpsc::Sender<Bytes>) {
    let mut s = body.into_data_stream();
    while let Some(Ok(chunk)) = s.next().await {
        if tx.send(chunk).await.is_err() {
            break;
        }
    }
}

async fn user_output(State(s): State<AppState>, Extension(ctx): Extension<RequestCtx>, Path((ns, id)): Path<(String, String)>) -> ApiResult<Body> {
    let (sid, sess) = session_for_user(&s, &ctx, &ns, &id)?;
    // don't hold the user on a session no node will join
    while !*sess.claimed.lock().unwrap() {
        if sess.created.elapsed() >= CLAIM_TIMEOUT {
            s.exec.remove(sid);
            return Err(ApiError::new(ErrorCode::Conflict, format!("node {} didn't pick up the session within {}s", sess.node, CLAIM_TIMEOUT.as_secs())).into());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let rx = sess.to_user.1.lock().unwrap().take().ok_or_else(|| ApiError::new(ErrorCode::Conflict, "this session's output is already being read"))?;
    Ok(stream_body(rx, Some((s.exec.clone(), sid))))
}

async fn user_input(State(s): State<AppState>, Extension(ctx): Extension<RequestCtx>, Path((ns, id)): Path<(String, String)>, body: Body) -> ApiResult<StatusCode> {
    let (_, sess) = session_for_user(&s, &ctx, &ns, &id)?;
    pump(body, sess.to_node.0.clone()).await;
    Ok(StatusCode::NO_CONTENT)
}

async fn node_input(State(s): State<AppState>, Extension(ctx): Extension<RequestCtx>, Path((name, id)): Path<(String, String)>) -> ApiResult<Body> {
    let (_, sess) = session_for_node(&s, &ctx, &name, &id)?;
    let rx = sess.to_node.1.lock().unwrap().take().ok_or_else(|| ApiError::new(ErrorCode::Conflict, "session already joined"))?;
    *sess.claimed.lock().unwrap() = true;
    Ok(stream_body(rx, None))
}

async fn node_output(State(s): State<AppState>, Extension(ctx): Extension<RequestCtx>, Path((name, id)): Path<(String, String)>, body: Body) -> ApiResult<StatusCode> {
    let (_, sess) = session_for_node(&s, &ctx, &name, &id)?;
    pump(body, sess.to_user.0.clone()).await;
    Ok(StatusCode::NO_CONTENT)
}
