//! Runs agent instances: launch, ABI handshake, lease → deliver → complete, heartbeats, and
//! brokering of the agent's model/context calls (PRD §15.3, §20 instance admission).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clankr_types::agent::{AgentSpec, Runtime, admit_agent};
use clankr_types::protocol::{ABI_MAJOR, ABI_MINOR, AgentFrame, CallOp, MAX_FRAME, NodeFrame};
use clankr_types::{ApiError, ErrorCode};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixListener;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{mpsc, watch};

use crate::client::{Client, Lease};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Where the instance socket dir is mounted inside OCI containers.
const CONTAINER_SOCKET_DIR: &str = "/run/clankr";
const LEASE_WAIT_SECS: u64 = 20;

type LeaseFuture = std::pin::Pin<Box<dyn std::future::Future<Output = Result<Option<Lease>, ApiError>> + Send>>;

pub struct Config {
    pub agent: String,
    pub namespace: String,
    pub work_dir: PathBuf,
    pub instance: String,
    /// The kennel binary used for OCI runtimes.
    pub kennel: PathBuf,
    /// kennel image store root (`--root`); None uses kennel's default.
    pub kennel_root: Option<PathBuf>,
    /// Set while the agent has completed its handshake (reported in fleet heartbeats).
    pub ready: Option<Arc<std::sync::atomic::AtomicBool>>,
}

pub async fn write_frame(w: &mut OwnedWriteHalf, f: &NodeFrame) -> Result<()> {
    let body = serde_json::to_vec(f)?;
    w.write_all(&(body.len() as u32).to_be_bytes()).await?;
    w.write_all(&body).await?;
    Ok(())
}

pub async fn read_frame(r: &mut OwnedReadHalf) -> Result<AgentFrame> {
    let len = r.read_u32().await? as usize;
    if len > MAX_FRAME {
        bail!("frame of {len} bytes exceeds {MAX_FRAME}");
    }
    let mut buf = vec![0; len];
    r.read_exact(&mut buf).await?;
    serde_json::from_slice(&buf).context("malformed frame")
}

