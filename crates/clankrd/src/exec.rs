//! `clank exec` (ADR 0030): an audited shell into a running agent instance. Nodes connect to
//! the control plane, never the other way round, so clankrd is a rendezvous: a session waits in
//! Postgres until the instance's node picks it up (from its heartbeat), then bytes are relayed.
//! The user's and the node's connections may reach different replicas; each receiving leg
//! LISTENs on the session's channel and each sending leg NOTIFYs it. Both sides speak kennel's
//! exec frames, `[kind, len u32 BE, bytes]`, which clankrd passes through.
//!
//! Order, so nothing is sent before its listener exists: the user's output listener subscribes,
//! then the session is offered to the node; the node's input listener subscribes, then the
//! session is claimed and the user's input may flow.

use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use clankr_store::db;
use clankr_types::{ApiError, ErrorCode, RequestCtx};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::postgres::PgListener;
use uuid::Uuid;

use crate::{ApiResult, AppState};

/// Sessions the node hasn't picked up by then are dropped.
const CLAIM_TIMEOUT: Duration = Duration::from_secs(30);
/// Raw bytes per NOTIFY (base64 must stay under Postgres's 8000-byte payload limit).
const CHUNK: usize = 4096;

struct Session {
    namespace: String,
    node: String,
    actor: String,
    claimed: bool,
    age: Duration,
}

fn channel(id: Uuid, dir: &str) -> String {
    format!("exec_{}_{dir}", id.simple())
}

async fn load(s: &AppState, id: Uuid) -> Result<Option<Session>, ApiError> {
    let r: Option<(String, String, String, bool, f64)> =
        sqlx::query_as("select namespace, node, actor, claimed, extract(epoch from now() - created_at)::float8 from exec_sessions where id = $1").bind(id).fetch_optional(s.store.pool()).await.map_err(db)?;
    Ok(r.map(|(namespace, node, actor, claimed, age)| Session { namespace, node, actor, claimed, age: Duration::from_secs_f64(age.max(0.0)) }))
}

async fn remove(s: &AppState, id: Uuid) {
    let _ = sqlx::query("delete from exec_sessions where id = $1").bind(id).execute(s.store.pool()).await;
}

/// Sessions offered to `node`, for its heartbeat; also drops unclaimed ones past the timeout and any left behind by a vanished user.
pub async fn pending_for(s: &AppState, node: &str) -> Result<Vec<Value>, ApiError> {
    sqlx::query("delete from exec_sessions where (not claimed and created_at < now() - make_interval(secs => $1)) or created_at < now() - interval '1 day'")
        .bind(CLAIM_TIMEOUT.as_secs_f64())
        .execute(s.store.pool())
        .await
        .map_err(db)?;
    let rows: Vec<(Uuid, String, String, Value)> = sqlx::query_as("select id, namespace, instance, request from exec_sessions where node = $1 and user_listening and not claimed")
        .bind(node)
        .fetch_all(s.store.pool())
        .await
        .map_err(db)?;
    Ok(rows.into_iter().map(|(id, ns, instance, request)| json!({"session": id, "namespace": ns, "instance": instance, "request": request})).collect())
}

async fn listen(s: &AppState, id: Uuid, dir: &str) -> Result<PgListener, ApiError> {
    let mut l = PgListener::connect_with(s.store.pool()).await.map_err(db)?;
    l.listen(&channel(id, dir)).await.map_err(db)?;
    Ok(l)
}

/// A body of what arrives on the listener, until the sender's close (an empty payload) or a lost
/// connection, which ends the session rather than silently dropping bytes. `done` runs at the end.
fn stream_body(l: PgListener, done: Option<(AppState, Uuid)>) -> Body {
    let s = futures_util::stream::unfold((l, done), |(mut l, done)| async move {
        let chunk = match l.try_recv().await {
            Ok(Some(n)) if !n.payload().is_empty() => B64.decode(n.payload()).ok().map(Bytes::from),
            _ => None,
        };
        match chunk {
            Some(b) => Some((Ok::<_, std::io::Error>(b), (l, done))),
            None => {
                if let Some((s, id)) = done {
                    remove(&s, id).await;
                }
                None
            }
        }
    });
    Body::from_stream(s)
}

/// Forwards a streamed request body to the session's `dir` channel, then closes it.
async fn pump(s: &AppState, body: Body, id: Uuid, dir: &str) -> Result<(), ApiError> {
    let ch = channel(id, dir);
    let mut b = body.into_data_stream();
    while let Some(Ok(chunk)) = b.next().await {
        for piece in chunk.chunks(CHUNK) {
            sqlx::query("select pg_notify($1, $2)").bind(&ch).bind(B64.encode(piece)).execute(s.store.pool()).await.map_err(db)?;
        }
    }
    sqlx::query("select pg_notify($1, '')").bind(&ch).execute(s.store.pool()).await.map_err(db)?;
    Ok(())
}

