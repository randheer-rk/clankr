//! Scheduler (PRD §23): keeps every AgentDeployment at its replica count by placing instances
//! on Ready nodes. One active scheduler cluster-wide via a fenced Postgres leader lease; each
//! reconcile pass is one transaction that re-checks leadership first (CL-23-05, CL-36-03).

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use clankr_store::Store;
use clankr_store::cluster::{Instance, Leadership, NodeRow};
use clankr_types::agent::{Canary, NodeCapacity, admit_agent, admit_deployment};
use clankr_types::{ApiError, ErrorCode};
use serde::Serialize;
use serde_json::Value;

pub const LEASE: &str = "scheduler";
/// A node missing heartbeats this long is NotReady and its instances are moved.
pub const NODE_TIMEOUT_SECS: i64 = 10;

/// What one instance needs.
#[derive(Debug, Clone)]
pub struct Requirements {
    pub memory_mib: u64,
    pub cpus: f64,
    pub runtime: &'static str,
    pub selector: BTreeMap<String, String>,
    /// VolumeClaims the instance mounts (local-path, ReadWriteOnce).
    pub claims: Vec<String>,
}

/// A node as the scheduler sees it, with what's already placed on it.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub name: String,
    pub ready: bool,
    /// Cordoned or draining: no new placements.
    pub cordoned: bool,
    pub labels: BTreeMap<String, String>,
    pub capacity: NodeCapacity,
    pub used_memory_mib: u64,
    pub used_cpus: f64,
}

/// Hard filters, then the node with the most free memory (spreads load). Deterministic: ties
/// break by name. Err explains why nothing fits (CL-23-03/04).
pub fn place(req: &Requirements, nodes: &[Candidate]) -> Result<String, String> {
    let mut why = BTreeMap::<&str, usize>::new();
    let mut best: Option<(&Candidate, f64)> = None;
    for n in nodes {
        let reject = if !n.ready {
            Some("not ready")
        } else if n.cordoned {
            Some("cordoned")
        } else if !n.capacity.runtimes.iter().any(|r| r == req.runtime) {
            Some("runtime not supported")
        } else if !req.selector.iter().all(|(k, v)| n.labels.get(k) == Some(v)) {
            Some("nodeSelector mismatch")
        } else if n.used_memory_mib + req.memory_mib > n.capacity.memory_mib {
            Some("insufficient memory")
        } else if n.used_cpus + req.cpus > n.capacity.cpus + 1e-9 {
            Some("insufficient cpu")
        } else {
            None
        };
        if let Some(r) = reject {
            *why.entry(r).or_default() += 1;
            continue;
        }
        let free = (n.capacity.memory_mib - n.used_memory_mib - req.memory_mib) as f64;
        if best.is_none_or(|(b, f)| free > f || (free == f && n.name < b.name)) {
            best = Some((n, free));
        }
    }
    best.map(|(n, _)| n.name.clone()).ok_or_else(|| {
        if nodes.is_empty() {
            "no nodes registered".to_owned()
        } else {
            why.iter().map(|(r, c)| format!("{c} node(s) {r}")).collect::<Vec<_>>().join(", ")
        }
    })
}

#[derive(Debug, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub created: usize,
    pub moved: usize,
    pub removed: usize,
    pub pending: usize,
    /// Instances restarted in place because their Agent changed.
    pub restarted: usize,
}

fn suffix() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..6].to_owned()
}

