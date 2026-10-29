//! Durable mailboxes: at-least-once delivery with visibility leases fenced by an epoch per
//! delivery, nack/redelivery, max-deliveries and TTL dead-lettering, operator requeue.

use chrono::{DateTime, Utc};
use clankr_types::{ApiError, ErrorCode};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::{Store, db};

pub const MAX_PAYLOAD_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    pub id: Uuid,
    pub seq: i64,
    pub sender: String,
    pub correlation_id: Option<String>,
    pub dedup_key: Option<String>,
    pub payload: Value,
    pub state: String,
    /// Fencing token of the current delivery; required to ack/nack.
    pub lease_epoch: i64,
    pub deliveries: i32,
    pub expires_at: DateTime<Utc>,
    pub dead_reason: Option<String>,
    pub created_at: DateTime<Utc>,
}

const COLS: &str = "id, seq, sender, correlation_id, dedup_key, payload, state, lease_epoch, deliveries, expires_at, dead_reason, created_at";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MailboxTick {
    /// Leases that lapsed (message back to ready, or dead at max deliveries).
    pub lapsed: u64,
    /// Messages dead-lettered for outliving their TTL.
    pub expired: u64,
}

fn stale(id: Uuid, epoch: i64) -> ApiError {
    ApiError::new(ErrorCode::StaleLease, format!("delivery epoch {epoch} of message {id} is no longer current"))
}

pub struct SendOpts<'a> {
    pub sender: &'a str,
    pub correlation_id: Option<&'a str>,
    pub dedup_key: Option<&'a str>,
    pub ttl_secs: i64,
    pub max_deliveries: i32,
}

