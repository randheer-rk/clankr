//! Authoritative metadata store on Postgres (PRD §36). Resources with CAS, change log for watch.

use chrono::Utc;
use clankr_types::{ApiError, ErrorCode, RequestCtx, Resource, WatchEvent, WatchEventType, validate_envelope};
use serde_json::Value;
use sqlx::postgres::{PgPool, PgPoolOptions};
use sqlx::types::Json;
use sqlx::{Postgres, Row, Transaction};
use std::collections::BTreeMap;
use uuid::Uuid;

mod audit;
pub mod cluster;
pub mod context;
pub mod mailbox;
pub mod secrets;
pub mod tokens;
#[cfg(feature = "embedded")]
pub mod embedded;

pub use audit::{AuditEvent, AuditTarget, record as audit_in_tx};

pub const CHANGES_CHANNEL: &str = "clankr_changes";

#[derive(Clone)]
pub struct Store {
    pool: PgPool,
}

pub type Tx = Transaction<'static, Postgres>;

impl Store {
    pub async fn connect(url: &str) -> anyhow::Result<Self> {
        let s = Self::open(url).await?;
        s.migrate().await?;
        Ok(s)
    }

    /// Connects without touching the schema (backup verification inspects it as-is).
    pub async fn open(url: &str) -> anyhow::Result<Self> {
        Ok(Self { pool: PgPoolOptions::new().max_connections(16).connect(url).await? })
    }

    pub async fn migrate(&self) -> anyhow::Result<()> {
        Ok(sqlx::migrate!().run(&self.pool).await?)
    }

    /// Newest schema migration this binary knows (backups record and check it).
    pub fn schema_version() -> i64 {
        sqlx::migrate!().iter().map(|m| m.version).max().unwrap_or(0)
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Current cluster-wide resource revision (bumped by every resource write).
    pub async fn revision(&self) -> Result<i64, ApiError> {
        sqlx::query_scalar("select rev from revision").fetch_one(&self.pool).await.map_err(db)
    }

    /// Serializes check-then-write sequences on one namespace across all clankrd replicas
    /// (quota admission). The lock lives as long as the returned transaction; dropping it
    /// (including on cancellation) rolls back and releases.
    pub async fn namespace_lock(&self, ns: &str) -> Result<sqlx::Transaction<'static, sqlx::Postgres>, ApiError> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        sqlx::query("select pg_advisory_xact_lock(hashtextextended('clankr-ns:' || $1, 0))").bind(ns).execute(&mut *tx).await.map_err(db)?;
        Ok(tx)
    }

    pub async fn ping(&self) -> Result<(), ApiError> {
        sqlx::query("select 1").execute(&self.pool).await.map(drop).map_err(db)
    }

    pub async fn create(&self, ctx: &RequestCtx, mut r: Resource) -> Result<Resource, ApiError> {
        validate_envelope(&r)?;
        let m = &mut r.metadata;
        m.uid = Some(Uuid::new_v4().to_string());
        m.generation = 1;
        m.creation_timestamp = Some(Utc::now());
        m.deletion_timestamp = None;
        r.status = None;

        let mut tx = self.pool.begin().await.map_err(db)?;
        let rev = commit_change(&mut tx, WatchEventType::Added, &mut r).await?;
        let res = sqlx::query("insert into resources (uid, namespace, kind, name, rv, object) values ($1, $2, $3, $4, $5, $6)")
            .bind(uid(&r))
            .bind(&r.metadata.namespace)
            .bind(&r.kind)
            .bind(&r.metadata.name)
            .bind(rev)
            .bind(Json(&r))
            .execute(&mut *tx)
            .await;
        if let Err(e) = res {
            return Err(if is_unique_violation(&e) {
                ApiError::new(ErrorCode::AlreadyExists, format!("{} {}/{} already exists", r.kind, r.metadata.namespace, r.metadata.name))
            } else {
                db(e)
            });
        }
        audit::allow(&mut tx, ctx, "create", &r).await?;
        tx.commit().await.map_err(db)?;
        Ok(r)
    }

    pub async fn get(&self, namespace: &str, kind: &str, name: &str) -> Result<Resource, ApiError> {
        let row = sqlx::query("select object from resources where namespace = $1 and kind = $2 and name = $3")
            .bind(namespace)
            .bind(kind)
            .bind(name)
            .fetch_optional(&self.pool)
            .await
            .map_err(db)?;
        row.map(object).ok_or_else(|| not_found(kind, namespace, name))
    }