/// One reconcile pass. Fails with StaleLease (and writes nothing) if `lead` is no longer current.
#[tracing::instrument(name = "scheduler.reconcile", skip(store, lead), fields(epoch = lead.epoch, holder = %lead.holder))]
pub async fn reconcile(store: &Store, lead: &Leadership) -> Result<Report, ApiError> {
    // desired state is read outside the txn; a racing edit is picked up next pass
    let (deployments, _) = store.list("AgentDeployment", None, &BTreeMap::new()).await?;
    let mut wanted = HashMap::new();
    for d in &deployments {
        let ns = &d.metadata.namespace;
        let spec = match admit_deployment(&d.spec) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let (agent, generation, agent_json) = match store.get(ns, "Agent", &spec.agent).await.and_then(|a| Ok((admit_agent(&a.spec)?, a.metadata.generation, a.spec))) {
            Ok(a) => a,
            Err(e) => {
                set_status(store, d, 0, spec.replicas as usize, &format!("agent {}: {}", spec.agent, e.message), None).await;
                continue;
            }
        };
        let req = Requirements {
            memory_mib: agent.resources.memory_mib,
            cpus: agent.resources.cpus,
            runtime: agent.runtime.kind(),
            selector: spec.node_selector.clone(),
            claims: agent.volumes.iter().map(|v| v.claim.clone()).collect(),
        };
        wanted.insert((ns.clone(), d.metadata.name.clone()), (spec, req, d.clone(), generation, agent_json));
    }

    let mut tx = store.pool().begin().await.map_err(clankr_store::db)?;
    Store::fence(&mut tx, lead).await?;
    // queue-driven sizing; the leader owns autoscale_state, so this is race-free
    let mut scaled: HashMap<(String, String), (u32, i64)> = HashMap::new();
    for ((ns, dep), (spec, ..)) in wanted.iter_mut() {
        let Some(a) = spec.autoscale.clone() else { continue };
        let depth: i64 = sqlx::query_scalar("select count(*) from tasks where namespace = $1 and queue = $2 and state in ('ready', 'leased')")
            .bind(ns)
            .bind(format!("agent/{}", spec.agent))
            .fetch_one(&mut *tx)
            .await
            .map_err(clankr_store::db)?;
        let row: Option<(i64, Option<chrono::DateTime<chrono::Utc>>)> = sqlx::query_as("select replicas, low_since from autoscale_state where namespace = $1 and deployment = $2")
            .bind(ns)
            .bind(dep)
            .fetch_optional(&mut *tx)
            .await
            .map_err(clankr_store::db)?;
        let (current, low_since) = row.map_or((a.min_replicas, None), |(r, l)| (r as u32, l));
        let (next, low) = autoscale_step(&a, depth, current, low_since, chrono::Utc::now());
        sqlx::query(
            "insert into autoscale_state (namespace, deployment, replicas, low_since) values ($1, $2, $3, $4)
             on conflict (namespace, deployment) do update set replicas = excluded.replicas, low_since = excluded.low_since, updated_at = now()",
        )
        .bind(ns)
        .bind(dep)
        .bind(next as i64)
        .bind(low)
        .execute(&mut *tx)
        .await
        .map_err(clankr_store::db)?;
        if next != current {
            tracing::info!(namespace = %ns, deployment = %dep, from = current, to = next, queue_depth = depth, "autoscaled");
        }
        spec.replicas = next;
        scaled.insert((ns.clone(), dep.clone()), (next, depth));
    }

    sqlx::query("update nodes set ready = false where ready and last_heartbeat < now() - make_interval(secs => $1)")
        .bind(NODE_TIMEOUT_SECS as f64)
        .execute(&mut *tx)
        .await
        .map_err(clankr_store::db)?;
    let nodes: Vec<NodeRow> = sqlx::query_as("select name, labels, capacity, ready, cordoned, draining, last_heartbeat from nodes order by name for update")
        .fetch_all(&mut *tx)
        .await
        .map_err(clankr_store::db)?;
    let instances: Vec<Instance> = sqlx::query_as("select id, namespace, deployment, agent, node, epoch, memory_mib, cpus, note, agent_generation, ready_epoch, ready_since from instances order by id for update")
        .fetch_all(&mut *tx)
        .await
        .map_err(clankr_store::db)?;
    let mut bindings: HashMap<(String, String), String> = sqlx::query_as::<_, (String, String, String)>("select namespace, claim, node from volume_bindings for update")
        .fetch_all(&mut *tx)
        .await
        .map_err(clankr_store::db)?
        .into_iter()
        .map(|(ns, c, n)| ((ns, c), n))
        .collect();

    let node_state: HashMap<&str, (bool, bool)> = nodes.iter().map(|n| (n.name.as_str(), (n.ready, n.draining))).collect();
    let mut cands: Vec<Candidate> = nodes
        .iter()
        .filter_map(|n| {
            let capacity: NodeCapacity = serde_json::from_value(n.capacity.0.clone()).ok()?;
            let labels = serde_json::from_value(n.labels.0.clone()).unwrap_or_default();
            Some(Candidate { name: n.name.clone(), ready: n.ready, cordoned: n.cordoned || n.draining, labels, capacity, used_memory_mib: 0, used_cpus: 0.0 })
        })
        .collect();
    let claims_of = |i: &Instance| wanted.get(&(i.namespace.clone(), i.deployment.clone())).map(|w| w.1.claims.clone()).unwrap_or_default();
    // the node an instance's volumes live on, if any of its claims is bound
    let pin_of = |ns: &str, claims: &[String], b: &HashMap<(String, String), String>| claims.iter().find_map(|c| b.get(&(ns.to_owned(), c.clone())).map(|n| (c.clone(), n.clone())));
    // an instance stays where it is on a ready node; draining moves it unless its data is local there
    let keep = |i: &Instance, b: &HashMap<(String, String), String>| {
        let Some(node) = i.node.as_deref() else { return false };
        match node_state.get(node) {
            Some((true, false)) => true,
            Some((true, true)) => pin_of(&i.namespace, &claims_of(i), b).is_some_and(|(_, n)| n == node),
            _ => false,
        }
    };
    let mut in_use: HashMap<(String, String), String> = HashMap::new();
    for i in instances.iter().filter(|i| keep(i, &bindings)) {
        if let Some(c) = cands.iter_mut().find(|c| Some(&c.name) == i.node.as_ref()) {
            c.used_memory_mib += i.memory_mib as u64;
            c.used_cpus += i.cpus;
        }
        for claim in claims_of(i) {
            in_use.insert((i.namespace.clone(), claim), i.id.clone());
        }
    }

    let mut report = Report::default();
    let mut by_dep: HashMap<(String, String), Vec<&Instance>> = HashMap::new();
    for i in &instances {
        by_dep.entry((i.namespace.clone(), i.deployment.clone())).or_default().push(i);
    }
    for (key, list) in &by_dep {
        if !wanted.contains_key(key) {
            for i in list {
                delete(&mut tx, &i.id).await?;
                report.removed += 1;
            }
        }
    }

    let mut statuses = vec![];
    for (key, (spec, req, res, latest, latest_spec)) in &wanted {
        let ns = &key.0;
        let mut list = by_dep.get(key).cloned().unwrap_or_default();
        list.sort_by_key(|i| (keep(i, &bindings), i.id.clone()));
        while list.len() > spec.replicas as usize {
            let i = list.remove(0);
            if keep(i, &bindings) {
                release(&mut cands, i);
                in_use.retain(|_, id| *id != i.id);
            }
            delete(&mut tx, &i.id).await?;
            report.removed += 1;
        }
        let mut reason = String::new();
        let mut placed = 0;
        // rolling restarts: at most maxUnavailable instances not-ready at a time
        let min_ready = chrono::Duration::seconds(spec.rollout.min_ready_seconds.into());
        // which generation this pass rolls toward, and how many more instances may move to it
        let plan = match &spec.rollout.canary {
            Some(c) => canary(&mut tx, ns, &key.1, c, spec.replicas, *latest, latest_spec, &list, min_ready).await?,
            None => Plan { generation: *latest, spec: Some(latest_spec.clone()), budget: usize::MAX, stable: None, note: None },
        };
        let generation = &plan.generation;
        let mut budget = plan.budget;
        let mut unavailable = list.iter().filter(|i| !i.available(min_ready)).count();
        let mut waiting_rollout = 0;
        for i in &list {
            if keep(i, &bindings) {
                placed += 1;
                let draining = i.node.as_deref().and_then(|n| node_state.get(n)).is_some_and(|s| s.1);
                let note = draining.then(|| {
                    let (c, n) = pin_of(ns, &req.claims, &bindings).unwrap_or_default();
                    format!("drain blocked: local volume {ns}/{c} lives on {n}; data doesn't move between nodes")
                });
                set_note(&mut tx, i, note.as_deref()).await?;
                if i.agent_generation != *generation {
                    let was_available = i.available(min_ready);
                    if budget == 0 || (was_available && unavailable >= spec.rollout.max_unavailable as usize) {
                        waiting_rollout += 1;
                        continue;
                    }
                    if was_available {
                        unavailable += 1;
                    }
                    budget -= 1;
                    // the Agent changed (new image, env, limits): restart in place on the new spec;
                    // the epoch bump fences the old process like any move
                    sqlx::query("update instances set epoch = epoch + 1, agent_generation = $2, memory_mib = $3, cpus = $4, agent_spec = $5, updated_at = now() where id = $1")
                        .bind(&i.id)
                        .bind(generation)
                        .bind(req.memory_mib as i64)
                        .bind(req.cpus)
                        .bind(&plan.spec)
                        .execute(&mut *tx)
                        .await
                        .map_err(clankr_store::db)?;
                    report.restarted += 1;
                }
                continue;
            }
            match try_place(req, ns, &i.id, &bindings, &in_use, &cands) {
                Ok(node) => {
                    assign(&mut cands, &node, req);
                    bind(&mut tx, &mut bindings, &mut in_use, ns, req, &node, &i.id).await?;
                    // ponytail: a move keeps the instance's generation and spec; the rollout upgrades it later.
                    // Resources come from the latest Agent, slightly off for an instance held back by a canary.
                    sqlx::query("update instances set node = $2, epoch = epoch + 1, note = null, memory_mib = $3, cpus = $4, updated_at = now() where id = $1")
                        .bind(&i.id)
                        .bind(&node)
                        .bind(req.memory_mib as i64)
                        .bind(req.cpus)
                        .execute(&mut *tx)
                        .await
                        .map_err(clankr_store::db)?;
                    report.moved += 1;
                    placed += 1;
                }
                Err(r) => {
                    sqlx::query("update instances set node = null, epoch = epoch + case when node is null then 0 else 1 end, note = $2, updated_at = now() where id = $1")
                        .bind(&i.id)
                        .bind(&r)
                        .execute(&mut *tx)
                        .await
                        .map_err(clankr_store::db)?;
                    reason = r;
                }
            }
        }
        for _ in list.len()..spec.replicas as usize {
            let id = format!("{}-{}", key.1, suffix());
            let node = match try_place(req, ns, &id, &bindings, &in_use, &cands) {
                Ok(n) => {
                    assign(&mut cands, &n, req);
                    bind(&mut tx, &mut bindings, &mut in_use, ns, req, &n, &id).await?;
                    placed += 1;
                    Some(n)
                }
                Err(r) => {
                    reason = r;
                    None
                }
            };
            // new instances join the target generation while the canary has room, else the stable one
            let (g, gspec) = if budget > 0 {
                budget = budget.saturating_sub(1);
                (*generation, plan.spec.clone())
            } else {
                (plan.fallback_generation(), plan.fallback_spec())
            };
            sqlx::query("insert into instances (id, namespace, deployment, agent, node, memory_mib, cpus, note, agent_generation, agent_spec) values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)")
                .bind(&id)
                .bind(ns)
                .bind(&key.1)
                .bind(&spec.agent)
                .bind(&node)
                .bind(req.memory_mib as i64)
                .bind(req.cpus)
                .bind(node.is_none().then_some(&reason))
                .bind(g)
                .bind(gspec)
                .execute(&mut *tx)
                .await
                .map_err(clankr_store::db)?;
            report.created += 1;
        }
        let pending = spec.replicas as usize - placed.min(spec.replicas as usize);
        report.pending += pending;
        let available = list.iter().filter(|i| i.available(min_ready) && i.agent_generation == *generation).count();
        // still rolling while any instance runs an older generation (or hasn't proven ready on this one)
        let updated = list.iter().filter(|i| i.agent_generation == *generation).count();
        let rollout = (updated < list.len() || waiting_rollout > 0).then(|| {
            format!("{updated}/{} on generation {generation}, {available} ready; {waiting_rollout} waiting (maxUnavailable {})", list.len(), spec.rollout.max_unavailable)
        });
        statuses.push((res.clone(), placed, pending, reason, Rollup { generation: *generation, available, rollout, autoscale: scaled.get(key).copied(), canary: plan.note }));
    }
    tx.commit().await.map_err(clankr_store::db)?;

    for (res, placed, pending, reason, rollup) in statuses {
        set_status(store, &res, placed, pending, &reason, Some(rollup)).await;
    }
    Ok(report)
}

