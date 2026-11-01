//! ResourceQuota admission and the namespace usage report (PLA-015, CL-31-12).
//!
//! Writes that could raise usage take a per-namespace advisory lock, recompute usage with the
//! new object in place and refuse with QuotaExceeded if a limit would be crossed. A namespace
//! already over a lowered quota can still shrink: only increases are refused.

use std::collections::BTreeMap;

use axum::extract::{Path, State};
use axum::routing::get;
use axum::{Json, Router};
use clankr_store::Store;
use clankr_types::quota::{admit_agent_budget, admit_resource_quota};
use clankr_types::{ApiError, ErrorCode, Resource, plural};
use serde_json::{Value, json};

use crate::{ApiResult, AppState};

pub fn routes() -> Router<AppState> {
    Router::new().route("/v1/namespaces/{ns}/usage", get(usage))
}

/// Strictest limit per key across the namespace's ResourceQuotas.
async fn hard(store: &Store, ns: &str) -> Result<BTreeMap<String, f64>, ApiError> {
    let mut hard: BTreeMap<String, f64> = BTreeMap::new();
    for q in store.list("ResourceQuota", Some(ns), &Default::default()).await?.0 {
        for (k, v) in admit_resource_quota(&q.spec)?.hard {
            hard.entry(k).and_modify(|h| *h = h.min(v)).or_insert(v);
        }
    }
    Ok(hard)
}

/// Usage for `keys`, with `replace` standing in for its stored version (a create or update).
async fn measure(store: &Store, ns: &str, keys: &[&String], replace: Option<&Resource>) -> Result<BTreeMap<String, f64>, ApiError> {
    let list = |kind: &'static str| async move {
        let mut items = store.list(kind, Some(ns), &Default::default()).await?.0;
        if let Some(r) = replace.filter(|r| r.kind == kind) {
            items.retain(|i| i.metadata.name != r.metadata.name);
            items.push(r.clone());
        }
        Ok::<_, ApiError>(items)
    };
    let mut used = BTreeMap::new();
    if keys.iter().any(|k| matches!(k.as_str(), "cpus" | "memoryMiB" | "instances")) {
        let agents: BTreeMap<String, Resource> = list("Agent").await?.into_iter().map(|a| (a.metadata.name.clone(), a)).collect();
        let (mut cpus, mut mem, mut inst) = (0.0, 0.0, 0.0);
        for d in list("AgentDeployment").await? {
            let Ok(d) = clankr_types::agent::admit_deployment(&d.spec) else { continue };
            // a deployment of a missing Agent places nothing until the Agent exists, then counts
            let Some(Ok(a)) = agents.get(&d.agent).map(|a| clankr_types::agent::admit_agent(&a.spec)) else { continue };
            let n = f64::from(d.replicas);
            (cpus, mem, inst) = (cpus + n * a.resources.cpus, mem + n * a.resources.memory_mib as f64, inst + n);
        }
        used.extend([("cpus".to_owned(), cpus), ("memoryMiB".to_owned(), mem), ("instances".to_owned(), inst)]);
    }
    if keys.iter().any(|k| *k == "volumeMiB") {
        let total = list("VolumeClaim").await?.iter().filter_map(|v| clankr_types::agent::admit_volume_claim(&v.spec).ok()).map(|v| v.capacity_mib as f64).sum();
        used.insert("volumeMiB".into(), total);
    }
    for k in keys {
        if let Some(kind) = k.strip_prefix("count/").and_then(clankr_types::kind_for) {
            used.insert((*k).clone(), list(kind).await?.len() as f64);
        }
    }
    if keys.iter().any(|k| *k == "activeRuns") {
        let n: i64 = sqlx::query_scalar("select count(*) from workflow_runs where namespace = $1 and status = 'Running'")
            .bind(ns)
            .fetch_one(store.pool())
            .await
            .map_err(clankr_store::db)?;
        used.insert("activeRuns".into(), n as f64);
    }
    Ok(used)
}

fn exceeded(key: &str, hard: f64, now: f64, after: f64) -> ApiError {
    ApiError::new(ErrorCode::QuotaExceeded, format!("ResourceQuota {key}: {after} would exceed the limit {hard} (in use: {now})"))
}

