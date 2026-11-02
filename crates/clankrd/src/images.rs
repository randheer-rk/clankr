//! Image promotion by digest (BLD-17, CL-24-07):
//!
//!   POST /v1/namespaces/{ns}/agentimages/{name}/promotions   {stage, digest, reason?, evidence?}
//!   POST /v1/namespaces/{ns}/agentimages/{name}/rollback     {stage, reason?}
//!   GET  /v1/namespaces/{ns}/agentimages/{name}/promotions   (?stage=)  newest first
//!
//! A digest enters stage N only if it was promoted to stage N-1 first; a stage may require an
//! evaluation report with a minimum pass rate. Every Agent tracking the stage gets its
//! `runtime.image` rewritten in the same request (audited as the promoter), and the scheduler
//! restarts its instances on the new digest. Rollback re-promotes the stage's previous digest.

use axum::extract::{Extension, Path, Query, State};
use axum::routing::post;
use axum::{Json, Router};
use clankr_store::Store;
use clankr_types::agent::{AgentImageSpec, admit_agent, admit_agent_image, is_digest};
use clankr_types::{ApiError, ErrorCode, RequestCtx, Resource};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{ApiResult, AppState};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/namespaces/{ns}/agentimages/{name}/promotions", post(promote).get(history))
        .route("/v1/namespaces/{ns}/agentimages/{name}/rollback", post(rollback))
}

fn bad(field: &str, msg: impl Into<String>) -> ApiError {
    ApiError::new(ErrorCode::InvalidSpec, msg).field(field)
}

async fn image(store: &Store, ns: &str, name: &str) -> Result<AgentImageSpec, ApiError> {
    admit_agent_image(&store.get(ns, "AgentImage", name).await?.spec)
}

/// Current digest of each stage, newest promotion wins.
pub(crate) async fn current(store: &Store, ns: &str, name: &str, stage: &str) -> Result<Option<String>, ApiError> {
    sqlx::query_scalar("select digest from image_promotions where namespace = $1 and image = $2 and stage = $3 order by id desc limit 1")
        .bind(ns)
        .bind(name)
        .bind(stage)
        .fetch_optional(store.pool())
        .await
        .map_err(clankr_store::db)
}

/// The namespace's ImagePolicies, by name.
pub(crate) async fn policies(store: &Store, ns: &str) -> Result<Vec<(String, clankr_types::agent::ImagePolicySpec)>, ApiError> {
    store
        .list("ImagePolicy", Some(ns), &Default::default())
        .await?
        .0
        .into_iter()
        .map(|p| Ok((p.metadata.name.clone(), clankr_types::agent::admit_image_policy(&p.spec)?)))
        .collect()
}