/// Placement with volume rules: a ReadWriteOnce claim attached elsewhere blocks; a bound claim
/// pins the instance to its node (local data never moves).
fn try_place(
    req: &Requirements,
    ns: &str,
    instance: &str,
    bindings: &HashMap<(String, String), String>,
    in_use: &HashMap<(String, String), String>,
    cands: &[Candidate],
) -> Result<String, String> {
    if let Some((c, other)) = req.claims.iter().find_map(|c| in_use.get(&(ns.to_owned(), c.clone())).filter(|id| *id != instance).map(|id| (c, id))) {
        return Err(format!("ReadWriteOnce claim {ns}/{c} is attached to instance {other}"));
    }
    match req.claims.iter().find_map(|c| bindings.get(&(ns.to_owned(), c.clone())).map(|n| (c, n))) {
        Some((c, node)) => {
            let only: Vec<Candidate> = cands.iter().filter(|x| &x.name == node).cloned().collect();
            place(req, &only).map_err(|why| format!("volume {ns}/{c} is local to node {node} ({why}); data doesn't move between nodes"))
        }
        None => place(req, cands),
    }
}

/// Binds the instance's unbound claims to `node` and marks them attached to `instance`.
#[allow(clippy::too_many_arguments)]
async fn bind(
    tx: &mut clankr_store::Tx,
    bindings: &mut HashMap<(String, String), String>,
    in_use: &mut HashMap<(String, String), String>,
    ns: &str,
    req: &Requirements,
    node: &str,
    instance: &str,
) -> Result<(), ApiError> {
    for c in &req.claims {
        let key = (ns.to_owned(), c.clone());
        if !bindings.contains_key(&key) {
            sqlx::query("insert into volume_bindings (namespace, claim, node) values ($1, $2, $3)")
                .bind(ns)
                .bind(c)
                .bind(node)
                .execute(&mut **tx)
                .await
                .map_err(clankr_store::db)?;
            bindings.insert(key.clone(), node.to_owned());
        }
        in_use.insert(key, instance.to_owned());
    }
    Ok(())
}

