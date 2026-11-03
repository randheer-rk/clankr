//! Fleet mode: join the cluster, heartbeat, and run exactly the instances the scheduler assigns
//! to this node (PRD §23). An instance whose assignment disappears or changes epoch is stopped.
//! If the control plane can't be reached for longer than the node timeout, every instance is
//! stopped: the scheduler has moved them elsewhere by then (self-fencing).

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::Value;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use std::sync::{Arc, RwLock};

use crate::client::{Assignment, Client, NodeTls};
use crate::daemon::{Config, run_instance};

pub struct FleetConfig {
    /// One control plane URL, or several replicas comma-separated (failover).
    pub server: String,
    /// PEM of the cluster CA (clankrd's `<data-dir>/pki/ca.crt`), obtained out of band.
    pub ca_pem: String,
    pub name: String,
    pub join_token: String,
    pub capacity: Value,
    pub labels: Value,
    pub work_dir: PathBuf,
    pub kennel: PathBuf,
    pub kennel_root: Option<PathBuf>,
    pub heartbeat: Duration,
    /// Stop everything after this long without a successful heartbeat.
    pub fence_after: Duration,
    /// Rotate the node certificate when less than this much validity remains.
    pub renew_before: Duration,
}

struct Running {
    epoch: i64,
    ready: Arc<std::sync::atomic::AtomicBool>,
    identity: Arc<RwLock<String>>,
    stop: watch::Sender<bool>,
    hibernate: watch::Sender<bool>,
    task: JoinHandle<Result<()>>,
}

pub async fn run_fleet(cfg: FleetConfig, mut shutdown: watch::Receiver<bool>) -> Result<()> {
    let ca = reqwest::Certificate::from_pem(cfg.ca_pem.as_bytes())?;
    let servers = crate::client::Endpoints::parse(&cfg.server);
    let (identity, mut cert) = Client::join(&servers, &ca, &cfg.name, &cfg.join_token, &cfg.capacity, &cfg.labels).await?;
    let mut tls = NodeTls { ca, identity: Some(identity) };
    // instances share this client; rotation swaps it under them
    let http: crate::client::SharedHttp = Arc::new(RwLock::new(tls.client(Duration::from_secs(330))));
    tracing::info!(node = %cfg.name, "joined cluster (mTLS)");
    let mut running: HashMap<String, Running> = HashMap::new();
    let mut exec_started: std::collections::HashSet<String> = Default::default();
    let mut last_ok = Instant::now();
    let mut tick = tokio::time::interval(cfg.heartbeat);
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = shutdown.changed() => break,
        }
        let expires = clankr_tls::not_after_pem(&cert).unwrap_or(0);
        if expires - (chrono::Utc::now().timestamp()) < cfg.renew_before.as_secs() as i64 {
            match Client::renew(&servers, &tls, &cfg.name).await {
                Ok((id, pem)) => {
                    tls.identity = Some(id);
                    cert = pem;
                    *http.write().expect("lock") = tls.client(Duration::from_secs(330));
                    tracing::info!(node = %cfg.name, "node certificate rotated");
                }
                Err(e) => tracing::warn!(error = %e, "certificate renewal failed; will retry"),
            }
        }
        // which instances are up, for rolling restarts; only counts for the epoch reported
        let report: Vec<serde_json::Value> = running
            .iter()
            .map(|(id, r)| serde_json::json!({"id": id, "epoch": r.epoch, "ready": !r.task.is_finished() && r.ready.load(std::sync::atomic::Ordering::Relaxed)}))
            .collect();
        let (want, execs): (Vec<Assignment>, Vec<crate::exec::ExecRequest>) = match Client::node_heartbeat(&servers, &tls, &cfg.name, &serde_json::json!({"instances": report})).await {
            Ok(a) => {
                last_ok = Instant::now();
                a
            }
            Err(e) => {
                tracing::warn!(error = %e, "heartbeat failed");
                if last_ok.elapsed() > cfg.fence_after && !running.is_empty() {
                    tracing::warn!("control plane unreachable past the node timeout; stopping all instances");
                    stop_all(&mut running).await;
                }
                continue;
            }
        };
        // clank exec sessions into our instances, each started once
        for x in execs {
            if !running.contains_key(&x.instance) || !exec_started.insert(x.session.clone()) {
                continue;
            }
            let (base, tls, node, kennel, root) = (servers.current(), tls.clone(), cfg.name.clone(), cfg.kennel.clone(), cfg.kennel_root.clone());
            tokio::spawn(async move {
                let session = x.session.clone();
                if let Err(e) = crate::exec::serve(base, tls, node, kennel, root, x).await {
                    tracing::warn!(%session, error = %format!("{e:#}"), "exec session failed");
                }
            });
        }
        // stop what's no longer ours (or was reassigned and came back with a new epoch)
        let ids: Vec<String> = running.keys().cloned().collect();
        for id in ids {
            let keep = want.iter().any(|a| a.id == id && a.epoch == running[&id].epoch);
            if !keep {
                let r = running.remove(&id).unwrap();
                let _ = r.stop.send(true);
                tracing::info!(instance = %id, "instance unassigned; stopping");
            }
        }
        for a in want {
            if let Some(r) = running.get(&a.id).filter(|r| !r.task.is_finished()) {
                *r.identity.write().expect("lock") = a.identity.clone();
                if a.hibernate {
                    r.hibernate.send_if_modified(|h| !std::mem::replace(h, true));
                }
                continue;
            }
            if a.hibernate {
                continue; // checkpointed (or never started): stays down until the deployment resumes
            }
            let (stop, rx) = watch::channel(false);
            let (hibernate, hib_rx) = watch::channel(false);
            let ready = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let identity = Arc::new(RwLock::new(a.identity.clone()));
            let client = Client::fleet(&servers, http.clone(), &a.namespace, identity.clone());
            let icfg = Config {
                agent: a.agent.clone(),
                namespace: a.namespace.clone(),
                work_dir: cfg.work_dir.clone(),
                instance: a.id.clone(),
                kennel: cfg.kennel.clone(),
                kennel_root: cfg.kennel_root.clone(),
                ready: Some(ready.clone()),
                spec: a.agent_spec.clone(),
                hibernate: Some(hib_rx),
                checkpoint: a.checkpoint_version.map(|v| (v, a.checkpoint.clone().unwrap_or_default())),
            };
            tracing::info!(instance = %a.id, agent = %a.agent, epoch = a.epoch, "starting instance");
            running.insert(a.id.clone(), Running { epoch: a.epoch, ready, identity, stop, hibernate, task: tokio::spawn(run_instance(client, icfg, rx)) });
        }
    }
    stop_all(&mut running).await;
    Ok(())
}

async fn stop_all(running: &mut HashMap<String, Running>) {
    for (_, r) in running.drain() {
        let _ = r.stop.send(true);
        let _ = tokio::time::timeout(Duration::from_secs(10), r.task).await;
    }
}