    /// Lists matching resources and the revision they are consistent at; pass that revision to
    /// [`Store::changes_since`] to watch without gaps. `namespace: None` lists across namespaces.
    pub async fn list(
        &self,
        kind: &str,
        namespace: Option<&str>,
        labels: &BTreeMap<String, String>,
    ) -> Result<(Vec<Resource>, i64), ApiError> {
        let mut tx = self.snapshot_tx().await?;
        let rows = sqlx::query(
            "select object from resources
             where kind = $1 and ($2::text is null or namespace = $2)
               and coalesce(object->'metadata'->'labels', '{}') @> $3
             order by namespace, name",
        )
        .bind(kind)
        .bind(namespace)
        .bind(Json(labels))
        .fetch_all(&mut *tx)
        .await
        .map_err(db)?;
        let rev: i64 = sqlx::query_scalar("select rev from revision").fetch_one(&mut *tx).await.map_err(db)?;
        tx.commit().await.map_err(db)?;
        Ok((rows.into_iter().map(object).collect(), rev))
    }

    /// Replaces spec, labels, annotations and finalizers. Fails with Conflict unless
    /// `r.metadata.resource_version` is the stored one (CL-19-03). Status is not writable here (CL-19-02).
    pub async fn update(&self, ctx: &RequestCtx, mut r: Resource) -> Result<Resource, ApiError> {
        validate_envelope(&r)?;
        let mut tx = self.pool.begin().await.map_err(db)?;
        let cur = lock_current(&mut tx, &r.metadata.namespace, &r.kind, &r.metadata.name).await?;
        check_rv(&cur, r.metadata.resource_version)?;

        let m = &mut r.metadata;
        m.uid = cur.metadata.uid.clone();
        m.creation_timestamp = cur.metadata.creation_timestamp;
        m.deletion_timestamp = cur.metadata.deletion_timestamp;
        m.generation = cur.metadata.generation + i64::from(r.spec != cur.spec);
        r.status = cur.status;

        if r.metadata.deletion_timestamp.is_some() && r.metadata.finalizers.is_empty() {
            remove(&mut tx, &mut r).await?;
        } else {
            write(&mut tx, WatchEventType::Modified, &mut r).await?;
        }
        audit::allow(&mut tx, ctx, "update", &r).await?;
        tx.commit().await.map_err(db)?;
        Ok(r)
    }