async fn set_note(tx: &mut clankr_store::Tx, i: &Instance, note: Option<&str>) -> Result<(), ApiError> {
    if i.note.as_deref() == note {
        return Ok(());
    }
    sqlx::query("update instances set note = $2 where id = $1").bind(&i.id).bind(note).execute(&mut **tx).await.map_err(clankr_store::db).map(drop)
}

fn assign(cands: &mut [Candidate], node: &str, req: &Requirements) {
    if let Some(c) = cands.iter_mut().find(|c| c.name == node) {
        c.used_memory_mib += req.memory_mib;
        c.used_cpus += req.cpus;
    }
}

fn release(cands: &mut [Candidate], i: &Instance) {
    if let Some(c) = cands.iter_mut().find(|c| Some(&c.name) == i.node.as_ref()) {
        c.used_memory_mib = c.used_memory_mib.saturating_sub(i.memory_mib as u64);
        c.used_cpus -= i.cpus;
    }
}

async fn delete(tx: &mut clankr_store::Tx, id: &str) -> Result<(), ApiError> {
    sqlx::query("delete from instances where id = $1").bind(id).execute(&mut **tx).await.map_err(clankr_store::db).map(drop)
}

/// Best effort: status is informational, the instances table is the truth.
/// One autoscaling step: the replica count to run now and when demand first dipped below it.
/// Up: immediately to what the queue needs. Down: only once demand has stayed lower for the
/// delay, then straight to what it needs (no flapping on bursty queues).
pub fn autoscale_step(
    a: &clankr_types::agent::Autoscale,
    depth: i64,
    current: u32,
    low_since: Option<chrono::DateTime<chrono::Utc>>,
    now: chrono::DateTime<chrono::Utc>,
) -> (u32, Option<chrono::DateTime<chrono::Utc>>) {
    let need = (depth.max(0) as u64).div_ceil(a.target_tasks_per_replica as u64) as u32;
    let need = need.clamp(a.min_replicas, a.max_replicas);
    let current = current.clamp(a.min_replicas, a.max_replicas);
    if need >= current {
        return (need, None);
    }
    let since = low_since.unwrap_or(now);
    if (now - since).num_seconds() >= a.scale_down_delay_seconds as i64 { (need, None) } else { (current, Some(since)) }
}

