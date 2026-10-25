//! Cluster state: leader leases, nodes and instance placement (PRD §23).

use chrono::{DateTime, Utc};
use clankr_types::{ApiError, ErrorCode};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{Store, Tx, db};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct NodeRow {
    pub name: String,
    pub labels: sqlx::types::Json<Value>,
    pub capacity: sqlx::types::Json<Value>,
    pub ready: bool,
    pub cordoned: bool,
    pub draining: bool,
    pub last_heartbeat: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Instance {
    pub id: String,
    pub namespace: String,
    pub deployment: String,
    pub agent: String,
    pub node: Option<String>,
    pub epoch: i64,
    pub memory_mib: i64,
    pub cpus: f64,
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct VolumeBinding {
    pub namespace: String,
    pub claim: String,
    pub node: String,
}

pub fn hash_credential(c: &str) -> String {
    hex::encode(Sha256::digest(c.as_bytes()))
}

/// Proof of current leadership, checked inside every fenced transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Leadership {
    pub name: String,
    pub holder: String,
    pub epoch: i64,
}

impl Store {
    /// Acquires or renews `name` for `holder`. Returns the epoch while held, None while someone
    /// else holds an unexpired lease. A new holder always gets a higher epoch.
    pub async fn acquire_leadership(&self, name: &str, holder: &str, ttl_secs: f64) -> Result<Option<Leadership>, ApiError> {
        let epoch: Option<i64> = sqlx::query_scalar(
            "insert into leader_leases (name, holder, epoch, expires_at) values ($1, $2, 1, now() + make_interval(secs => $3))
             on conflict (name) do update set
                 epoch = case when leader_leases.holder = excluded.holder then leader_leases.epoch else leader_leases.epoch + 1 end,
                 holder = excluded.holder,
                 expires_at = excluded.expires_at
             where leader_leases.holder = excluded.holder or leader_leases.expires_at < now()
             returning epoch",
        )
        .bind(name)
        .bind(holder)
        .bind(ttl_secs)
        .fetch_optional(self.pool())
        .await
        .map_err(db)?;
        Ok(epoch.map(|epoch| Leadership { name: name.into(), holder: holder.into(), epoch }))
    }

    /// Locks the lease row and fails with StaleLease unless `l` is still the live leadership.
    /// Call first inside a transaction; every write after it is fenced (CL-36-03).
    pub async fn fence(tx: &mut Tx, l: &Leadership) -> Result<(), ApiError> {
        let ok: Option<bool> = sqlx::query_scalar("select holder = $2 and epoch = $3 and expires_at > now() from leader_leases where name = $1 for update")
            .bind(&l.name)
            .bind(&l.holder)
            .bind(l.epoch)
            .fetch_optional(&mut **tx)
            .await
            .map_err(db)?;
        if ok != Some(true) {
            return Err(ApiError::new(ErrorCode::StaleLease, format!("leadership {} epoch {} of {} is no longer current", l.name, l.epoch, l.holder)));
        }
        Ok(())
    }

    /// Registers (or re-registers) a node with a fresh credential.
    pub async fn register_node(&self, name: &str, credential: &str, labels: &Value, capacity: &Value) -> Result<(), ApiError> {
        sqlx::query(
            "insert into nodes (name, credential_hash, labels, capacity, ready, last_heartbeat) values ($1, $2, $3, $4, true, now())
             on conflict (name) do update set credential_hash = excluded.credential_hash, labels = excluded.labels,
                 capacity = excluded.capacity, ready = true, last_heartbeat = now(),
                 prev_credential_hash = null, prev_valid_until = null",
        )
        .bind(name)
        .bind(hash_credential(credential))
        .bind(labels)
        .bind(capacity)
        .execute(self.pool())
        .await
        .map_err(db)
        .map(drop)
    }

    /// Rotates a node's credential (certificate fingerprint). The previous one stays valid for
    /// `grace_secs` so requests already in flight on old connections complete.
    pub async fn rotate_node_credential(&self, name: &str, credential: &str, grace_secs: f64) -> Result<(), ApiError> {
        sqlx::query(
            "update nodes set prev_credential_hash = credential_hash, prev_valid_until = now() + make_interval(secs => $3),
                 credential_hash = $2 where name = $1",
        )
        .bind(name)
        .bind(hash_credential(credential))
        .bind(grace_secs)
            .execute(self.pool())
            .await
            .map_err(db)
            .map(drop)
    }

    /// The node a credential belongs to, if any.
    pub async fn node_for_credential(&self, credential: &str) -> Result<Option<String>, ApiError> {
        sqlx::query_scalar("select name from nodes where credential_hash = $1 or (prev_credential_hash = $1 and prev_valid_until > now())")
            .bind(hash_credential(credential))
            .fetch_optional(self.pool())
            .await
            .map_err(db)
    }

    /// Records a heartbeat and returns the node's current assignments.
    pub async fn node_heartbeat(&self, name: &str) -> Result<Vec<Instance>, ApiError> {
        sqlx::query("update nodes set last_heartbeat = now(), ready = true where name = $1").bind(name).execute(self.pool()).await.map_err(db)?;
        self.instances_on(name).await
    }

    pub async fn instances_on(&self, node: &str) -> Result<Vec<Instance>, ApiError> {
        sqlx::query_as("select id, namespace, deployment, agent, node, epoch, memory_mib, cpus, note from instances where node = $1 order by id")
            .bind(node)
            .fetch_all(self.pool())
            .await
            .map_err(db)
    }

    pub async fn nodes(&self) -> Result<Vec<NodeRow>, ApiError> {
        sqlx::query_as("select name, labels, capacity, ready, cordoned, draining, last_heartbeat from nodes order by name").fetch_all(self.pool()).await.map_err(db)
    }

    pub async fn instances(&self) -> Result<Vec<Instance>, ApiError> {
        sqlx::query_as("select id, namespace, deployment, agent, node, epoch, memory_mib, cpus, note from instances order by id").fetch_all(self.pool()).await.map_err(db)
    }

    /// Where an instance is placed right now: (node, epoch), or None if unknown/unplaced.
    pub async fn instance_placement(&self, id: &str) -> Result<Option<Instance>, ApiError> {
        sqlx::query_as("select id, namespace, deployment, agent, node, epoch, memory_mib, cpus, note from instances where id = $1")
            .bind(id)
            .fetch_optional(self.pool())
            .await
            .map_err(db)
    }
}

impl Store {
    /// cordon: no new placements. drain: cordon + move instances off (volume-pinned ones stay).
    pub async fn set_node_maintenance(&self, name: &str, cordoned: bool, draining: bool) -> Result<(), ApiError> {
        let n = sqlx::query("update nodes set cordoned = $2, draining = $3 where name = $1")
            .bind(name)
            .bind(cordoned)
            .bind(draining)
            .execute(self.pool())
            .await
            .map_err(db)?
            .rows_affected();
        if n == 0 {
            return Err(ApiError::new(ErrorCode::NotFound, format!("node {name} not found")));
        }
        Ok(())
    }

    pub async fn volume_bindings(&self) -> Result<Vec<VolumeBinding>, ApiError> {
        sqlx::query_as("select namespace, claim, node from volume_bindings order by namespace, claim").fetch_all(self.pool()).await.map_err(db)
    }
}
