//! Approval inbox (HUM-01/02, SEC-17). Each visit of an approval node opens one request, bound
//! to that run, node, visit and the run data at that moment (its digest). Deciding checks the
//! approver's group, refuses self-approval, requires the digest the approver reviewed, and
//! resumes the run in the same transaction, so a request is decided at most once.

use chrono::{DateTime, Utc};
use clankr_store::{Tx, db};
use clankr_types::{ApiError, ErrorCode, RequestCtx};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::Engine;
use crate::durable::{audit, step};
use crate::run::{Event, approval_signal};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Approval {
    pub id: Uuid,
    pub namespace: String,
    pub run_id: Uuid,
    pub node: String,
    pub visit: i32,
    pub approver_group: String,
    pub requested_by: Option<String>,
    pub subject: sqlx::types::Json<Value>,
    pub digest: String,
    pub state: String,
    pub deadline: Option<DateTime<Utc>>,
    pub decided_by: Option<String>,
    pub decided_at: Option<DateTime<Utc>>,
    pub comment: Option<String>,
    pub created_at: DateTime<Utc>,
}

const COLS: &str = "id, namespace, run_id, node, visit, approver_group, requested_by, subject, digest, state, deadline, decided_by, decided_at, comment, created_at";

/// Who is deciding: their actor string, groups, and whether this is the break-glass owner
/// (exempt from group membership and self-approval, still audited).
pub struct Decider<'a> {
    pub ctx: &'a RequestCtx,
    pub groups: &'a [String],
    pub break_glass: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Decision {
    Approve,
    Reject,
}

fn digest(run_id: Uuid, node: &str, visit: u32, subject: &Value) -> String {
    let canon = json!({"run": run_id, "node": node, "visit": visit, "subject": subject});
    hex::encode(Sha256::digest(canon.to_string().as_bytes()))
}

/// Opens the request for an approval node visit (called from the step that emitted WaitStarted).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn request(
    tx: &mut Tx,
    run_id: Uuid,
    ns: &str,
    node: &str,
    visit: u32,
    group: &str,
    requested_by: Option<&str>,
    data: &Map<String, Value>,
    deadline: Option<DateTime<Utc>>,
) -> Result<(), ApiError> {
    let subject = Value::Object(data.clone());
    sqlx::query(
        "insert into approvals (id, namespace, run_id, node, visit, approver_group, requested_by, subject, digest, deadline)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
    )
    .bind(Uuid::new_v4())
    .bind(ns)
    .bind(run_id)
    .bind(node)
    .bind(visit as i32)
    .bind(group)
    .bind(requested_by)
    .bind(&subject)
    .bind(digest(run_id, node, visit, &subject))
    .bind(deadline)
    .execute(&mut **tx)
    .await
    .map_err(db)
    .map(drop)
}

fn forbidden(msg: impl Into<String>) -> ApiError {
    ApiError::new(ErrorCode::Forbidden, msg)
}