/// Rollout progress for a deployment's status.
struct Rollup {
    generation: i64,
    /// Ready on the current Agent generation for at least minReadySeconds.
    available: usize,
    rollout: Option<String>,
    /// (replicas chosen, queue depth) when autoscaled.
    autoscale: Option<(u32, i64)>,
    canary: Option<Value>,
}

async fn set_status(store: &Store, res: &clankr_types::Resource, scheduled: usize, pending: usize, reason: &str, rollup: Option<Rollup>) {
    let mut status = serde_json::json!({ "scheduled": scheduled, "pending": pending, "phase": if pending == 0 { "Ready" } else { "Pending" } });
    if pending > 0 && !reason.is_empty() {
        status["reason"] = reason.into();
    }
    if let Some(r) = rollup {
        status["agentGeneration"] = r.generation.into();
        status["available"] = r.available.into();
        if let Some((replicas, depth)) = r.autoscale {
            status["autoscale"] = serde_json::json!({"replicas": replicas, "queueDepth": depth});
        }
        if let Some(c) = r.canary {
            status["canary"] = c;
        }
        if let Some(msg) = r.rollout {
            status["phase"] = "RollingOut".into();
            status["rollout"] = msg.into();
        }
    }
    if res.status.as_ref() == Some(&status) {
        return;
    }
    let ctx = clankr_types::RequestCtx { actor: "scheduler".into(), request_id: "reconcile".into() };
    if let Ok(cur) = store.get(&res.metadata.namespace, &res.kind, &res.metadata.name).await {
        let _ = store.update_status(&ctx, &cur.metadata.namespace, &cur.kind, &cur.metadata.name, cur.metadata.resource_version, status).await;
    }
}