impl Store {
    /// Returns the message and whether it was a duplicate of an earlier send with the same dedup key.
    pub async fn mailbox_send(&self, ns: &str, mailbox: &str, payload: &Value, o: SendOpts<'_>) -> Result<(Message, bool), ApiError> {
        let size = payload.to_string().len();
        if size > MAX_PAYLOAD_BYTES {
            return Err(ApiError::new(ErrorCode::QuotaExceeded, format!("payload is {size} bytes; max {MAX_PAYLOAD_BYTES} (send an artifact reference)")));
        }
        let ins: Option<Message> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "insert into mailbox_messages (id, namespace, mailbox, sender, correlation_id, dedup_key, payload, state, max_deliveries, expires_at)
             values ($1, $2, $3, $4, $5, $6, $7, 'ready', $8, now() + make_interval(secs => $9))
             on conflict (namespace, mailbox, dedup_key) do nothing returning {COLS}"
        )))
        .bind(Uuid::new_v4())
        .bind(ns)
        .bind(mailbox)
        .bind(o.sender)
        .bind(o.correlation_id)
        .bind(o.dedup_key)
        .bind(payload)
        .bind(o.max_deliveries)
        .bind(o.ttl_secs as f64)
        .fetch_optional(self.pool())
        .await
        .map_err(db)?;
        if let Some(m) = ins {
            sqlx::query("select pg_notify('clankr_mailbox', $1)").bind(format!("{ns}/{mailbox}")).execute(self.pool()).await.map_err(db)?;
            return Ok((m, false));
        }
        let existing: Message = sqlx::query_as(sqlx::AssertSqlSafe(format!("select {COLS} from mailbox_messages where namespace = $1 and mailbox = $2 and dedup_key = $3")))
            .bind(ns)
            .bind(mailbox)
            .bind(o.dedup_key)
            .fetch_one(self.pool())
            .await
            .map_err(db)?;
        Ok((existing, true))
    }

    /// Leases up to `max` ready messages in send order; each delivery bumps its epoch.
    pub async fn mailbox_receive(&self, ns: &str, mailbox: &str, owner: &str, max: i64, visibility_secs: f64) -> Result<Vec<Message>, ApiError> {
        sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "with next as (
                 select id from mailbox_messages
                 where namespace = $1 and mailbox = $2 and state = 'ready' and not_before <= now() and expires_at > now()
                 order by seq for update skip locked limit $3)
             update mailbox_messages m set state = 'leased', lease_epoch = m.lease_epoch + 1, lease_owner = $4,
                 lease_expires = now() + make_interval(secs => $5), deliveries = m.deliveries + 1
             from next where m.id = next.id
             returning {}",
            COLS.split(", ").map(|c| format!("m.{c}")).collect::<Vec<_>>().join(", ")
        )))
        .bind(ns)
        .bind(mailbox)
        .bind(max)
        .bind(owner)
        .bind(visibility_secs)
        .fetch_all(self.pool())
        .await
        .map_err(db)
        .map(|mut v: Vec<Message>| {
            v.sort_by_key(|m| m.seq);
            v
        })
    }

    /// Ack from the current delivery: done. A repeat from the same epoch succeeds; older epochs are StaleLease.
    pub async fn mailbox_ack(&self, ns: &str, mailbox: &str, id: Uuid, epoch: i64) -> Result<(), ApiError> {
        let row: Option<(String, i64)> = sqlx::query_as(
            "update mailbox_messages set state = 'acked', lease_owner = null, lease_expires = null
             where id = $1 and namespace = $2 and mailbox = $3 and lease_epoch = $4 and state in ('leased', 'acked')
             returning state, lease_epoch",
        )
        .bind(id)
        .bind(ns)
        .bind(mailbox)
        .bind(epoch)
        .fetch_optional(self.pool())
        .await
        .map_err(db)?;
        row.map(drop).ok_or_else(|| stale(id, epoch))
    }

    /// Gives a delivery back: redelivered after `delay_secs`, or dead-lettered at max deliveries.
    pub async fn mailbox_nack(&self, ns: &str, mailbox: &str, id: Uuid, epoch: i64, delay_secs: f64, reason: &str) -> Result<Message, ApiError> {
        sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "update mailbox_messages set
                 state = case when deliveries >= max_deliveries then 'dead' else 'ready' end,
                 dead_reason = case when deliveries >= max_deliveries then 'max deliveries reached: ' || $5 else null end,
                 not_before = now() + make_interval(secs => $6), lease_owner = null, lease_expires = null
             where id = $1 and namespace = $2 and mailbox = $3 and lease_epoch = $4 and state = 'leased'
             returning {COLS}"
        )))
        .bind(id)
        .bind(ns)
        .bind(mailbox)
        .bind(epoch)
        .bind(reason)
        .bind(delay_secs)
        .fetch_optional(self.pool())
        .await
        .map_err(db)?
        .ok_or_else(|| stale(id, epoch))
    }

    pub async fn mailbox_list(&self, ns: &str, mailbox: &str, state: Option<&str>, limit: i64) -> Result<Vec<Message>, ApiError> {
        sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "select {COLS} from mailbox_messages where namespace = $1 and mailbox = $2 and ($3::text is null or state = $3) order by seq limit $4"
        )))
        .bind(ns)
        .bind(mailbox)
        .bind(state)
        .bind(limit)
        .fetch_all(self.pool())
        .await
        .map_err(db)
    }

    /// Operator action: put a dead message back with a fresh delivery budget (CL-22-05 style).
    pub async fn mailbox_requeue(&self, ns: &str, mailbox: &str, id: Uuid, ttl_secs: i64) -> Result<Message, ApiError> {
        sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "update mailbox_messages set state = 'ready', deliveries = 0, dead_reason = null, not_before = now(),
                 expires_at = greatest(expires_at, now() + make_interval(secs => $4))
             where id = $1 and namespace = $2 and mailbox = $3 and state = 'dead' returning {COLS}"
        )))
        .bind(id)
        .bind(ns)
        .bind(mailbox)
        .bind(ttl_secs as f64)
        .fetch_optional(self.pool())
        .await
        .map_err(db)?
        .ok_or_else(|| ApiError::new(ErrorCode::Conflict, format!("message {id} is not dead-lettered")))
    }

    /// Housekeeping: lapsed leases go back to ready (or dead at max deliveries); expired TTLs die.
    pub async fn mailbox_tick(&self) -> Result<MailboxTick, ApiError> {
        let lapsed = sqlx::query(
            "update mailbox_messages set
                 state = case when deliveries >= max_deliveries then 'dead' else 'ready' end,
                 dead_reason = case when deliveries >= max_deliveries then 'max deliveries reached: lease expired' else null end,
                 lease_owner = null, lease_expires = null
             where state = 'leased' and lease_expires < now()",
        )
        .execute(self.pool())
        .await
        .map_err(db)?
        .rows_affected();
        let expired = sqlx::query("update mailbox_messages set state = 'dead', dead_reason = 'ttl expired' where state in ('ready', 'leased') and expires_at < now()")
            .execute(self.pool())
            .await
            .map_err(db)?
            .rows_affected();
        Ok(MailboxTick { lapsed, expired })
    }
}