/// Runs one agent instance until `shutdown` flips, restarting the agent process when it dies.
// ponytail: unbounded restarts with capped backoff; AgentSupervisor restart budgets (CL-20-04) come later
pub async fn run_instance(client: Client, cfg: Config, mut shutdown: watch::Receiver<bool>) -> Result<()> {
    let mut backoff = Duration::from_secs(1);
    while !*shutdown.borrow() {
        let spec = admit_agent(&client.agent(&cfg.agent).await?.spec)?;
        let outcome = run_once(&client, &cfg, &spec, &mut shutdown).await;
        if let Some(r) = &cfg.ready {
            r.store(false, std::sync::atomic::Ordering::Relaxed);
        }
        match outcome {
            Ok(()) => return Ok(()),
            Err(e) => {
                tracing::warn!(instance = %cfg.instance, error = %format!("{e:#}"), retry_in = ?backoff, "agent instance stopped");
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = shutdown.changed() => {}
                }
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
    }
    Ok(())
}

/// Socket paths are capped at ~108 bytes (sun_path), so sockets live under the short per-user
/// runtime dir (`$XDG_RUNTIME_DIR`, else the work dir), in a subdirectory keyed by the work dir
/// so separate nodes on one host never collide.
fn socket_dir(cfg: &Config) -> Result<PathBuf> {
    use sha2::Digest;
    let key = hex::encode(&sha2::Sha256::digest(cfg.work_dir.to_string_lossy().as_bytes())[..4]);
    let base = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(|| cfg.work_dir.clone());
    let dir = base.join(format!("clankr-node-{key}")).join(&cfg.instance);
    let len = dir.join("agent.sock").as_os_str().len();
    if len >= 104 {
        bail!("agent socket path {} is {len} bytes, over the unix socket limit; set XDG_RUNTIME_DIR or a shorter --work-dir", dir.display());
    }
    Ok(dir)
}

/// One agent process lifetime. Ok(()) only on requested shutdown.
async fn run_once(client: &Client, cfg: &Config, spec: &AgentSpec, shutdown: &mut watch::Receiver<bool>) -> Result<()> {
    // fresh socket per instance lifetime, private to this user (CL-32-06)
    let dir = socket_dir(cfg)?;
    use std::os::unix::fs::PermissionsExt;
    // the node dir stays private on the host; the instance dir is traverse-only and the socket
    // world-connectable so a non-root agent user (via subuid) can reach it inside the container
    std::fs::create_dir_all(&dir)?;
    std::fs::set_permissions(dir.parent().unwrap(), std::fs::Permissions::from_mode(0o700))?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o711))?;
    let sock = dir.join("agent.sock");
    let _ = std::fs::remove_file(&sock);
    // ImagePolicy, re-checked at every launch: the node is the last line before code runs
    let keys = clankr_types::agent::image_allowed(&client.image_policies().await?, &spec.runtime).map_err(|e| anyhow::anyhow!("refusing to launch: {e}"))?;
    let key_files: Vec<PathBuf> = keys
        .iter()
        .enumerate()
        .map(|(i, pem)| {
            let p = dir.join(format!("trusted-{i}.pub"));
            std::fs::write(&p, pem).map(|_| p)
        })
        .collect::<std::io::Result<_>>()?;
    let listener = UnixListener::bind(&sock)?;
    std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o666))?;

    // local-path volumes: <work-dir>/volumes/<ns>/<claim>. The parent stays private; the claim dir
    // is world-writable so a non-root agent user (subuid-mapped) can write inside the container.
    let mut volumes = vec![];
    for v in &spec.volumes {
        let root = cfg.work_dir.join("volumes");
        let host = root.join(&cfg.namespace).join(&v.claim);
        std::fs::create_dir_all(&host)?;
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))?;
        std::fs::set_permissions(&host, std::fs::Permissions::from_mode(0o777))?;
        volumes.push((host, v.mount_path.clone(), v.read_only));
    }

    let mut cmd = match &spec.runtime {
        // S0: plain host process, dev only
        Runtime::Process { command, env, working_dir } => {
            let mut c = tokio::process::Command::new(&command[0]);
            c.args(&command[1..]).envs(env).env("CLANKR_SOCKET", &sock);
            // dev runtime can't mount: hand the agent a mountPath -> host dir map instead
            if !volumes.is_empty() {
                let map: serde_json::Map<String, Value> = volumes.iter().map(|(h, m, _)| (m.clone(), Value::String(h.display().to_string()))).collect();
                c.env("CLANKR_VOLUMES", Value::Object(map).to_string());
            }
            if let Some(d) = working_dir {
                c.current_dir(d);
            }
            c
        }
        // S1: sandboxed container via kennel (ADR 0007). No network: every call the agent makes
        // goes through the socket. Only the socket dir is writable besides the container's /tmp.
        Runtime::Oci { image } => {
            let mut c = tokio::process::Command::new(&cfg.kennel);
            if let Some(root) = &cfg.kennel_root {
                c.arg("--root").arg(root);
            }
            let r = &spec.resources;
            c.args(["run", "--network", "none", "--name", &cfg.instance])
                .args(["--memory", &r.memory_mib.to_string(), "--cpus", &r.cpus.to_string(), "--pids", &r.pids.to_string(), "-v"])
                .arg(format!("{}:{CONTAINER_SOCKET_DIR}", dir.display()))
                .args(["-e", &format!("CLANKR_SOCKET={CONTAINER_SOCKET_DIR}/agent.sock")])
                .args(["-e", &format!("CLANKR_AGENT={}", cfg.agent)])
                .args(["-e", &format!("CLANKR_NAMESPACE={}", cfg.namespace)]);
            for (host, mount, ro) in &volumes {
                c.arg("-v").arg(format!("{}:{mount}{}", host.display(), if *ro { ":ro" } else { "" }));
            }
            for k in &key_files {
                c.arg("--verify-key").arg(k);
            }
            c.arg(image);
            c
        }
    };
    cmd.env("CLANKR_AGENT", &cfg.agent)
        .env("CLANKR_NAMESPACE", &cfg.namespace)
        .env_remove("CLANKR_TOKEN") // the node's credential must not leak into the agent
        .kill_on_drop(true); // SIGKILL to kennel takes its container down too (kennel-init pdeathsig)
    let command = format!("{:?}", cmd.as_std());
    let mut child = cmd.spawn().with_context(|| format!("spawn {command}"))?;

    let (stream, _) = tokio::select! {
        r = tokio::time::timeout(HANDSHAKE_TIMEOUT, listener.accept()) => r.context("agent did not connect to CLANKR_SOCKET")??,
        s = child.wait() => bail!("agent exited before connecting: {}", s?),
    };
    let (mut rd, mut wr) = stream.into_split();

    // handshake (CL-25-02): incompatible major ABI is rejected before any work is dispatched
    let hello = tokio::time::timeout(HANDSHAKE_TIMEOUT, read_frame(&mut rd)).await.context("no Hello")??;
    let AgentFrame::Hello { abi_major, abi_minor, sdk } = hello else { bail!("expected Hello, got {hello:?}") };
    if abi_major != ABI_MAJOR {
        let reason = format!("agent ABI {abi_major}.{abi_minor} is incompatible with node ABI {ABI_MAJOR}.{ABI_MINOR}");
        write_frame(&mut wr, &NodeFrame::Reject { reason: reason.clone() }).await.ok();
        // let the agent read and report the reason before it's killed
        let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
        bail!(reason);
    }
    write_frame(
        &mut wr,
        &NodeFrame::Welcome {
            abi_major: ABI_MAJOR,
            abi_minor: ABI_MINOR,
            namespace: cfg.namespace.clone(),
            agent: cfg.agent.clone(),
            instance: cfg.instance.clone(),
            models: spec.models.clone(),
            contexts: spec.contexts.clone(),
        },
    )
    .await?;
    let ready = tokio::time::timeout(HANDSHAKE_TIMEOUT, read_frame(&mut rd)).await.context("no Ready")??;
    if ready != AgentFrame::Ready {
        bail!("expected Ready, got {ready:?}");
    }
    tracing::info!(instance = %cfg.instance, sdk, "agent ready");
    if let Some(r) = &cfg.ready {
        r.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    // frames arrive through a channel so the select loop below stays cancel-safe
    let (ftx, mut frames) = mpsc::channel::<Result<AgentFrame>>(16);
    tokio::spawn(async move {
        loop {
            let f = read_frame(&mut rd).await;
            let stop = f.is_err();
            if ftx.send(f).await.is_err() || stop {
                break;
            }
        }
    });

    let who = format!("agent/{}/{}", cfg.agent, cfg.instance);
    let queue = format!("agent/{}", cfg.agent);
    let ttl = spec.task_ttl_seconds;
    let mut busy: Option<(u64, Lease)> = None;
    let mut next_delivery = 0u64;
    let mut lease_fut: Option<LeaseFuture> = None;
    let mut heartbeat = tokio::time::interval(Duration::from_secs((ttl / 3).max(1)));
    let mut draining = false;

    loop {
        if busy.is_none() && !draining && lease_fut.is_none() {
            let (c, q, owner) = (client.clone(), queue.clone(), who.clone());
            lease_fut = Some(Box::pin(async move { c.lease(&q, &owner, ttl, LEASE_WAIT_SECS).await }));
        }
        if draining && busy.is_none() {
            write_frame(&mut wr, &NodeFrame::Shutdown { grace_ms: 5000 }).await.ok();
            let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
            return Ok(()); // child is killed on drop if still running
        }
        tokio::select! {
            r = async { lease_fut.as_mut().unwrap().await }, if lease_fut.is_some() => {
                lease_fut = None;
                match r {
                    Ok(Some(l)) => {
                        next_delivery += 1;
                        write_frame(&mut wr, &NodeFrame::Deliver {
                            delivery_id: next_delivery,
                            run_id: l.run_id.clone(),
                            task: l.task.clone(),
                            node: l.node.clone(),
                            attempt: l.attempt,
                            idempotency_key: l.idempotency_key.clone(),
                            input: l.input.clone(),
                        }).await?;
                        heartbeat.reset();
                        busy = Some((next_delivery, l));
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::warn!(error = %e, "lease failed");
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                }
            }
            f = frames.recv() => {
                let f = match f {
                    Some(Ok(f)) => f,
                    // agent went away mid-task: don't guess the outcome; the lease lapses and the
                    // engine redelivers (idempotent) or marks it OutcomeUnknown (unsafe)
                    Some(Err(e)) => bail!("agent connection lost: {e:#}"),
                    None => bail!("agent connection closed"),
                };
                match f {
                    AgentFrame::Complete { delivery_id, result } => match take_if(&mut busy, delivery_id) {
                        Some(l) => {
                            if let Err(e) = client.complete(&l, result).await {
                                tracing::warn!(task = %l.task, error = %e, "completion rejected");
                            }
                        }
                        None => tracing::warn!(delivery_id, "agent answered a delivery it doesn't hold; ignored"),
                    },
                    AgentFrame::Fail { delivery_id, error, retryable } => match take_if(&mut busy, delivery_id) {
                        Some(l) => {
                            if let Err(e) = client.fail(&l, &error, retryable).await {
                                tracing::warn!(task = %l.task, error = %e, "failure report rejected");
                            }
                        }
                        None => tracing::warn!(delivery_id, "agent answered a delivery it doesn't hold; ignored"),
                    },
                    AgentFrame::Call { call_id, op } => {
                        let reply = match broker(client, spec, &who, busy.as_ref().map(|b| &b.1), op).await {
                            Ok(v) => NodeFrame::Reply { call_id, ok: Some(v), error: None },
                            Err(e) => NodeFrame::Reply { call_id, ok: None, error: Some(e) },
                        };
                        write_frame(&mut wr, &reply).await?;
                    }
                    AgentFrame::Log { level, message } => tracing::info!(target: "agent", instance = %cfg.instance, level, "{message}"),
                    other => tracing::warn!(?other, "unexpected frame after handshake"),
                }
            }
            _ = heartbeat.tick(), if busy.is_some() => {
                if let Some((_, l)) = &busy
                    && let Err(e) = client.heartbeat(l, ttl).await
                {
                    // fenced: someone else owns the task now; drop it so we don't report a stale result
                    tracing::warn!(task = %l.task, error = %e, "heartbeat rejected; abandoning task");
                    if e.code == ErrorCode::StaleLease {
                        busy = None;
                    }
                }
            }
            s = child.wait() => bail!("agent process exited: {}", s?),
            _ = shutdown.changed() => {
                draining = true;
                lease_fut = None;
            }
        }
    }
}

fn take_if(busy: &mut Option<(u64, Lease)>, delivery_id: u64) -> Option<Lease> {
    if busy.as_ref().is_some_and(|b| b.0 == delivery_id) { busy.take().map(|b| b.1) } else { None }
}

/// Authorizes and forwards one agent call. The claim must be declared in the Agent spec;
/// the SDK's view of what's allowed is never trusted (CL-25-08, RTE-18).
async fn broker(client: &Client, spec: &AgentSpec, who: &str, task: Option<&Lease>, op: CallOp) -> Result<Value, ApiError> {
    let deny = |kind: &str, claim: &str| ApiError::new(ErrorCode::Forbidden, format!("agent is not bound to {kind} {claim:?}"));
    match op {
        CallOp::Generate { claim, mut request } => {
            if !spec.models.contains(&claim) {
                return Err(deny("ModelClaim", &claim));
            }
            // attribute usage and apply the per-run budget to the task being worked on
            if let (Some(l), Some(obj)) = (task, request.as_object_mut()) {
                obj.insert("runId".into(), l.run_id.clone().into());
                obj.insert("task".into(), l.task.clone().into());
            }
            client.generate(&claim, &request, who).await
        }
        CallOp::ContextGet { claim, key } => {
            if !spec.contexts.contains(&claim) {
                return Err(deny("ContextClaim", &claim));
            }
            match client.context_get(&claim, &key, who).await {
                Err(e) if e.code == ErrorCode::NotFound => Ok(Value::Null),
                r => r,
            }
        }
        CallOp::ContextPut { claim, key, value, if_version } => {
            if !spec.contexts.contains(&claim) {
                return Err(deny("ContextClaim", &claim));
            }
            client.context_put(&claim, &key, &value, if_version, who).await
        }
        CallOp::MailboxSend { mailbox, payload, correlation_id, dedup_key, ttl_seconds } => {
            if !spec.mailboxes.contains(&mailbox) {
                return Err(deny("AgentMailbox", &mailbox));
            }
            let body = json!({"payload": payload, "correlationId": correlation_id, "dedupKey": dedup_key, "ttlSeconds": ttl_seconds});
            client.mailbox(&mailbox, "messages", &body, who).await
        }
        CallOp::MailboxReceive { mailbox, max, visibility_seconds, wait_seconds } => {
            if !spec.mailboxes.contains(&mailbox) {
                return Err(deny("AgentMailbox", &mailbox));
            }
            let body = json!({"owner": who, "max": max, "visibilitySeconds": visibility_seconds, "waitSeconds": wait_seconds});
            client.mailbox(&mailbox, "receive", &body, who).await
        }
        CallOp::MailboxAck { mailbox, id, epoch } => {
            if !spec.mailboxes.contains(&mailbox) {
                return Err(deny("AgentMailbox", &mailbox));
            }
            client.mailbox(&mailbox, "ack", &json!({"id": id, "epoch": epoch}), who).await
        }
        CallOp::MailboxNack { mailbox, id, epoch, delay_seconds, reason } => {
            if !spec.mailboxes.contains(&mailbox) {
                return Err(deny("AgentMailbox", &mailbox));
            }
            client.mailbox(&mailbox, "nack", &json!({"id": id, "epoch": epoch, "delaySeconds": delay_seconds, "reason": reason}), who).await
        }
        CallOp::ContextList { claim, prefix } => {
            if !spec.contexts.contains(&claim) {
                return Err(deny("ContextClaim", &claim));
            }
            client.context_list(&claim, &prefix, who).await
        }
    }
}