/// What a reconcile pass rolls a deployment toward.
struct Plan {
    generation: i64,
    /// The Agent spec for `generation`; None only for instances from before specs were recorded.
    spec: Option<Value>,
    /// How many more instances may move to `generation` this pass.
    budget: usize,
    /// While held by a canary: the release everything else stays on.
    stable: Option<(i64, Option<Value>)>,
    /// `status.canary` of the deployment.
    note: Option<Value>,
}

impl Plan {
    fn fallback_generation(&self) -> i64 {
        self.stable.as_ref().map_or(self.generation, |s| s.0)
    }
    fn fallback_spec(&self) -> Option<Value> {
        self.stable.as_ref().map_or(self.spec.clone(), |s| s.1.clone())
    }
}

/// Canary analysis for one deployment (reads and advances its `rollouts` row).
#[allow(clippy::too_many_arguments)]
async fn canary(tx: &mut clankr_store::Tx, ns: &str, dep: &str, c: &Canary, replicas: u32, latest: i64, latest_spec: &Value, list: &[&Instance], min_ready: chrono::Duration) -> Result<Plan, ApiError> {
    let full = Plan { generation: latest, spec: Some(latest_spec.clone()), budget: usize::MAX, stable: None, note: None };
    type Row = (i64, Option<sqlx::types::Json<Value>>, String, Option<String>, chrono::DateTime<chrono::Utc>);
    let row: Option<Row> =
        sqlx::query_as("select stable_generation, stable_spec, phase, reason, started_at from rollouts where namespace = $1 and deployment = $2 and generation = $3")
            .bind(ns)
            .bind(dep)
            .bind(latest)
            .fetch_optional(&mut **tx)
            .await
            .map_err(clankr_store::db)?;
    let (stable_gen, stable_spec, phase, reason, started) = match row {
        Some((g, sp, p, r, t)) => (g, sp.map(|j| j.0), p, r, t),
        None => {
            // nothing older is running (a new deployment, or the canary already covers it all): no canary
            let mut olds: Vec<&&Instance> = list.iter().filter(|i| i.agent_generation != latest).collect();
            if olds.is_empty() {
                return Ok(full);
            }
            // the stable release is what most of the deployment runs now
            olds.sort_by_key(|i| std::cmp::Reverse(list.iter().filter(|j| j.agent_generation == i.agent_generation).count()));
            let g = olds[0].agent_generation;
            let sp: Option<sqlx::types::Json<Value>> = sqlx::query_scalar("select agent_spec from instances where namespace = $1 and deployment = $2 and agent_generation = $3 and agent_spec is not null limit 1")
                .bind(ns)
                .bind(dep)
                .bind(g)
                .fetch_optional(&mut **tx)
                .await
                .map_err(clankr_store::db)?
                .flatten();
            let sp = sp.map(|j| j.0);
            sqlx::query("insert into rollouts (namespace, deployment, generation, stable_generation, stable_spec, phase) values ($1, $2, $3, $4, $5, 'Canary')")
                .bind(ns)
                .bind(dep)
                .bind(latest)
                .bind(g)
                .bind(&sp)
                .execute(&mut **tx)
                .await
                .map_err(clankr_store::db)?;
            tracing::info!(namespace = %ns, deployment = %dep, generation = latest, stable = g, "canary started");
            (g, sp, "Canary".to_owned(), None, chrono::Utc::now())
        }
    };
    let note = |phase: &str, reason: Option<&str>| serde_json::json!({"generation": latest, "stableGeneration": stable_gen, "phase": phase, "reason": reason});
    match phase.as_str() {
        "Promoted" => return Ok(Plan { note: Some(note("Promoted", None)), ..full }),
        "Failed" => return Ok(rollback(latest, stable_gen, stable_spec, note("Failed", reason.as_deref()))),
        _ => {}
    }

    let canaries: Vec<&&Instance> = list.iter().filter(|i| i.agent_generation == latest).collect();
    let want = (c.replicas.min(replicas)) as usize;
    let now = chrono::Utc::now();
    let owners: Vec<String> = canaries.iter().map(|i| format!("agent/{}/{}", i.agent, i.id)).collect();
    let (failed, finished): (i64, i64) = sqlx::query_as(
        "select count(*) filter (where state = 'failed'), count(*) from tasks
         where namespace = $1 and lease_owner = any($2) and state in ('completed', 'failed') and finished_at >= $3",
    )
    .bind(ns)
    .bind(&owners)
    .bind(started)
    .fetch_one(&mut **tx)
    .await
    .map_err(clankr_store::db)?;
    let all_ready = canaries.len() >= want && canaries.iter().all(|i| i.available(min_ready));
    let verdict = if finished > 0 && finished >= c.min_tasks.max(1) as i64 && failed as f64 / finished as f64 > c.max_failure_ratio {
        Some(Err(format!("{failed} of {finished} canary tasks failed (max {}%)", (c.max_failure_ratio * 100.0).round())))
    } else if !all_ready && now - started > chrono::Duration::seconds(c.progress_deadline_seconds.into()) {
        Some(Err(format!("canary not ready within {}s", c.progress_deadline_seconds)))
    } else if all_ready
        && finished >= c.min_tasks as i64
        && canaries.iter().filter_map(|i| i.ready_since).max().is_some_and(|t| now - t >= min_ready + chrono::Duration::seconds(c.analysis_seconds.into()))
    {
        Some(Ok(()))
    } else {
        None
    };
    let held = Plan {
        generation: latest,
        spec: Some(latest_spec.clone()),
        budget: want.saturating_sub(canaries.len()),
        stable: Some((stable_gen, stable_spec.clone())),
        note: Some(note("Canary", Some(&format!("{}/{want} canaries ready; {finished} tasks finished, {failed} failed", canaries.iter().filter(|i| i.available(min_ready)).count())))),
    };
    let Some(v) = verdict else { return Ok(held) };
    let (phase, why) = match &v {
        Ok(()) => ("Promoted", None),
        Err(e) => ("Failed", Some(e.as_str())),
    };
    sqlx::query("update rollouts set phase = $4, reason = $5, updated_at = now() where namespace = $1 and deployment = $2 and generation = $3")
        .bind(ns)
        .bind(dep)
        .bind(latest)
        .bind(phase)
        .bind(why)
        .execute(&mut **tx)
        .await
        .map_err(clankr_store::db)?;
    Ok(match v {
        Ok(()) => {
            tracing::info!(namespace = %ns, deployment = %dep, generation = latest, "canary promoted");
            Plan { note: Some(note("Promoted", None)), ..full }
        }
        Err(e) => {
            tracing::warn!(namespace = %ns, deployment = %dep, generation = latest, reason = %e, "canary failed; rolling back");
            rollback(latest, stable_gen, stable_spec, note("Failed", Some(&e)))
        }
    })
}

