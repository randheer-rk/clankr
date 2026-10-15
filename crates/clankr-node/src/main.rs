//! `clankr-node`: runs instances of one Agent on this machine.

use std::path::PathBuf;

use anyhow::Context;
use clankr_node::client::Client;
use clankr_node::daemon::{Config, run_instance};
use clap::Parser;

#[derive(Parser)]
#[command(version, about = "Clankr node daemon: runs agent instances and brokers their calls")]
struct Args {
    #[arg(long, env = "CLANKR_SERVER", default_value = "http://127.0.0.1:7600")]
    server: String,
    #[arg(short, long, default_value = "default")]
    namespace: String,
    /// Agent resource to run (single-agent mode).
    #[arg(short, long, required_unless_present = "join_token")]
    agent: Option<String>,
    /// Fleet mode: join the cluster and run whatever the scheduler assigns.
    #[arg(long, env = "CLANKR_JOIN_TOKEN")]
    join_token: Option<String>,
    /// Fleet mode: the cluster CA certificate (clankrd's <data-dir>/pki/ca.crt).
    #[arg(long, env = "CLANKR_CA")]
    ca_cert: Option<PathBuf>,
    /// Fleet mode node name (default: hostname).
    #[arg(long)]
    name: Option<String>,
    /// Fleet mode capacity offered to the scheduler.
    #[arg(long, default_value_t = 4096)]
    memory_mib: u64,
    #[arg(long, default_value_t = 2.0)]
    cpus: f64,
    #[arg(long, value_delimiter = ',', default_value = "oci")]
    runtimes: Vec<String>,
    /// Node labels for nodeSelector, e.g. --label gpu=yes
    #[arg(long = "label")]
    labels: Vec<String>,
    #[arg(long, default_value_t = 1)]
    instances: usize,
    #[arg(long, env = "CLANKR_NODE_DIR")]
    work_dir: Option<PathBuf>,
    /// kennel binary for `runtime: oci` agents.
    #[arg(long, env = "KENNEL", default_value = "kennel")]
    kennel: PathBuf,
    /// kennel image store root.
    #[arg(long, env = "KENNEL_ROOT")]
    kennel_root: Option<PathBuf>,
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").expect("HOME set"))
}

fn token() -> anyhow::Result<String> {
    if let Ok(t) = std::env::var("CLANKR_TOKEN") {
        return Ok(t);
    }
    let dir = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| home().join(".config"));
    let p = dir.join("clankr/token");
    Ok(std::fs::read_to_string(&p).with_context(|| format!("read {}", p.display()))?.trim().to_owned())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into())).init();
    let a = Args::parse();
    let work_dir = a.work_dir.clone().unwrap_or_else(|| home().join(".local/share/clankr/node"));
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);

    if let Some(join_token) = a.join_token.clone() {
        let name = a.name.clone().unwrap_or_else(|| std::fs::read_to_string("/etc/hostname").map(|h| h.trim().to_lowercase()).unwrap_or_else(|_| "node".into()));
        let labels: serde_json::Map<String, serde_json::Value> =
            a.labels.iter().filter_map(|l| l.split_once('=').map(|(k, v)| (k.to_owned(), v.into()))).collect();
        let ca_path = a.ca_cert.clone().ok_or_else(|| anyhow::anyhow!("fleet mode needs --ca-cert (clankrd's <data-dir>/pki/ca.crt)"))?;
        let cfg = clankr_node::fleet::FleetConfig {
            ca_pem: std::fs::read_to_string(&ca_path).with_context(|| format!("read {}", ca_path.display()))?,
            server: a.server.clone(),
            name,
            join_token,
            capacity: serde_json::json!({"memoryMiB": a.memory_mib, "cpus": a.cpus, "runtimes": a.runtimes}),
            labels: labels.into(),
            work_dir,
            kennel: a.kennel.clone(),
            kennel_root: a.kennel_root.clone(),
            heartbeat: std::time::Duration::from_secs(2),
            fence_after: std::time::Duration::from_secs(10),
            renew_before: std::time::Duration::from_secs(10 * 86_400),
        };
        let fleet = tokio::spawn(clankr_node::fleet::run_fleet(cfg, stop_rx));
        wait_for_signal().await?;
        tracing::info!("draining");
        stop_tx.send(true)?;
        return fleet.await?;
    }
    let agent = a.agent.clone().expect("clap requires --agent without --join-token");
    let client = Client::new(&a.server, &token()?, &a.namespace);

    let mut tasks = vec![];
    for _ in 0..a.instances {
        let cfg = Config {
            agent: agent.clone(),
            namespace: a.namespace.clone(),
            work_dir: work_dir.clone(),
            instance: format!("{}-{}", agent, &uuid::Uuid::new_v4().to_string()[..8]),
            kennel: a.kennel.clone(),
            kennel_root: a.kennel_root.clone(),
        };
        tasks.push(tokio::spawn(run_instance(client.clone(), cfg, stop_rx.clone())));
    }
    wait_for_signal().await?;
    tracing::info!("draining");
    stop_tx.send(true)?;
    for t in tasks {
        if let Err(e) = t.await? {
            tracing::warn!(error = %format!("{e:#}"), "instance ended with error");
        }
    }
    Ok(())
}

async fn wait_for_signal() -> anyhow::Result<()> {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
    Ok(())
}
