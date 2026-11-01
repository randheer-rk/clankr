//! Persistence for runs: one transaction per step (decide → append events → task/timer rows →
//! cached state → audit). Task leases are fenced by `lease_epoch` (CL-22-02/03).
//!
//! Lock order is always run row → task/timer row, so completion, cancel and timer firing can't
//! deadlock each other.

use std::time::Duration;

use chrono::{DateTime, SubsecRound, Utc};
use clankr_store::{AuditTarget, Store, Tx, audit_in_tx, db};
use clankr_types::{ApiError, ErrorCode, RequestCtx};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::Row;
use uuid::Uuid;

use crate::graph::SideEffects;
use crate::run::{Event, Reject, RunState, RunStatus, decide, evolve, replay, resolve_signal};
use crate::WorkflowSpec;

/// NOTIFY channel pinged whenever a task becomes ready, so lease long-polls wake up.
pub const TASKS_CHANNEL: &str = "clankr_tasks";

#[derive(Clone)]
pub struct Engine {
    store: Store,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunView {
    pub run_id: Uuid,
    pub namespace: String,
    pub workflow: String,
    pub workflow_generation: i64,
    pub graph_digest: String,
    pub status: RunStatus,
    pub seq: i64,
    pub state: RunState,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntry {
    pub seq: i64,
    pub at: DateTime<Utc>,
    pub event: Event,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LeasedTask {
    pub run_id: Uuid,
    pub task: String,
    pub node: String,
    pub attempt: i32,
    pub queue: String,
    /// Fencing token: must accompany heartbeat/complete/fail.
    pub epoch: i64,
    pub lease_expires: DateTime<Utc>,
    /// Same for every attempt of this node visit; pass it to external systems so a retried
    /// side effect is deduplicated on their end (CL-28-06).
    pub idempotency_key: String,
    /// Run data at lease time: `{"input": ..., "<node>": <result>, ...}`.
    pub input: Value,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct TickReport {
    pub leases_expired: u64,
    pub outcomes_unknown: usize,
    pub timers_fired: usize,
    pub approvals_expired: u64,
}

fn err(code: ErrorCode, msg: impl Into<String>) -> ApiError {
    ApiError::new(code, msg)
}

fn reject(r: Reject) -> ApiError {
    let code = match r {
        Reject::NotAnInput => ErrorCode::InvalidSpec,
        _ => ErrorCode::Conflict,
    };
    err(code, r.to_string())
}

fn run_not_found(run_id: Uuid) -> ApiError {
    err(ErrorCode::NotFound, format!("run {run_id} not found"))
}

/// Admission for a Workflow spec: parse + validate, all problems in one error.
pub fn admit_workflow(spec: &Value) -> Result<WorkflowSpec, ApiError> {
    let g: WorkflowSpec = serde_json::from_value(spec.clone()).map_err(|e| err(ErrorCode::InvalidSpec, format!("workflow spec: {e}")).field("spec"))?;
    g.validate().map_err(|es| {
        let msg = es.iter().map(ToString::to_string).collect::<Vec<_>>().join("; ");
        err(ErrorCode::InvalidSpec, msg).field(es[0].field.clone())
    })?;
    Ok(g)
}

impl Engine {
    pub fn new(store: Store) -> Self {
        Self { store }
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub async fn start(
        &self,
        ctx: &RequestCtx,
        ns: &str,
        workflow: &str,
        input: Value,
        idempotency_key: Option<&str>,
    ) -> Result<RunView, ApiError> {
        let wf = self.store.get(ns, "Workflow", workflow).await?;
        let g = admit_workflow(&wf.spec)?;
        let graph = serde_json::to_value(&g).expect("serializable");
        let digest = hex::encode(Sha256::digest(graph.to_string().as_bytes()));

        if let Some(key) = idempotency_key
            && let Some(run) = self.by_idempotency_key(ns, key).await?
        {
            return Ok(run);
        }
        let run_id = Uuid::new_v4();
        let mut tx = self.store.pool().begin().await.map_err(db)?;
        let ins = sqlx::query(
            "insert into workflow_runs (run_id, namespace, workflow, workflow_generation, graph, graph_digest, status, seq, state, idempotency_key, started_by)
             values ($1, $2, $3, $4, $5, $6, 'Running', 0, $7, $8, $9)",
        )
        .bind(run_id)
        .bind(ns)
        .bind(workflow)
        .bind(wf.metadata.generation)
        .bind(&graph)
        .bind(&digest)
        .bind(serde_json::to_value(RunState::default()).unwrap())
        .bind(idempotency_key)
        .bind(&ctx.actor)
        .execute(&mut *tx)
        .await;
        if let Err(e) = ins {
            // lost a race on the same idempotency key: return the winner's run
            if let (Some(key), Some("23505")) = (idempotency_key, e.as_database_error().and_then(|d| d.code()).as_deref()) {
                drop(tx);
                return self.by_idempotency_key(ns, key).await?.ok_or_else(|| run_not_found(run_id));
            }
            return Err(db(e));
        }
        step(&mut tx, run_id, ns, Event::RunStarted { input }).await?;
        audit(&mut tx, ctx, "start", ns, run_id).await?;
        tx.commit().await.map_err(db)?;
        self.get(ns, run_id).await
    }

    pub async fn by_idempotency_key(&self, ns: &str, key: &str) -> Result<Option<RunView>, ApiError> {
        let id: Option<Uuid> = sqlx::query_scalar("select run_id from workflow_runs where namespace = $1 and idempotency_key = $2")
            .bind(ns)
            .bind(key)
            .fetch_optional(self.store.pool())
            .await
            .map_err(db)?;
        match id {
            Some(id) => Ok(Some(self.get(ns, id).await?)),
            None => Ok(None),
        }
    }

    pub async fn signal(&self, ctx: &RequestCtx, ns: &str, run_id: Uuid, signal: &str, payload: Value) -> Result<RunView, ApiError> {
        if signal.starts_with(crate::run::APPROVAL_PREFIX) {
            // approver group, self-approval and digest checks live in the approvals API
            return Err(err(ErrorCode::Forbidden, "approvals are decided through the approvals API, not raw signals"));
        }
        let mut tx = self.store.pool().begin().await.map_err(db)?;
        step(&mut tx, run_id, ns, Event::SignalReceived { signal: signal.into(), payload }).await?;
        audit(&mut tx, ctx, &format!("signal {signal}"), ns, run_id).await?;
        tx.commit().await.map_err(db)?;
        self.get(ns, run_id).await
    }

    pub async fn cancel(&self, ctx: &RequestCtx, ns: &str, run_id: Uuid, reason: &str) -> Result<RunView, ApiError> {
        let mut tx = self.store.pool().begin().await.map_err(db)?;
        step(&mut tx, run_id, ns, Event::RunCancelled { reason: reason.into() }).await?;
        audit(&mut tx, ctx, "cancel", ns, run_id).await?;
        tx.commit().await.map_err(db)?;
        self.get(ns, run_id).await
    }

    pub async fn get(&self, ns: &str, run_id: Uuid) -> Result<RunView, ApiError> {
        let row = sqlx::query(
            "select run_id, namespace, workflow, workflow_generation, graph_digest, seq, state, created_at, updated_at
             from workflow_runs where run_id = $1 and namespace = $2",
        )
        .bind(run_id)
        .bind(ns)
        .fetch_optional(self.store.pool())
        .await
        .map_err(db)?
        .ok_or_else(|| run_not_found(run_id))?;
        let state: RunState = serde_json::from_value(row.get("state")).map_err(|e| err(ErrorCode::StorageUnavailable, e.to_string()))?;
        Ok(RunView {
            run_id,
            namespace: row.get("namespace"),
            workflow: row.get("workflow"),
            workflow_generation: row.get("workflow_generation"),
            graph_digest: row.get("graph_digest"),
            status: state.status,
            seq: row.get("seq"),
            state,
            created_at: row.get("created_at"),
            updated_at: row.get("updated_at"),
        })
    }

    pub async fn list(&self, ns: &str, workflow: Option<&str>, limit: i64) -> Result<Vec<RunView>, ApiError> {
        let ids: Vec<Uuid> = sqlx::query_scalar(
            "select run_id from workflow_runs where namespace = $1 and ($2::text is null or workflow = $2)
             order by created_at desc limit $3",
        )
        .bind(ns)
        .bind(workflow)
        .bind(limit)
        .fetch_all(self.store.pool())
        .await
        .map_err(db)?;
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            out.push(self.get(ns, id).await?);
        }
        Ok(out)
    }

    pub async fn history(&self, ns: &str, run_id: Uuid) -> Result<Vec<HistoryEntry>, ApiError> {
        self.get(ns, run_id).await?; // namespace check
        let rows = sqlx::query("select seq, at, event from workflow_events where run_id = $1 order by seq")
            .bind(run_id)
            .fetch_all(self.store.pool())
            .await
            .map_err(db)?;
        rows.into_iter()
            .map(|r| {
                Ok(HistoryEntry {
                    seq: r.get("seq"),
                    at: r.get("at"),
                    event: serde_json::from_value(r.get("event")).map_err(|e| err(ErrorCode::StorageUnavailable, e.to_string()))?,
                })
            })
            .collect()
    }

    /// Replays the stored history and checks it reproduces the cached state with a gap-free sequence.
    pub async fn verify(&self, ns: &str, run_id: Uuid) -> Result<(), ApiError> {
        let run = self.get(ns, run_id).await?;
        let h = self.history(ns, run_id).await?;
        let gapless = h.iter().enumerate().all(|(i, e)| e.seq == i as i64 + 1) && h.len() as i64 == run.seq;
        if !gapless {
            return Err(err(ErrorCode::OutcomeUnknown, format!("run {run_id}: history sequence has gaps")));
        }
        if replay(h.iter().map(|e| &e.event)) != run.state {
            return Err(err(ErrorCode::OutcomeUnknown, format!("run {run_id}: replay diverges from stored state")));
        }
        Ok(())
    }

    /// The queue a task belongs to (for authorizing who may report on it).
    pub async fn task_queue(&self, ns: &str, run_id: Uuid, task: &str) -> Result<String, ApiError> {
        sqlx::query_scalar("select queue from tasks where run_id = $1 and task = $2 and namespace = $3")
            .bind(run_id)
            .bind(task)
            .bind(ns)
            .fetch_optional(self.store.pool())
            .await
            .map_err(db)?
            .ok_or_else(|| err(ErrorCode::NotFound, format!("task {task} not found")))
    }

    /// Takes the oldest ready task on `queue`, bumping its fencing epoch. `None` if nothing is ready.
    #[tracing::instrument(name = "task.lease", skip(self, ttl), fields(task = tracing::field::Empty, run_id = tracing::field::Empty, epoch = tracing::field::Empty))]
    pub async fn lease(&self, ns: &str, queue: &str, owner: &str, ttl: Duration) -> Result<Option<LeasedTask>, ApiError> {
        let row = sqlx::query(
            "with next as (
                 select run_id, task from tasks
                 where namespace = $1 and queue = $2 and state = 'ready' and not_before <= now()
                 order by not_before, created_at
                 for update skip locked limit 1)
             update tasks t set state = 'leased', lease_epoch = t.lease_epoch + 1, lease_owner = $3,
                    lease_expires = now() + make_interval(secs => $4), deliveries = t.deliveries + 1
             from next where t.run_id = next.run_id and t.task = next.task
             returning t.run_id, t.task, t.node, t.attempt, t.queue, t.lease_epoch, t.lease_expires",
        )
        .bind(ns)
        .bind(queue)
        .bind(owner)
        .bind(ttl.as_secs_f64())
        .fetch_optional(self.store.pool())
        .await
        .map_err(db)?;
        let Some(r) = row else { return Ok(None) };
        let run_id: Uuid = r.get("run_id");
        let task: String = r.get("task");
        let span = tracing::Span::current();
        span.record("task", task.as_str());
        span.record("run_id", tracing::field::display(run_id));
        span.record("epoch", r.get::<i64, _>("lease_epoch"));
        let visit = task.rsplit_once('.').map_or(task.as_str(), |(v, _)| v);
        let idempotency_key = format!("{run_id}/{visit}");
        let input: Value = sqlx::query_scalar("select state->'data' from workflow_runs where run_id = $1")
            .bind(run_id)
            .fetch_one(self.store.pool())
            .await
            .map_err(db)?;
        Ok(Some(LeasedTask {
            run_id,
            idempotency_key,
            task,
            node: r.get("node"),
            attempt: r.get("attempt"),
            queue: r.get("queue"),
            epoch: r.get("lease_epoch"),
            lease_expires: r.get("lease_expires"),
            input,
        }))
    }

    /// Extends a lease; only the current epoch holder may.
    pub async fn heartbeat(&self, ns: &str, run_id: Uuid, task: &str, epoch: i64, ttl: Duration) -> Result<DateTime<Utc>, ApiError> {
        let exp: Option<DateTime<Utc>> = sqlx::query_scalar(
            "update tasks set lease_expires = now() + make_interval(secs => $5)
             where run_id = $1 and task = $2 and namespace = $3 and lease_epoch = $4 and state = 'leased'
             returning lease_expires",
        )
        .bind(run_id)
        .bind(task)
        .bind(ns)
        .bind(epoch)
        .bind(ttl.as_secs_f64())
        .fetch_optional(self.store.pool())
        .await
        .map_err(db)?;
        exp.ok_or_else(|| stale(task, epoch))
    }

    pub async fn complete(&self, ns: &str, run_id: Uuid, task: &str, epoch: i64, result: Value) -> Result<(), ApiError> {
        self.finish(ns, run_id, task, epoch, Event::TaskCompleted { task: task.into(), result }).await
    }

    pub async fn fail(&self, ns: &str, run_id: Uuid, task: &str, epoch: i64, error: &str, retryable: bool) -> Result<(), ApiError> {
        self.finish(ns, run_id, task, epoch, Event::TaskFailed { task: task.into(), error: error.into(), retryable }).await
    }

    /// Accepts a task outcome at most once per task (CL-22-03): a repeat from the same epoch is a
    /// no-op success, anything from an older epoch is StaleLease.
    #[tracing::instrument(name = "task.finish", skip(self, outcome), fields(run_id = %run_id, outcome = input_kind(&outcome)))]
    async fn finish(&self, ns: &str, run_id: Uuid, task: &str, epoch: i64, outcome: Event) -> Result<(), ApiError> {
        let mut tx = self.store.pool().begin().await.map_err(db)?;
        lock_run(&mut tx, run_id, ns).await?;
        let row = sqlx::query("select state, lease_epoch from tasks where run_id = $1 and task = $2 for update")
            .bind(run_id)
            .bind(task)
            .fetch_optional(&mut *tx)
            .await
            .map_err(db)?
            .ok_or_else(|| err(ErrorCode::NotFound, format!("task {task} not found")))?;
        let (state, current): (String, i64) = (row.get("state"), row.get("lease_epoch"));
        match (state.as_str(), current == epoch) {
            ("completed" | "failed", true) => return Ok(()),
            ("leased", true) => {}
            // the lease lapsed and the outcome was marked unknown, but the original holder
            // reports back: that's the real answer, so it resolves the ambiguity
            ("unknown", true) => {
                let payload = match &outcome {
                    Event::TaskCompleted { result, .. } => serde_json::json!({ "outcome": "completed", "result": result }),
                    Event::TaskFailed { retryable: true, .. } => serde_json::json!({ "outcome": "retry" }),
                    Event::TaskFailed { error, .. } => serde_json::json!({ "outcome": "failed", "error": error }),
                    _ => unreachable!(),
                };
                step(&mut tx, run_id, ns, Event::SignalReceived { signal: resolve_signal(task), payload }).await?;
                return tx.commit().await.map_err(db);
            }
            _ => return Err(stale(task, epoch)),
        }
        let new_state = if matches!(outcome, Event::TaskCompleted { .. }) { "completed" } else { "failed" };
        sqlx::query("update tasks set state = $3, finished_at = now() where run_id = $1 and task = $2")
            .bind(run_id)
            .bind(task)
            .bind(new_state)
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        step(&mut tx, run_id, ns, outcome).await?;
        tx.commit().await.map_err(db)
    }

    /// Housekeeping: return expired leases to the queue (the next lease bumps the epoch, fencing
    /// the old holder) and fire due timers. Safe to run concurrently from several processes.
    pub async fn tick(&self) -> Result<TickReport, ApiError> {
        let expired = sqlx::query(
            "update tasks set state = 'ready', lease_owner = null, lease_expires = null
             where state = 'leased' and lease_expires < now() and side_effects = 'idempotent'",
        )
        .execute(self.store.pool())
        .await
        .map_err(db)?
        .rows_affected();
        if expired > 0 {
            notify_tasks(self.store.pool()).await?;
        }
        let mut unknown = 0;
        loop {
            let lapsed = sqlx::query("select run_id, task, namespace from tasks where state = 'leased' and side_effects = 'unsafe' and lease_expires < now() limit 1")
                .fetch_optional(self.store.pool())
                .await
                .map_err(db)?;
            let Some(l) = lapsed else { break };
            let (run_id, task, ns): (Uuid, String, String) = (l.get("run_id"), l.get("task"), l.get("namespace"));
            let mut tx = self.store.pool().begin().await.map_err(db)?;
            lock_run(&mut tx, run_id, &ns).await?;
            let claimed = sqlx::query("update tasks set state = 'unknown' where run_id = $1 and task = $2 and state = 'leased' and lease_expires < now()")
                .bind(run_id)
                .bind(&task)
                .execute(&mut *tx)
                .await
                .map_err(db)?
                .rows_affected();
            if claimed == 1 {
                step(&mut tx, run_id, &ns, Event::TaskOutcomeUnknown { task }).await?;
                unknown += 1;
            }
            tx.commit().await.map_err(db)?;
        }
        let mut fired = 0;
        // ponytail: fires up to 100 timers per tick, one transaction each; batch if timer volume grows
        for _ in 0..100 {
            let due = sqlx::query("select t.run_id, t.timer, r.namespace from timers t join workflow_runs r using (run_id) where not t.fired and t.due_at <= now() order by t.due_at limit 1")
                .fetch_optional(self.store.pool())
                .await
                .map_err(db)?;
            let Some(due) = due else { break };
            let (run_id, timer, ns): (Uuid, String, String) = (due.get("run_id"), due.get("timer"), due.get("namespace"));
            let mut tx = self.store.pool().begin().await.map_err(db)?;
            lock_run(&mut tx, run_id, &ns).await?;
            let claimed = sqlx::query("update timers set fired = true where run_id = $1 and timer = $2 and not fired")
                .bind(run_id)
                .bind(&timer)
                .execute(&mut *tx)
                .await
                .map_err(db)?
                .rows_affected();
            if claimed == 1 {
                match step(&mut tx, run_id, &ns, Event::TimerFired { timer }).await {
                    Ok(_) | Err(ApiError { code: ErrorCode::Conflict, .. }) => {} // run already finished
                    Err(e) => return Err(e),
                }
                fired += 1;
            }
            tx.commit().await.map_err(db)?;
        }
        let approvals_expired = self.expire_approvals().await?;
        Ok(TickReport { leases_expired: expired, outcomes_unknown: unknown, timers_fired: fired, approvals_expired })
    }
}

fn stale(task: &str, epoch: i64) -> ApiError {
    err(ErrorCode::StaleLease, format!("lease epoch {epoch} for task {task} is no longer current"))
}

async fn lock_run(tx: &mut Tx, run_id: Uuid, ns: &str) -> Result<(), ApiError> {
    sqlx::query("select 1 from workflow_runs where run_id = $1 and namespace = $2 for update")
        .bind(run_id)
        .bind(ns)
        .fetch_optional(&mut **tx)
        .await
        .map_err(db)?
        .map(drop)
        .ok_or_else(|| run_not_found(run_id))
}

async fn notify_tasks(pool: &sqlx::PgPool) -> Result<(), ApiError> {
    sqlx::query("select pg_notify($1, '')").bind(TASKS_CHANNEL).execute(pool).await.map(drop).map_err(db)
}

pub(crate) async fn audit(tx: &mut Tx, ctx: &RequestCtx, action: &str, ns: &str, run_id: Uuid) -> Result<(), ApiError> {
    let id = run_id.to_string();
    let target = AuditTarget { kind: "WorkflowRun", namespace: ns, name: &id, uid: Some(id.clone()) };
    audit_in_tx(tx, ctx, action, Some(target), true, None, None).await
}

/// One durable step: decide on `input`, append the resulting events, materialize the task and
/// timer rows they imply, and store the folded state, all inside the caller's transaction.
#[tracing::instrument(name = "engine.step", skip(tx, input), fields(run_id = %run_id, ns, input = input_kind(&input), events = tracing::field::Empty, status = tracing::field::Empty))]
pub(crate) async fn step(tx: &mut Tx, run_id: Uuid, ns: &str, input: Event) -> Result<RunState, ApiError> {
    let row = sqlx::query("select graph, state, seq, started_by from workflow_runs where run_id = $1 and namespace = $2 for update")
        .bind(run_id)
        .bind(ns)
        .fetch_optional(&mut **tx)
        .await
        .map_err(db)?
        .ok_or_else(|| run_not_found(run_id))?;
    let corrupt = |e: serde_json::Error| err(ErrorCode::StorageUnavailable, format!("run {run_id} unreadable: {e}"));
    let g: WorkflowSpec = serde_json::from_value(row.get("graph")).map_err(corrupt)?;
    let mut state: RunState = serde_json::from_value(row.get("state")).map_err(corrupt)?;
    let mut seq: i64 = row.get("seq");

    let now = Utc::now().trunc_subsecs(6);
    let events = decide(&g, &state, input, now).map_err(reject)?;
    let mut task_ready = false;
    for e in &events {
        seq += 1;
        state = evolve(state, e);
        sqlx::query("insert into workflow_events (run_id, seq, at, event) values ($1, $2, $3, $4)")
            .bind(run_id)
            .bind(seq)
            .bind(now)
            .bind(serde_json::to_value(e).unwrap())
            .execute(&mut **tx)
            .await
            .map_err(db)?;
        match e {
            Event::TaskScheduled { task, node, attempt, queue, not_before } => {
                let side_effects = match g.nodes[node].side_effects {
                    SideEffects::Idempotent => "idempotent",
                    SideEffects::Unsafe => "unsafe",
                };
                sqlx::query(
                    "insert into tasks (run_id, task, namespace, node, attempt, queue, state, not_before, side_effects)
                     values ($1, $2, $3, $4, $5, $6, 'ready', $7, $8)",
                )
                .bind(run_id)
                .bind(task)
                .bind(ns)
                .bind(node)
                .bind(*attempt as i32)
                .bind(queue)
                .bind(not_before)
                .bind(side_effects)
                .execute(&mut **tx)
                .await
                .map_err(db)?;
                task_ready = true;
            }
            Event::SignalReceived { signal, payload } if signal.starts_with("resolve.") => {
                let done = if payload["outcome"] == "completed" { "completed" } else { "failed" };
                sqlx::query("update tasks set state = $3, finished_at = now() where run_id = $1 and task = $2 and state = 'unknown'")
                    .bind(run_id)
                    .bind(&signal["resolve.".len()..])
                    .bind(done)
                    .execute(&mut **tx)
                    .await
                    .map_err(db)?;
            }
            Event::TimerScheduled { timer, due_at, .. } => {
                sqlx::query("insert into timers (run_id, timer, due_at) values ($1, $2, $3)")
                    .bind(run_id)
                    .bind(timer)
                    .bind(due_at)
                    .execute(&mut **tx)
                    .await
                    .map_err(db)?;
            }
            Event::WaitStarted { node, .. } => {
                if let crate::graph::NodeKind::Approval { approver_group, timeout, .. } = &g.nodes[node].kind {
                    let started_by: Option<String> = row.get("started_by");
                    let deadline = timeout.as_deref().and_then(crate::graph::parse_duration).map(|d| now + chrono::Duration::from_std(d).unwrap_or_default());
                    crate::approvals::request(tx, run_id, ns, node, state.visits[node], approver_group, started_by.as_deref(), &state.data, deadline).await?;
                }
            }
            Event::RunCompleted | Event::RunFailed { .. } | Event::RunCancelled { .. } => {
                sqlx::query("update approvals set state = 'cancelled' where run_id = $1 and state = 'pending'")
                    .bind(run_id)
                    .execute(&mut **tx)
                    .await
                    .map_err(db)?;
                sqlx::query("update tasks set state = 'cancelled', finished_at = now() where run_id = $1 and state in ('ready', 'leased', 'unknown')")
                    .bind(run_id)
                    .execute(&mut **tx)
                    .await
                    .map_err(db)?;
                sqlx::query("update timers set fired = true where run_id = $1 and not fired")
                    .bind(run_id)
                    .execute(&mut **tx)
                    .await
                    .map_err(db)?;
            }
            _ => {}
        }
    }
    sqlx::query("update workflow_runs set state = $2, seq = $3, status = $4, updated_at = now() where run_id = $1")
        .bind(run_id)
        .bind(serde_json::to_value(&state).unwrap())
        .bind(seq)
        .bind(format!("{:?}", state.status))
        .execute(&mut **tx)
        .await
        .map_err(db)?;
    if task_ready {
        sqlx::query("select pg_notify($1, '')").bind(TASKS_CHANNEL).execute(&mut **tx).await.map_err(db)?;
    }
    let span = tracing::Span::current();
    span.record("events", events.len());
    span.record("status", format!("{:?}", state.status));
    Ok(state)
}

fn input_kind(e: &Event) -> &'static str {
    match e {
        Event::RunStarted { .. } => "RunStarted",
        Event::TaskCompleted { .. } => "TaskCompleted",
        Event::TaskFailed { .. } => "TaskFailed",
        Event::TimerFired { .. } => "TimerFired",
        Event::TaskOutcomeUnknown { .. } => "TaskOutcomeUnknown",
        Event::SignalReceived { .. } => "SignalReceived",
        Event::RunCancelled { .. } => "RunCancelled",
        _ => "other",
    }
}