/// Back to the stable release, or, when its spec predates recording (nodes would fetch the
/// latest Agent instead), freeze the deployment where it is.
fn rollback(latest: i64, stable_gen: i64, stable_spec: Option<Value>, mut note: Value) -> Plan {
    match stable_spec {
        Some(sp) => Plan { generation: stable_gen, spec: Some(sp), budget: usize::MAX, stable: None, note: Some(note) },
        None => {
            note["reason"] = format!("{}; can't roll back automatically (generation {stable_gen}'s spec wasn't recorded), so the rollout is frozen", note["reason"].as_str().unwrap_or_default()).into();
            Plan { generation: latest, spec: None, budget: 0, stable: None, note: Some(note) }
        }
    }
}

/// Runs in every clankrd; only the lease holder reconciles.
pub fn spawn(store: Store, holder: String, every: Duration) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(every);
        loop {
            tick.tick().await;
            let lead = match store.acquire_leadership(LEASE, &holder, (every.as_secs_f64() * 5.0).max(2.0)).await {
                Ok(Some(l)) => l,
                Ok(None) => continue,
                Err(e) => {
                    tracing::warn!(error = %e, "leader lease");
                    continue;
                }
            };
            match reconcile(&store, &lead).await {
                Ok(r) if r != Report::default() && (r.created + r.moved + r.removed + r.restarted) > 0 => tracing::info!(?r, epoch = lead.epoch, "scheduled"),
                Ok(_) => {}
                Err(e) if e.code == ErrorCode::StaleLease => tracing::warn!(error = %e, "lost leadership mid-pass"),
                Err(e) => tracing::warn!(error = %e, "reconcile failed"),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autoscale_up_fast_down_slow() {
        let a = clankr_types::agent::Autoscale { min_replicas: 0, max_replicas: 5, target_tasks_per_replica: 2, scale_down_delay_seconds: 60 };
        let t0 = chrono::Utc::now();
        let at = |s: i64| t0 + chrono::Duration::seconds(s);
        assert_eq!(autoscale_step(&a, 0, 0, None, t0), (0, None), "scale to zero when idle");
        assert_eq!(autoscale_step(&a, 3, 0, None, t0), (2, None), "ceil(3/2)");
        assert_eq!(autoscale_step(&a, 50, 2, None, t0), (5, None), "capped at max");
        assert_eq!(autoscale_step(&a, 1, 5, None, at(0)), (5, Some(at(0))), "demand dropped: start the clock");
        assert_eq!(autoscale_step(&a, 1, 5, Some(at(0)), at(30)), (5, Some(at(0))), "still within the delay");
        assert_eq!(autoscale_step(&a, 9, 5, Some(at(0)), at(40)), (5, None), "a burst resets it");
        assert_eq!(autoscale_step(&a, 1, 5, Some(at(0)), at(61)), (1, None), "down after the delay");
        let floor = clankr_types::agent::Autoscale { min_replicas: 2, ..a };
        assert_eq!(autoscale_step(&floor, 0, 0, None, t0), (2, None), "never below min");
    }

    fn node(name: &str, mem: u64, used: u64, runtimes: &[&str], labels: &[(&str, &str)]) -> Candidate {
        Candidate {
            name: name.into(),
            ready: true,
            cordoned: false,
            labels: labels.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            capacity: NodeCapacity { memory_mib: mem, cpus: 4.0, runtimes: runtimes.iter().map(|s| s.to_string()).collect() },
            used_memory_mib: used,
            used_cpus: 0.0,
        }
    }

    fn req(mem: u64, runtime: &'static str, sel: &[(&str, &str)]) -> Requirements {
        Requirements { memory_mib: mem, cpus: 1.0, runtime, selector: sel.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(), claims: vec![] }
    }

    #[test]
    fn filters_then_spreads_by_free_memory() {
        let nodes = vec![node("a", 4096, 3000, &["oci"], &[]), node("b", 4096, 1000, &["oci"], &[]), node("c", 8192, 0, &["process"], &[])];
        assert_eq!(place(&req(512, "oci", &[]), &nodes).unwrap(), "b", "most free memory among oci nodes");
        assert_eq!(place(&req(512, "process", &[]), &nodes).unwrap(), "c");
        let e = place(&req(4000, "oci", &[]), &nodes).unwrap_err();
        assert_eq!(e, "2 node(s) insufficient memory, 1 node(s) runtime not supported");
    }

    #[test]
    fn selector_readiness_and_ties() {
        let mut nodes = vec![node("b", 4096, 0, &["oci"], &[("gpu", "yes")]), node("a", 4096, 0, &["oci"], &[])];
        assert_eq!(place(&req(1, "oci", &[]), &nodes).unwrap(), "a", "tie breaks by name");
        assert_eq!(place(&req(1, "oci", &[("gpu", "yes")]), &nodes).unwrap(), "b");
        nodes[0].ready = false;
        assert_eq!(place(&req(1, "oci", &[("gpu", "yes")]), &nodes).unwrap_err(), "1 node(s) nodeSelector mismatch, 1 node(s) not ready");
        assert_eq!(place(&req(1, "oci", &[]), &[]).unwrap_err(), "no nodes registered");
        nodes[1].cordoned = true;
        assert_eq!(place(&req(1, "oci", &[]), &nodes).unwrap_err(), "1 node(s) cordoned, 1 node(s) not ready");
    }

    #[test]
    fn volumes_pin_and_are_exclusive() {
        let nodes = vec![node("a", 4096, 0, &["oci"], &[]), node("b", 8192, 0, &["oci"], &[])];
        let mut r = req(256, "oci", &[]);
        r.claims = vec!["ws".into()];
        let mut bindings = HashMap::new();
        let mut in_use = HashMap::new();
        // unbound: normal scoring picks b
        assert_eq!(try_place(&r, "ns", "i1", &bindings, &in_use, &nodes).unwrap(), "b");
        // bound to a: pinned there even though b has more room
        bindings.insert(("ns".to_owned(), "ws".to_owned()), "a".to_owned());
        assert_eq!(try_place(&r, "ns", "i1", &bindings, &in_use, &nodes).unwrap(), "a");
        // RWO: attached to i1, so i2 can't have it
        in_use.insert(("ns".to_owned(), "ws".to_owned()), "i1".to_owned());
        assert!(try_place(&r, "ns", "i2", &bindings, &in_use, &nodes).unwrap_err().starts_with("ReadWriteOnce claim ns/ws is attached to instance i1"));
        // pinned node gone: explicit blocker, no silent move to b
        let mut down = nodes.clone();
        down[0].ready = false;
        let e = try_place(&r, "ns", "i1", &bindings, &HashMap::new(), &down).unwrap_err();
        assert_eq!(e, "volume ns/ws is local to node a (1 node(s) not ready); data doesn't move between nodes");
    }
}
