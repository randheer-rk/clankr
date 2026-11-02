//! Multi-cluster placement (ADR 0025): explicit cross-cluster routing of workflow runs.
//!
//!   POST /v1/namespaces/{ns}/workflows/{wf}/runs   {input, cluster}  -> the remote run (+ cluster)
//!   GET  /v1/namespaces/{ns}/remoteruns             runs started elsewhere from here
//!   GET  /v1/namespaces/{ns}/remoteruns/{id}        live status, fetched from the remote cluster
//!
//! A start is refused unless all of these hold:
//! - the namespace's PlacementPolicies allow the cluster's region;
//! - the remote cluster already holds the identical Workflow (same spec digest);
//! - it holds every secret the policies require.
//!
//! Nothing is copied over implicitly.

use axum::extract::{Extension, Path, State};
use axum::routing::get;
use axum::{Json, Router};
use clankr_types::federation::{ClusterSpec, admit_cluster, admit_placement_policy, placement_allowed};
use clankr_types::{ApiError, ErrorCode, RequestCtx};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{ApiResult, AppState};

pub const SYSTEM_NS: &str = "clankr-system";

pub fn routes() -> Router<AppState> {
    Router::new().route("/v1/namespaces/{ns}/remoteruns", get(list)).route("/v1/namespaces/{ns}/remoteruns/{id}", get(fetch))
}

fn deny(msg: String) -> ApiError {
    ApiError::new(ErrorCode::Forbidden, msg)
}

/// A remote cluster's API, authenticated with the ServiceAccount token kept for it.
struct Remote {
    name: String,
    spec: ClusterSpec,
    http: reqwest::Client,
    token: String,
}

impl Remote {
    async fn open(s: &AppState, ctx: &RequestCtx, name: &str) -> Result<Self, ApiError> {
        let spec = admit_cluster(&s.store.get(SYSTEM_NS, "Cluster", name).await.map_err(|_| ApiError::new(ErrorCode::NotFound, format!("no Cluster {name:?} in {SYSTEM_NS}")))?.spec)?;
        let r = &spec.token_secret_ref;
        let token = s.store.secret_read(ctx, &s.master, SYSTEM_NS, &r.name, &r.key, &format!("federation: cluster {name}")).await?;
        let mut b = reqwest::Client::builder().timeout(std::time::Duration::from_secs(30)).redirect(reqwest::redirect::Policy::none());
        if let Some(pem) = &spec.ca_pem {
            b = b.add_root_certificate(reqwest::Certificate::from_pem(pem.as_bytes()).map_err(|e| ApiError::new(ErrorCode::InvalidSpec, format!("cluster {name} caPem: {e}")))?);
        }
        let http = b.build().map_err(|e| ApiError::new(ErrorCode::StorageUnavailable, e.to_string()))?;
        Ok(Self { name: name.to_owned(), spec, http, token })
    }

    /// (status, body) of a call to the remote API.
    async fn call(&self, method: reqwest::Method, path: &str, body: Option<&Value>) -> Result<(u16, Value), ApiError> {
        let mut rb = self.http.request(method, format!("{}{path}", self.spec.endpoint.trim_end_matches('/'))).bearer_auth(&self.token);
        if let Some(b) = body {
            rb = rb.json(b);
        }
        let resp = rb.send().await.map_err(|e| ApiError::new(ErrorCode::ProviderUnavailable, format!("cluster {} unreachable: {}", self.name, e.without_url())))?;
        let status = resp.status().as_u16();
        Ok((status, resp.json().await.unwrap_or(Value::Null)))
    }
}

fn digest(spec: &Value) -> String {
    hex::encode(Sha256::digest(spec.to_string().as_bytes()))
}