    /// Controller-only path: replaces status, nothing else (CL-19-02).
    pub async fn update_status(
        &self,
        ctx: &RequestCtx,
        namespace: &str,
        kind: &str,
        name: &str,
        resource_version: i64,
        status: Value,
    ) -> Result<Resource, ApiError> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        let mut r = lock_current(&mut tx, namespace, kind, name).await?;
        check_rv(&r, resource_version)?;
        r.status = Some(status);
        write(&mut tx, WatchEventType::Modified, &mut r).await?;
        audit::allow(&mut tx, ctx, "update_status", &r).await?;
        tx.commit().await.map_err(db)?;
        Ok(r)
    }

    /// With finalizers present the resource only enters Terminating (deletionTimestamp set);
    /// it is removed when an update clears the last finalizer (CL-19-05).
    pub async fn delete(
        &self,
        ctx: &RequestCtx,
        namespace: &str,
        kind: &str,
        name: &str,
        resource_version: Option<i64>,
    ) -> Result<Resource, ApiError> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        let mut r = lock_current(&mut tx, namespace, kind, name).await?;
        if let Some(rv) = resource_version {
            check_rv(&r, rv)?;
        }
        if r.metadata.finalizers.is_empty() {
            remove(&mut tx, &mut r).await?;
        } else if r.metadata.deletion_timestamp.is_none() {
            r.metadata.deletion_timestamp = Some(Utc::now());
            write(&mut tx, WatchEventType::Modified, &mut r).await?;
        }
        audit::allow(&mut tx, ctx, "delete", &r).await?;
        tx.commit().await.map_err(db)?;
        Ok(r)
    }

    /// Records a denied request (authn/authz failure) in its own transaction.
    pub async fn audit_deny(&self, ctx: &RequestCtx, action: &str, reason: &str) -> Result<(), ApiError> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        audit::deny(&mut tx, ctx, action, reason).await?;
        tx.commit().await.map_err(db)
    }

    /// Records an allowed action on a named target in its own transaction.
    pub async fn audit_in_tx_simple(&self, ctx: &RequestCtx, action: &str, name: &str) -> Result<(), ApiError> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        let target = audit::AuditTarget { kind: "Node", namespace: "", name, uid: None };
        audit::record(&mut tx, ctx, action, Some(target), true, None, None).await?;
        tx.commit().await.map_err(db)
    }

    /// Audit records after `after_seq`, oldest first.
    /// Where the off-site export to `dest` has got to: (seq, hash) of the last row sent.
    pub async fn audit_export_cursor(&self, dest: &str) -> Result<(i64, String), ApiError> {
        sqlx::query("insert into audit_exports (dest) values ($1) on conflict do nothing").bind(dest).execute(self.pool()).await.map_err(db)?;
        sqlx::query_as("select seq, hash from audit_exports where dest = $1").bind(dest).fetch_one(self.pool()).await.map_err(db)
    }

    pub async fn set_audit_export_cursor(&self, dest: &str, seq: i64, hash: &str) -> Result<(), ApiError> {
        sqlx::query("update audit_exports set seq = $2, hash = $3 where dest = $1").bind(dest).bind(seq).bind(hash).execute(self.pool()).await.map(drop).map_err(db)
    }

    /// The audit row at `seq`, if there is one.
    pub async fn audit_at(&self, seq: i64) -> Result<Option<AuditEvent>, ApiError> {
        sqlx::query_as("select * from audit_events where seq = $1").bind(seq).fetch_optional(self.pool()).await.map_err(db)
    }

    pub async fn audit_since(&self, after_seq: i64, limit: i64) -> Result<Vec<AuditEvent>, ApiError> {
        audit::since(&self.pool, after_seq, limit).await
    }

    /// The newest `limit` records, newest first.
    pub async fn audit_latest(&self, limit: i64) -> Result<Vec<AuditEvent>, ApiError> {
        audit::latest(&self.pool, limit).await
    }

    /// Recomputes the hash chain; returns the first seq whose hash doesn't match, if any.
    pub async fn verify_audit_chain(&self) -> Result<Option<i64>, ApiError> {
        audit::verify(&self.pool).await
    }

    /// Changes with revision > `after`, oldest first, at most `limit`.
    /// Returns HistoryExpired if `after` predates retained history; the client must relist.
    pub async fn changes_since(
        &self,
        after: i64,
        kind: Option<&str>,
        namespace: Option<&str>,
        limit: i64,
    ) -> Result<Vec<(i64, WatchEvent)>, ApiError> {
        let mut tx = self.snapshot_tx().await?;
        let compacted: i64 = sqlx::query_scalar("select compacted from revision").fetch_one(&mut *tx).await.map_err(db)?;
        if after < compacted {
            return Err(ApiError::new(
                ErrorCode::HistoryExpired,
                format!("revision {after} compacted (oldest retained is {}); relist", compacted + 1),
            ));
        }
        let rows = sqlx::query(
            "select rev, change, object from resource_changes
             where rev > $1 and ($2::text is null or kind = $2) and ($3::text is null or namespace = $3)
             order by rev limit $4",
        )
        .bind(after)
        .bind(kind)
        .bind(namespace)
        .bind(limit)
        .fetch_all(&mut *tx)
        .await
        .map_err(db)?;
        tx.commit().await.map_err(db)?;
        Ok(rows
            .into_iter()
            .map(|row| {
                let kind = match row.get::<&str, _>("change") {
                    "Added" => WatchEventType::Added,
                    "Modified" => WatchEventType::Modified,
                    _ => WatchEventType::Deleted,
                };
                (row.get("rev"), WatchEvent { kind, object: row.get::<Json<Resource>, _>("object").0 })
            })
            .collect())
    }

    /// Drops change history up to and including `upto`.
    pub async fn compact(&self, upto: i64) -> Result<(), ApiError> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        let upto: i64 = sqlx::query_scalar("update revision set compacted = greatest(compacted, least($1, rev)) returning compacted")
            .bind(upto)
            .fetch_one(&mut *tx)
            .await
            .map_err(db)?;
        sqlx::query("delete from resource_changes where rev <= $1").bind(upto).execute(&mut *tx).await.map_err(db)?;
        tx.commit().await.map_err(db)
    }

    /// Latest committed revision, pushed via LISTEN/NOTIFY. Watchers re-query on each change;
    /// a missed notification only delays them until their next keepalive poll.
    pub async fn change_notifier(&self) -> anyhow::Result<tokio::sync::watch::Receiver<i64>> {
        self.notifier(CHANGES_CHANNEL).await
    }

    /// LISTEN on `channel`; the receiver's value is the latest numeric payload (0 if not numeric).
    pub async fn notifier(&self, channel: &str) -> anyhow::Result<tokio::sync::watch::Receiver<i64>> {
        let mut listener = sqlx::postgres::PgListener::connect_with(&self.pool).await?;
        listener.listen(channel).await?;
        let (tx, rx) = tokio::sync::watch::channel(0);
        tokio::spawn(async move {
            // recv() reconnects on its own after connection loss
            while !tx.is_closed() {
                if let Ok(n) = listener.recv().await {
                    let v = n.payload().parse().unwrap_or(0);
                    // send_modify always marks changed, even for repeated payloads
                    tx.send_modify(|cur| *cur = v);
                }
            }
        });
        Ok(rx)
    }

    async fn snapshot_tx(&self) -> Result<Tx, ApiError> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        sqlx::query("set transaction isolation level repeatable read, read only").execute(&mut *tx).await.map_err(db)?;
        Ok(tx)
    }
}