/// Agents must satisfy the namespace's ImagePolicies (repositories, runtime); signatures are
/// checked by the node at launch, where the registry is reachable. Agents that track a stage
/// must run exactly what that stage holds (no hand-edited drift).
pub(crate) async fn admit_tracking(store: &Store, r: &Resource) -> Result<(), ApiError> {
    if r.kind != "Agent" {
        return Ok(());
    }
    let a = admit_agent(&r.spec)?;
    clankr_types::agent::image_allowed(&policies(store, &r.metadata.namespace).await?, &a.runtime).map_err(|e| ApiError::new(ErrorCode::Forbidden, e).field("spec.runtime"))?;
    let Some(t) = &a.track else { return Ok(()) };
    let img = image(store, &r.metadata.namespace, &t.image).await.map_err(|e| bad("spec.track.image", e.message))?;
    if !img.stages.contains(&t.stage) {
        return Err(bad("spec.track.stage", format!("AgentImage {} has no stage {:?} (stages: {})", t.image, t.stage, img.stages.join(", "))));
    }
    let clankr_types::agent::Runtime::Oci { image: running } = &a.runtime else {
        return Err(bad("spec.runtime", "tracking an AgentImage needs the oci runtime"));
    };
    let want = current(store, &r.metadata.namespace, &t.image, &t.stage).await?.ok_or_else(|| bad("spec.track", format!("nothing has been promoted to {}/{} yet", t.image, t.stage)))?;
    let expected = format!("{}@{want}", img.repository);
    if *running != expected {
        return Err(bad("spec.runtime.image", format!("tracks {}/{}, which is {expected}; promote instead of editing the image", t.image, t.stage)));
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PromoteBody {
    stage: String,
    digest: String,
    #[serde(default)]
    reason: String,
    /// e.g. {"eval": {"suite": "...", "passRate": 0.95, "report": "<sha256>"}} from `clank eval`
    #[serde(default)]
    evidence: Option<Value>,
}

async fn promote(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Path((ns, name)): Path<(String, String)>,
    Json(b): Json<PromoteBody>,
) -> ApiResult<Json<Value>> {
    if !is_digest(&b.digest) {
        return Err(bad("digest", "want sha256:<64 lowercase hex>; tags are mutable and can't be promoted").into());
    }
    let img = image(&s.store, &ns, &name).await?;
    let idx = img.stages.iter().position(|x| *x == b.stage).ok_or_else(|| bad("stage", format!("unknown stage {:?}; stages: {}", b.stage, img.stages.join(", "))))?;
    if idx > 0 {
        let prev = &img.stages[idx - 1];
        let seen: bool = sqlx::query_scalar("select exists(select 1 from image_promotions where namespace = $1 and image = $2 and stage = $3 and digest = $4)")
            .bind(&ns)
            .bind(&name)
            .bind(prev)
            .bind(&b.digest)
            .fetch_one(s.store.pool())
            .await
            .map_err(clankr_store::db)?;
        if !seen {
            return Err(ApiError::new(ErrorCode::Forbidden, format!("{} was never in {prev}; promote it there first (stages go {})", short(&b.digest), img.stages.join(" → "))).into());
        }
    }
    if let Some(min) = img.gates.get(&b.stage).and_then(|g| g.min_eval_pass_rate) {
        let rate = b.evidence.as_ref().and_then(|e| e["eval"]["passRate"].as_f64());
        match rate {
            Some(r) if r + 1e-9 >= min => {}
            Some(r) => return Err(ApiError::new(ErrorCode::Forbidden, format!("{} requires an evaluation pass rate of {:.0}%; evidence shows {:.0}%", b.stage, min * 100.0, r * 100.0)).into()),
            None => return Err(ApiError::new(ErrorCode::Forbidden, format!("{} requires evaluation evidence (evidence.eval.passRate ≥ {min})", b.stage)).into()),
        }
    }
    Ok(Json(apply(&s, &ctx, &ns, &name, &img, &b.stage, &b.digest, &b.reason, b.evidence, false).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RollbackBody {
    stage: String,
    #[serde(default)]
    reason: String,
}

async fn rollback(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Path((ns, name)): Path<(String, String)>,
    Json(b): Json<RollbackBody>,
) -> ApiResult<Json<Value>> {
    let img = image(&s.store, &ns, &name).await?;
    if !img.stages.contains(&b.stage) {
        return Err(bad("stage", format!("unknown stage {:?}", b.stage)).into());
    }
    let cur = current(&s.store, &ns, &name, &b.stage).await?.ok_or_else(|| ApiError::new(ErrorCode::Conflict, format!("nothing promoted to {} yet", b.stage)))?;
    // the newest earlier digest that isn't the current one
    let prev: Option<String> = sqlx::query_scalar("select digest from image_promotions where namespace = $1 and image = $2 and stage = $3 and digest <> $4 order by id desc limit 1")
        .bind(&ns)
        .bind(&name)
        .bind(&b.stage)
        .bind(&cur)
        .fetch_optional(s.store.pool())
        .await
        .map_err(clankr_store::db)?;
    let prev = prev.ok_or_else(|| ApiError::new(ErrorCode::Conflict, format!("{} has no earlier digest to roll back to", b.stage)))?;
    let reason = if b.reason.is_empty() { format!("rollback from {}", short(&cur)) } else { b.reason };
    Ok(Json(apply(&s, &ctx, &ns, &name, &img, &b.stage, &prev, &reason, None, true).await?))
}

fn short(d: &str) -> String {
    d.chars().take(19).collect()
}

/// Records the promotion and rewrites every Agent tracking the stage. The promotion row goes
/// in first; an Agent update that loses a race is retried once with a fresh read.
#[allow(clippy::too_many_arguments)]
async fn apply(s: &AppState, ctx: &RequestCtx, ns: &str, name: &str, img: &AgentImageSpec, stage: &str, digest: &str, reason: &str, evidence: Option<Value>, rollback: bool) -> Result<Value, ApiError> {
    // one promotion per namespace at a time, across clankrd replicas: "previous" stays true
    let _guard = s.store.namespace_lock(ns).await?;
    let previous = current(&s.store, ns, name, stage).await?;
    if previous.as_deref() == Some(digest) {
        return Err(ApiError::new(ErrorCode::Conflict, format!("{stage} already runs {}", short(digest))));
    }
    let mut tx = s.store.pool().begin().await.map_err(clankr_store::db)?;
    let id: i64 = sqlx::query_scalar(
        "insert into image_promotions (namespace, image, stage, digest, previous_digest, promoted_by, reason, evidence, rollback)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9) returning id",
    )
    .bind(ns)
    .bind(name)
    .bind(stage)
    .bind(digest)
    .bind(&previous)
    .bind(&ctx.actor)
    .bind(reason)
    .bind(&evidence)
    .bind(rollback)
    .fetch_one(&mut *tx)
    .await
    .map_err(clankr_store::db)?;
    let action = format!("{} {stage} {} → {}", if rollback { "rollback" } else { "promote" }, previous.as_deref().map_or("(none)".into(), short), short(digest));
    let target = clankr_store::AuditTarget { kind: "AgentImage", namespace: ns, name, uid: None };
    clankr_store::audit_in_tx(&mut tx, ctx, &action, Some(target), true, (!reason.is_empty()).then_some(reason), None).await?;
    tx.commit().await.map_err(clankr_store::db)?;

    let image_ref = format!("{}@{digest}", img.repository);
    let mut updated = vec![];
    for agent in s.store.list("Agent", Some(ns), &Default::default()).await?.0 {
        let tracks = |r: &Resource| r.spec["track"]["image"] == name && r.spec["track"]["stage"] == stage;
        if !tracks(&agent) {
            continue;
        }
        let mut r = agent;
        for attempt in 0..2 {
            r.spec["runtime"]["image"] = Value::String(image_ref.clone());
            match s.store.update(ctx, r.clone()).await {
                Ok(_) => break,
                Err(e) if e.code == ErrorCode::Conflict && attempt == 0 => r = s.store.get(ns, "Agent", &r.metadata.name).await?,
                Err(e) => return Err(e),
            }
        }
        updated.push(r.metadata.name.clone());
    }
    Ok(json!({"id": id, "image": name, "stage": stage, "digest": digest, "previousDigest": previous, "rollback": rollback, "agentsUpdated": updated}))
}

#[derive(Deserialize)]
struct HistoryQuery {
    stage: Option<String>,
}

async fn history(State(s): State<AppState>, Path((ns, name)): Path<(String, String)>, Query(q): Query<HistoryQuery>) -> ApiResult<Json<Value>> {
    let rows: Value = sqlx::query_scalar(
        "select coalesce(jsonb_agg(jsonb_build_object('id', id, 'stage', stage, 'digest', digest, 'previousDigest', previous_digest,
             'promotedBy', promoted_by, 'at', at, 'reason', reason, 'evidence', evidence, 'rollback', rollback) order by id desc), '[]')
         from image_promotions where namespace = $1 and image = $2 and ($3::text is null or stage = $3)",
    )
    .bind(&ns)
    .bind(&name)
    .bind(&q.stage)
    .fetch_one(s.store.pool())
    .await
    .map_err(clankr_store::db)?;
    Ok(Json(rows))
}