/// Checks a create/update against the namespace's quotas. Returns the lock guard; hold it until
/// the write has committed.
pub(crate) async fn admit(store: &Store, r: &Resource) -> Result<Option<sqlx::Transaction<'static, sqlx::Postgres>>, ApiError> {
    let ns = &r.metadata.namespace;
    // relevant: the object's own count, plus compute/volume totals for the kinds that feed them
    let affects = |k: &str| match k {
        "cpus" | "memoryMiB" | "instances" => matches!(r.kind.as_str(), "Agent" | "AgentDeployment"),
        "volumeMiB" => r.kind == "VolumeClaim",
        _ => k.strip_prefix("count/") == Some(plural(&r.kind).as_str()),
    };
    if !hard(store, ns).await?.keys().any(|k| affects(k)) {
        return Ok(None);
    }
    let guard = store.namespace_lock(ns).await?;
    // re-read under the lock: a quota created or tightened meanwhile counts
    let hard = hard(store, ns).await?;
    let keys: Vec<&String> = hard.keys().filter(|k| affects(k)).collect();
    let (now, after) = (measure(store, ns, &keys, None).await?, measure(store, ns, &keys, Some(r)).await?);
    for k in keys {
        let (n, a) = (now[k], after[k]);
        if a > hard[k] + 1e-9 && a > n {
            return Err(exceeded(k, hard[k], n, a));
        }
    }
    Ok(Some(guard))
}

/// `activeRuns` check for a run start; hold the guard until the run row is committed.
pub(crate) async fn admit_run(store: &Store, ns: &str) -> Result<Option<sqlx::Transaction<'static, sqlx::Postgres>>, ApiError> {
    if !hard(store, ns).await?.contains_key("activeRuns") {
        return Ok(None);
    }
    let guard = store.namespace_lock(ns).await?;
    let hard = hard(store, ns).await?;
    let key = "activeRuns".to_owned();
    if let Some(&max) = hard.get(&key) {
        let n = measure(store, ns, &[&key], None).await?[&key];
        if n + 1.0 > max + 1e-9 {
            return Err(exceeded(&key, max, n, n + 1.0));
        }
    }
    Ok(Some(guard))
}

/// Quotas (limit vs. use) and budgets (current window) for one namespace: the data behind the
/// budgets dashboard and `clank usage`.
async fn usage(State(s): State<AppState>, Path(ns): Path<String>) -> ApiResult<Json<Value>> {
    let hard = hard(&s.store, &ns).await?;
    let used = measure(&s.store, &ns, &hard.keys().collect::<Vec<_>>(), None).await?;
    let quotas: Vec<Value> = hard.iter().map(|(k, h)| json!({"resource": k, "hard": h, "used": used.get(k).copied().unwrap_or(0.0)})).collect();

    let counter = |key: String, scope: String| {
        let s = s.clone();
        let ns = ns.clone();
        async move {
            let row: Option<(i64, i64)> = sqlx::query_as("select used, reserved from model_usage where namespace = $1 and claim = $2 and scope = $3")
                .bind(&ns)
                .bind(&key)
                .bind(&scope)
                .fetch_optional(s.store.pool())
                .await
                .map_err(clankr_store::db)?;
            Ok::<_, ApiError>(row.unwrap_or_default())
        }
    };
    let now = chrono::Utc::now();
    let mut budgets = vec![];
    for b in s.store.list("AgentBudget", Some(&ns), &Default::default()).await?.0 {
        let spec = admit_agent_budget(&b.spec)?;
        let (name, window) = (b.metadata.name.clone(), spec.period.window(now));
        let mut v = json!({"name": name, "period": spec.period, "window": window});
        if let Some(max) = spec.max_tokens {
            let (u, r) = counter(format!("budget:{name}"), window.clone()).await?;
            v["tokens"] = json!({"limit": max, "used": u, "reserved": r});
        }
        if let Some(max) = spec.max_cost_usd {
            let (u, r) = counter(format!("budget-cost:{name}"), window.clone()).await?;
            v["costUSD"] = json!({"limit": max, "used": u as f64 / 1e6, "reserved": r as f64 / 1e6});
        }
        budgets.push(v);
    }
    // spend per claim this month, from the call ledger
    let rows: Vec<(String, i64, i64, Option<i64>, i64)> = sqlx::query_as(
        "select claim, coalesce(sum(input_tokens), 0)::bigint, coalesce(sum(output_tokens), 0)::bigint, sum(cost_micro_usd)::bigint, count(*)
         from model_calls where namespace = $1 and at >= date_trunc('month', now() at time zone 'utc') at time zone 'utc'
         group by claim order by claim",
    )
    .bind(&ns)
    .fetch_all(s.store.pool())
    .await
    .map_err(clankr_store::db)?;
    let claims: Vec<Value> = rows
        .into_iter()
        .map(|(claim, i, o, c, n)| json!({"claim": claim, "calls": n, "inputTokens": i, "outputTokens": o, "costUSD": c.map(|c| c as f64 / 1e6)}))
        .collect();
    Ok(Json(json!({"namespace": ns, "quotas": quotas, "budgets": budgets, "modelSpendThisMonth": claims})))
}