/// Bumps the global revision, stamps it on `r`, appends the change and notifies watchers.
async fn commit_change(tx: &mut Tx, change: WatchEventType, r: &mut Resource) -> Result<i64, ApiError> {
    let rev: i64 = sqlx::query_scalar("update revision set rev = rev + 1 returning rev").fetch_one(&mut **tx).await.map_err(db)?;
    r.metadata.resource_version = rev;
    sqlx::query("insert into resource_changes (rev, namespace, kind, change, object) values ($1, $2, $3, $4, $5)")
        .bind(rev)
        .bind(&r.metadata.namespace)
        .bind(&r.kind)
        .bind(format!("{change:?}"))
        .bind(Json(&*r))
        .execute(&mut **tx)
        .await
        .map_err(db)?;
    sqlx::query("select pg_notify($1, $2)").bind(CHANGES_CHANNEL).bind(rev.to_string()).execute(&mut **tx).await.map_err(db)?;
    Ok(rev)
}

async fn write(tx: &mut Tx, change: WatchEventType, r: &mut Resource) -> Result<(), ApiError> {
    let rev = commit_change(tx, change, r).await?;
    sqlx::query("update resources set rv = $2, object = $3 where uid = $1")
        .bind(uid(r))
        .bind(rev)
        .bind(Json(&*r))
        .execute(&mut **tx)
        .await
        .map_err(db)?;
    Ok(())
}

async fn remove(tx: &mut Tx, r: &mut Resource) -> Result<(), ApiError> {
    commit_change(tx, WatchEventType::Deleted, r).await?;
    sqlx::query("delete from resources where uid = $1").bind(uid(r)).execute(&mut **tx).await.map_err(db)?;
    Ok(())
}

async fn lock_current(tx: &mut Tx, namespace: &str, kind: &str, name: &str) -> Result<Resource, ApiError> {
    let row = sqlx::query("select object from resources where namespace = $1 and kind = $2 and name = $3 for update")
        .bind(namespace)
        .bind(kind)
        .bind(name)
        .fetch_optional(&mut **tx)
        .await
        .map_err(db)?;
    row.map(object).ok_or_else(|| not_found(kind, namespace, name))
}

fn check_rv(cur: &Resource, expected: i64) -> Result<(), ApiError> {
    if cur.metadata.resource_version == expected {
        return Ok(());
    }
    Err(ApiError::new(
        ErrorCode::Conflict,
        format!("resourceVersion {expected} is stale (current {}); re-read and retry", cur.metadata.resource_version),
    ))
}

fn object(row: sqlx::postgres::PgRow) -> Resource {
    row.get::<Json<Resource>, _>("object").0
}

fn uid(r: &Resource) -> Uuid {
    r.metadata.uid.as_deref().and_then(|u| u.parse().ok()).expect("stored resource has a uid")
}

fn not_found(kind: &str, namespace: &str, name: &str) -> ApiError {
    ApiError::new(ErrorCode::NotFound, format!("{kind} {namespace}/{name} not found"))
}

fn is_unique_violation(e: &sqlx::Error) -> bool {
    e.as_database_error().and_then(|d| d.code()).as_deref() == Some("23505")
}

/// Maps any database failure to StorageUnavailable.
pub fn db(e: sqlx::Error) -> ApiError {
    ApiError::new(ErrorCode::StorageUnavailable, e.to_string())
}
