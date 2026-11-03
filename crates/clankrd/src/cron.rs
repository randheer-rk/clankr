//! The `AgentCronJob` controller: starts a job's workflow in each minute its schedule matches.
//! One replica runs it (leader lease `cron`). Each run's idempotency key names the job and the
//! minute, so a tick that two replicas or a restart both see still starts one run.
//! A minute missed while no controller ran is skipped, not backfilled.

use std::time::Duration;

use chrono::{DateTime, DurationRound, Utc};
use clankr_engine::Engine;
use clankr_store::Store;
use clankr_types::RequestCtx;
use clankr_types::schedule::{ConcurrencyPolicy, Cron, admit_cron_job};
use serde_json::json;

const LEASE: &str = "cron";

pub fn spawn(store: Store, engine: Engine, holder: String, every: Duration) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(every);
        loop {
            tick.tick().await;
            match store.acquire_leadership(LEASE, &holder, (every.as_secs_f64() * 4.0).max(10.0)).await {
                Ok(Some(_)) => {
                    if let Err(e) = run_due(&store, &engine, Utc::now()).await {
                        tracing::warn!(error = %e.message, "cron pass failed");
                    }
                }
                Ok(None) => {}
                Err(e) => tracing::warn!(error = %e, "cron lease"),
            }
        }
    })
}

/// Starts every job due in the minute containing `now`; returns how many runs it started.
pub async fn run_due(store: &Store, engine: &Engine, now: DateTime<Utc>) -> Result<usize, clankr_types::ApiError> {
    let minute = now.duration_trunc(chrono::TimeDelta::minutes(1)).expect("minute");
    let (jobs, _) = store.list("AgentCronJob", None, &Default::default()).await?;
    let mut started = 0;
    for job in jobs {
        let (ns, name) = (job.metadata.namespace.clone(), job.metadata.name.clone());
        let Ok(spec) = admit_cron_job(&job.spec) else { continue };
        let status = job.status.clone().unwrap_or_default();
        let last = status["lastScheduleTime"].as_str().and_then(|t| t.parse::<DateTime<Utc>>().ok());
        if spec.suspend || last.is_some_and(|l| l >= minute) || !Cron::parse(&spec.schedule).is_ok_and(|c| c.matches(minute)) {
            continue;
        }
        let mut next = json!({"lastScheduleTime": minute.to_rfc3339(), "lastRun": status["lastRun"]});
        let running = match status["lastRun"].as_str().and_then(|r| r.parse().ok()) {
            Some(id) => engine.get(&ns, id).await.is_ok_and(|r| !r.status.is_terminal()),
            None => false,
        };
        if running && spec.concurrency_policy == ConcurrencyPolicy::Forbid {
            next["lastSkipped"] = json!({"at": minute.to_rfc3339(), "reason": "the previous run is still going (concurrencyPolicy Forbid)"});
        } else {
            let ctx = RequestCtx { actor: format!("cronjob/{ns}/{name}"), request_id: format!("cron-{}", minute.timestamp()) };
            match engine.start(&ctx, &ns, &spec.workflow, spec.input.clone(), Some(&format!("cron/{name}/{}", minute.timestamp()))).await {
                Ok(run) => {
                    next["lastRun"] = json!(run.run_id);
                    started += 1;
                }
                Err(e) => next["lastError"] = json!(e.message),
            }
        }
        let ctx = RequestCtx { actor: "cron".into(), request_id: "cron".into() };
        let _ = store.update_status(&ctx, &ns, "AgentCronJob", &name, job.metadata.resource_version, next).await;
    }
    Ok(started)
}