/// Waits for the node to claim the session (it's dropped if none does in time).
async fn wait_claimed(s: &AppState, id: Uuid) -> Result<(), ApiError> {
    loop {
        let x = load(s, id).await?.ok_or_else(|| ApiError::new(ErrorCode::NotFound, "no such exec session (expired?)"))?;
        if x.claimed {
            return Ok(());
        }
        if x.age >= CLAIM_TIMEOUT {
            remove(s, id).await;
            return Err(ApiError::new(ErrorCode::Conflict, format!("node {} didn't pick up the session within {}s", x.node, CLAIM_TIMEOUT.as_secs())));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
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
    let request = json!({"command": b.command, "tty": b.tty, "rows": b.rows, "cols": b.cols});
    sqlx::query("insert into exec_sessions (id, namespace, instance, node, actor, request) values ($1, $2, $3, $4, $5, $6)")
        .bind(session)
        .bind(&ns)
        .bind(&id)
        .bind(&node)
        .bind(&ctx.actor)
        .bind(&request)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    tx.commit().await.map_err(clankr_store::db)?;
    Ok((StatusCode::CREATED, Json(json!({"session": session}))))
}

fn shell_words(cmd: &[String]) -> String {
    cmd.iter().map(|a| if a.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:".contains(c)) { a.clone() } else { format!("{a:?}") }).collect::<Vec<_>>().join(" ")
}

async fn session_for_user(s: &AppState, ctx: &RequestCtx, ns: &str, id: &str) -> Result<Uuid, ApiError> {
    let id: Uuid = id.parse().map_err(|_| ApiError::new(ErrorCode::NotFound, "no such exec session"))?;
    let sess = load(s, id).await?.filter(|x| x.namespace == ns).ok_or_else(|| ApiError::new(ErrorCode::NotFound, "no such exec session (expired?)"))?;
    if sess.actor != ctx.actor {
        return Err(ApiError::new(ErrorCode::Forbidden, "exec sessions belong to whoever started them"));
    }
    Ok(id)
}

async fn session_for_node(s: &AppState, ctx: &RequestCtx, name: &str, id: &str) -> Result<Uuid, ApiError> {
    if crate::node_of(ctx) != Some(name) {
        return Err(ApiError::new(ErrorCode::Forbidden, "only the instance's node joins an exec session"));
    }
    let id: Uuid = id.parse().map_err(|_| ApiError::new(ErrorCode::NotFound, "no such exec session"))?;
    load(s, id).await?.filter(|x| x.node == name).ok_or_else(|| ApiError::new(ErrorCode::NotFound, "no such exec session"))?;
    Ok(id)
}

async fn user_output(State(s): State<AppState>, Extension(ctx): Extension<RequestCtx>, Path((ns, id)): Path<(String, String)>) -> ApiResult<Body> {
    let id = session_for_user(&s, &ctx, &ns, &id).await?;
    let l = listen(&s, id, "up").await?;
    let n = sqlx::query("update exec_sessions set user_listening = true where id = $1 and not user_listening").bind(id).execute(s.store.pool()).await.map_err(db)?.rows_affected();
    if n == 0 {
        return Err(ApiError::new(ErrorCode::Conflict, "this session's output is already being read").into());
    }
    // don't hold the user on a session no node will join
    wait_claimed(&s, id).await?;
    Ok(stream_body(l, Some((s, id))))
}

async fn user_input(State(s): State<AppState>, Extension(ctx): Extension<RequestCtx>, Path((ns, id)): Path<(String, String)>, body: Body) -> ApiResult<StatusCode> {
    let id = session_for_user(&s, &ctx, &ns, &id).await?;
    wait_claimed(&s, id).await?;
    pump(&s, body, id, "down").await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn node_input(State(s): State<AppState>, Extension(ctx): Extension<RequestCtx>, Path((name, id)): Path<(String, String)>) -> ApiResult<Body> {
    let id = session_for_node(&s, &ctx, &name, &id).await?;
    let l = listen(&s, id, "down").await?;
    let n = sqlx::query("update exec_sessions set claimed = true where id = $1 and user_listening and not claimed").bind(id).execute(s.store.pool()).await.map_err(db)?.rows_affected();
    if n == 0 {
        return Err(ApiError::new(ErrorCode::Conflict, "session already joined").into());
    }
    Ok(stream_body(l, None))
}

async fn node_output(State(s): State<AppState>, Extension(ctx): Extension<RequestCtx>, Path((name, id)): Path<(String, String)>, body: Body) -> ApiResult<StatusCode> {
    let id = session_for_node(&s, &ctx, &name, &id).await?;
    pump(&s, body, id, "up").await?;
    Ok(StatusCode::NO_CONTENT)
}