impl Engine {
    pub async fn approvals(&self, ns: &str, state: Option<&str>, limit: i64) -> Result<Vec<Approval>, ApiError> {
        sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "select {COLS} from approvals where namespace = $1 and ($2::text is null or state = $2) order by created_at desc limit $3"
        )))
        .bind(ns)
        .bind(state)
        .bind(limit)
        .fetch_all(self.store().pool())
        .await
        .map_err(db)
    }

    pub async fn approval(&self, ns: &str, id: Uuid) -> Result<Approval, ApiError> {
        sqlx::query_as(sqlx::AssertSqlSafe(format!("select {COLS} from approvals where id = $1 and namespace = $2")))
            .bind(id)
            .bind(ns)
            .fetch_optional(self.store().pool())
            .await
            .map_err(db)?
            .ok_or_else(|| ApiError::new(ErrorCode::NotFound, format!("approval {id} not found")))
    }

    /// Decides a pending request and resumes the run, atomically. `digest` must be the one the
    /// approver reviewed, so an approval can't be applied to a different action.
    pub async fn decide_approval(&self, who: &Decider<'_>, ns: &str, id: Uuid, decision: Decision, digest: &str, comment: &str) -> Result<Approval, ApiError> {
        let mut tx = self.store().pool().begin().await.map_err(db)?;
        // run row first (engine lock order), then the approval row
        let run_id: Uuid = sqlx::query_scalar("select run_id from approvals where id = $1 and namespace = $2")
            .bind(id)
            .bind(ns)
            .fetch_optional(&mut *tx)
            .await
            .map_err(db)?
            .ok_or_else(|| ApiError::new(ErrorCode::NotFound, format!("approval {id} not found")))?;
        sqlx::query("select 1 from workflow_runs where run_id = $1 for update").bind(run_id).execute(&mut *tx).await.map_err(db)?;
        let a: Approval = sqlx::query_as(sqlx::AssertSqlSafe(format!("select {COLS} from approvals where id = $1 for update")))
            .bind(id)
            .fetch_one(&mut *tx)
            .await
            .map_err(db)?;
        if a.state != "pending" {
            return Err(ApiError::new(ErrorCode::Conflict, format!("approval {id} is already {}", a.state)));
        }
        if a.digest != digest {
            return Err(ApiError::new(ErrorCode::Conflict, "digest doesn't match this approval request; review it again and use its current digest").field("digest"));
        }
        if !who.break_glass {
            if !who.groups.contains(&a.approver_group) {
                return Err(forbidden(format!("only members of group {:?} can decide this approval", a.approver_group)));
            }
            if a.requested_by.as_deref() == Some(who.ctx.actor.as_str()) {
                return Err(forbidden("you started this run; someone else has to approve it"));
            }
        }
        let state = match decision {
            Decision::Approve => "approved",
            Decision::Reject => "rejected",
        };
        let a: Approval = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "update approvals set state = $2, decided_by = $3, decided_at = now(), comment = $4 where id = $1 returning {COLS}"
        )))
        .bind(id)
        .bind(state)
        .bind(&who.ctx.actor)
        .bind(comment)
        .fetch_one(&mut *tx)
        .await
        .map_err(db)?;
        let payload = json!({"approved": decision == Decision::Approve, "by": who.ctx.actor, "comment": comment, "approval": id});
        step(&mut tx, run_id, ns, Event::SignalReceived { signal: approval_signal(&a.node), payload }).await?;
        let how = if who.break_glass { " (break-glass)" } else { "" };
        audit(&mut tx, who.ctx, &format!("approval {state} {}{how}", a.node), ns, run_id).await?;
        tx.commit().await.map_err(db)?;
        Ok(a)
    }

    /// Rejects requests past their deadline. Returns how many expired.
    pub async fn expire_approvals(&self) -> Result<u64, ApiError> {
        let due: Vec<(Uuid, Uuid, String, String)> =
            sqlx::query_as("select id, run_id, namespace, node from approvals where state = 'pending' and deadline <= now() limit 100")
                .fetch_all(self.store().pool())
                .await
                .map_err(db)?;
        let ctx = RequestCtx { actor: "system:approval-timeout".into(), request_id: Uuid::new_v4().to_string() };
        let mut n = 0;
        for (id, run_id, ns, node) in due {
            let mut tx = self.store().pool().begin().await.map_err(db)?;
            sqlx::query("select 1 from workflow_runs where run_id = $1 for update").bind(run_id).execute(&mut *tx).await.map_err(db)?;
            let claimed = sqlx::query("update approvals set state = 'expired', decided_at = now() where id = $1 and state = 'pending'")
                .bind(id)
                .execute(&mut *tx)
                .await
                .map_err(db)?
                .rows_affected();
            if claimed == 1 {
                let payload = json!({"approved": false, "reason": "approval timed out", "approval": id});
                step(&mut tx, run_id, &ns, Event::SignalReceived { signal: approval_signal(&node), payload }).await?;
                audit(&mut tx, &ctx, &format!("approval expired {node}"), &ns, run_id).await?;
                n += 1;
            }
            tx.commit().await.map_err(db)?;
        }
        Ok(n)
    }
}