/// Starts `workflow` on `cluster` after the placement checks; records it here.
pub(crate) async fn start_remote(s: &AppState, ctx: &RequestCtx, ns: &str, workflow: &str, cluster: &str, input: Value, key: Option<String>) -> Result<Value, ApiError> {
    if let Some(k) = &key
        && let Some((id, name)) = sqlx::query_as::<_, (Uuid, String)>("select remote_run_id, cluster from remote_runs where namespace = $1 and idempotency_key = $2").bind(ns).bind(k).fetch_optional(s.store.pool()).await.map_err(clankr_store::db)?
    {
        return refresh(s, ctx, ns, id, &name).await;
    }
    let refuse = |reason: String| {
        let (s, ctx) = (s.clone(), ctx.clone());
        let action = format!("start {ns}/{workflow} on cluster {cluster}");
        async move {
            let _ = s.store.audit_deny(&ctx, &action, &reason).await;
            deny(reason)
        }
    };
    let remote = Remote::open(s, ctx, cluster).await?;
    let policies: Vec<_> = s.store.list("PlacementPolicy", Some(ns), &Default::default()).await?.0.into_iter().map(|p| Ok((p.metadata.name.clone(), admit_placement_policy(&p.spec)?))).collect::<Result<_, ApiError>>()?;
    let secrets = match placement_allowed(&policies, cluster, &remote.spec) {
        Ok(v) => v,
        Err(e) => return Err(refuse(e).await),
    };
    // the remote must run exactly what we'd run here
    let local = s.store.get(ns, "Workflow", workflow).await?;
    let (st, theirs) = remote.call(reqwest::Method::GET, &format!("/v1/namespaces/{ns}/workflows/{workflow}"), None).await?;
    if st == 404 {
        return Err(refuse(format!("cluster {cluster} has no workflow {ns}/{workflow}; apply it there first")).await);
    }
    if st != 200 {
        return Err(ApiError::new(ErrorCode::ProviderUnavailable, format!("cluster {cluster}: reading the workflow returned {st}: {}", theirs["message"])));
    }
    if digest(&theirs["spec"]) != digest(&local.spec) {
        return Err(ApiError::new(ErrorCode::Conflict, format!("cluster {cluster}'s workflow {ns}/{workflow} differs from this cluster's (re-apply it there)")));
    }
    for sec in &secrets {
        let (st, _) = remote.call(reqwest::Method::GET, &format!("/v1/namespaces/{ns}/secrets/{sec}"), None).await?;
        if st != 200 {
            return Err(refuse(format!("cluster {cluster} lacks required secret {ns}/{sec}")).await);
        }
    }
    let key = key.unwrap_or_else(|| format!("{cluster}:{}", Uuid::new_v4()));
    let (st, run) = remote.call(reqwest::Method::POST, &format!("/v1/namespaces/{ns}/workflows/{workflow}/runs"), Some(&json!({"input": input, "idempotencyKey": key}))).await?;
    if st != 201 {
        let code = serde_json::from_value(run["code"].clone()).unwrap_or(ErrorCode::ProviderUnavailable);
        return Err(ApiError::new(code, format!("cluster {cluster} refused the run: {}", run["message"].as_str().unwrap_or("?"))));
    }
    let id: Uuid = run["runId"].as_str().and_then(|r| r.parse().ok()).ok_or_else(|| ApiError::new(ErrorCode::ProviderUnavailable, format!("cluster {cluster} returned no run id")))?;
    let mut tx = s.store.pool().begin().await.map_err(clankr_store::db)?;
    sqlx::query("insert into remote_runs (remote_run_id, namespace, workflow, cluster, region, idempotency_key, started_by, status) values ($1, $2, $3, $4, $5, $6, $7, $8) on conflict do nothing")
        .bind(id)
        .bind(ns)
        .bind(workflow)
        .bind(cluster)
        .bind(&remote.spec.region)
        .bind(&key)
        .bind(&ctx.actor)
        .bind(run["status"].as_str().unwrap_or("Running"))
        .execute(&mut *tx)
        .await
        .map_err(clankr_store::db)?;
    let target = clankr_store::AuditTarget { kind: "WorkflowRun", namespace: ns, name: &id.to_string(), uid: Some(id.to_string()) };
    clankr_store::audit_in_tx(&mut tx, ctx, &format!("start on cluster {cluster} ({})", remote.spec.region), Some(target), true, None, None).await?;
    tx.commit().await.map_err(clankr_store::db)?;
    Ok(with_cluster(run, cluster, &remote.spec.region))
}

fn with_cluster(mut run: Value, cluster: &str, region: &str) -> Value {
    run["cluster"] = json!({"name": cluster, "region": region});
    run
}

async fn refresh(s: &AppState, ctx: &RequestCtx, ns: &str, id: Uuid, cluster: &str) -> Result<Value, ApiError> {
    let remote = Remote::open(s, ctx, cluster).await?;
    let (st, run) = remote.call(reqwest::Method::GET, &format!("/v1/namespaces/{ns}/runs/{id}"), None).await?;
    if st != 200 {
        return Err(ApiError::new(ErrorCode::ProviderUnavailable, format!("cluster {cluster}: run {id} returned {st}")));
    }
    sqlx::query("update remote_runs set status = $2, updated_at = now() where remote_run_id = $1").bind(id).bind(run["status"].as_str().unwrap_or("Unknown")).execute(s.store.pool()).await.map_err(clankr_store::db)?;
    Ok(with_cluster(run, cluster, &remote.spec.region))
}

async fn list(State(s): State<AppState>, Path(ns): Path<String>) -> ApiResult<Json<Value>> {
    let rows: Value = sqlx::query_scalar(
        "select coalesce(jsonb_agg(jsonb_build_object('runId', remote_run_id, 'workflow', workflow, 'cluster', cluster, 'region', region,
             'status', status, 'startedBy', started_by, 'createdAt', created_at, 'updatedAt', updated_at) order by created_at desc), '[]')
         from remote_runs where namespace = $1",
    )
    .bind(&ns)
    .fetch_one(s.store.pool())
    .await
    .map_err(clankr_store::db)?;
    Ok(Json(rows))
}

async fn fetch(State(s): State<AppState>, Extension(ctx): Extension<RequestCtx>, Path((ns, id)): Path<(String, String)>) -> ApiResult<Json<Value>> {
    let id: Uuid = id.parse().map_err(|_| ApiError::new(ErrorCode::NotFound, "no such remote run"))?;
    let cluster: String = sqlx::query_scalar("select cluster from remote_runs where namespace = $1 and remote_run_id = $2")
        .bind(&ns)
        .bind(id)
        .fetch_optional(s.store.pool())
        .await
        .map_err(clankr_store::db)?
        .ok_or_else(|| ApiError::new(ErrorCode::NotFound, format!("no remote run {id}")))?;
    Ok(Json(refresh(&s, &ctx, &ns, id, &cluster).await?))
}
