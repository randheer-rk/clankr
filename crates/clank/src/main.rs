//! `clank` — Clankr CLI.

use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clankr_types::{ApiError, Resource, kind_for, plural};
use clap::{Parser, Subcommand, ValueEnum};
use reqwest::StatusCode;
use reqwest::blocking::{Client, RequestBuilder, Response};
use serde_json::Value;

mod eval;
mod oidc;

#[derive(Parser)]
#[command(version, about = "Clankr command-line client")]
struct Cli {
    #[arg(long, env = "CLANKR_SERVER", default_value = "https://127.0.0.1:7600", global = true)]
    server: String,
    /// Cluster CA to trust for https (default: clankrd's <data-dir>/pki/ca.crt).
    #[arg(long, env = "CLANKR_CA", global = true)]
    ca: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create or update resources from YAML/JSON manifests (multi-document; `-` for stdin).
    Apply {
        #[arg(short, long, required = true)]
        file: Vec<PathBuf>,
        /// Validate server-side without persisting.
        #[arg(long)]
        dry_run: bool,
    },
    /// List resources of a kind, or show one.
    Get {
        kind: String,
        name: Option<String>,
        #[arg(short, long, default_value = "default")]
        namespace: String,
        /// List across all namespaces.
        #[arg(short = 'A', long)]
        all_namespaces: bool,
        /// Label selector, e.g. team=a,tier=worker
        #[arg(short = 'l', long)]
        selector: Option<String>,
        #[arg(short, long, value_enum, default_value_t = Output::Table)]
        output: Output,
        /// Stream changes after the initial list.
        #[arg(short, long)]
        watch: bool,
    },
    Delete {
        kind: String,
        name: String,
        #[arg(short, long, default_value = "default")]
        namespace: String,
    },
    /// Start and operate workflow runs.
    Run {
        #[arg(short, long, default_value = "default", global = true)]
        namespace: String,
        #[command(subcommand)]
        cmd: RunCmd,
    },
    /// Cluster nodes, readiness and the instances placed on them.
    Nodes,
    /// Node maintenance: cordon (no new placements), uncordon, drain (cordon + move instances off).
    Node {
        #[arg(value_parser = ["cordon", "uncordon", "drain"])]
        action: String,
        name: String,
    },
    /// Inspect and operate AgentMailboxes.
    Mailbox {
        #[arg(short, long, default_value = "default", global = true)]
        namespace: String,
        #[command(subcommand)]
        cmd: MailboxCmd,
    },
    /// Encrypted secrets. Values are write-only: nothing reads them back except the gateway.
    Secret {
        #[arg(short, long, default_value = "default", global = true)]
        namespace: String,
        #[command(subcommand)]
        cmd: SecretCmd,
    },
    /// Who the server thinks you are.
    Whoami,
    /// Promote image digests through an AgentImage's stages, roll back, show history.
    Image {
        #[arg(short, long, default_value = "default", global = true)]
        namespace: String,
        #[command(subcommand)]
        cmd: ImageCmd,
    },
    /// Evaluate a workflow against a dataset and gate on thresholds / a baseline (exit 1 on failure).
    Eval {
        #[command(subcommand)]
        cmd: EvalCmd,
    },
    /// The approval inbox: what's waiting on a human decision.
    Approvals {
        #[arg(short, long, default_value = "default", global = true)]
        namespace: String,
        #[command(subcommand)]
        cmd: ApprovalCmd,
    },
    /// Sign in with your identity provider (OIDC device flow). Commands then use your ID token,
    /// refreshed automatically; CLANKR_TOKEN still overrides.
    Login {
        #[arg(long, env = "CLANKR_OIDC_ISSUER")]
        issuer: String,
        #[arg(long, env = "CLANKR_OIDC_CLIENT_ID")]
        client_id: String,
        #[arg(long, default_value = "openid email groups offline_access")]
        scope: String,
    },
    /// Forget the saved login.
    Logout,
    /// Quotas, budgets and model spend for a namespace.
    Usage {
        #[arg(short, long, default_value = "default")]
        namespace: String,
        #[arg(short, long, value_enum, default_value_t = Output::Table)]
        output: Output,
    },
    /// ServiceAccount API tokens (the account itself is a resource: `clank apply`).
    Token {
        #[arg(short, long, default_value = "default", global = true)]
        namespace: String,
        #[command(subcommand)]
        cmd: TokenCmd,
    },
    /// Dev worker (trusted tier S0, no sandbox): leases tasks from a queue and runs a shell
    /// command per task. Task data is JSON on stdin; stdout (JSON, else a string) is the result.
    /// Exit 0 completes; exit 65 fails permanently; any other exit fails retryably.
    Worker {
        #[arg(short, long, default_value = "default")]
        namespace: String,
        /// e.g. agent/researcher or tool/publisher
        #[arg(short, long)]
        queue: String,
        #[arg(long)]
        exec: String,
        #[arg(long, default_value_t = default_owner())]
        owner: String,
        #[arg(long, default_value_t = 60)]
        ttl_seconds: u64,
        /// Stop after this many tasks (0 = run forever).
        #[arg(long, default_value_t = 0)]
        max_tasks: u64,
    },
}

#[derive(Subcommand)]
enum MailboxCmd {
    /// Messages, optionally by state (ready, leased, acked, dead).
    Ls {
        mailbox: String,
        #[arg(long)]
        state: Option<String>,
    },
    Send {
        mailbox: String,
        /// JSON payload, or @file.json
        payload: String,
        #[arg(long)]
        correlation_id: Option<String>,
        #[arg(long)]
        dedup_key: Option<String>,
    },
    /// Move a dead-lettered message back to ready.
    Requeue { mailbox: String, id: String },
}

#[derive(Subcommand)]
enum ImageCmd {
    /// Move a digest into a stage; Agents tracking the stage switch to it.
    Promote {
        image: String,
        stage: String,
        /// sha256:… (from `kennel build` / `kennel push`)
        digest: String,
        #[arg(long, default_value = "")]
        reason: String,
        /// Attach a `clank eval run --report` file as evidence (for gated stages).
        #[arg(long)]
        eval_report: Option<PathBuf>,
    },
    /// Put a stage back on its previous digest.
    Rollback {
        image: String,
        stage: String,
        #[arg(long, default_value = "")]
        reason: String,
    },
    /// Promotion history, newest first.
    History {
        image: String,
        #[arg(long)]
        stage: Option<String>,
    },
}

#[derive(Subcommand)]
enum EvalCmd {
    /// Run every case in a suite file and score it.
    Run {
        suite: PathBuf,
        /// Write the JSON report here (commit it as the next baseline).
        #[arg(long)]
        report: Option<PathBuf>,
        /// Fail on regressions against this earlier report.
        #[arg(long)]
        baseline: Option<PathBuf>,
        /// Token used for autoApprove decisions; must be a different principal from the one
        /// starting the runs (nobody approves their own run).
        #[arg(long, env = "CLANKR_EVAL_APPROVER_TOKEN", hide_env_values = true)]
        approver_token: Option<String>,
    },
    /// Print a run's recorded model calls as a replay script (ProviderConnection `script:`).
    Trace {
        run: String,
        #[arg(short, long, default_value = "default")]
        namespace: String,
    },
}

#[derive(Subcommand)]
enum ApprovalCmd {
    /// Pending approvals you can decide (--all: everything pending; --state for history).
    Ls {
        #[arg(long)]
        all: bool,
        #[arg(long, default_value = "pending")]
        state: String,
    },
    /// Show one request, including the run data it gates.
    Show { id: String },
    Approve {
        id: String,
        #[arg(long, default_value = "")]
        comment: String,
    },
    Reject {
        id: String,
        #[arg(long, default_value = "")]
        comment: String,
    },
}

#[derive(Subcommand)]
enum TokenCmd {
    /// Issue a token; it is printed once and can't be shown again.
    Create {
        service_account: String,
        #[arg(long, default_value = "")]
        description: String,
        /// Lifetime in seconds (default: no expiry).
        #[arg(long)]
        ttl_seconds: Option<i64>,
    },
    List { service_account: String },
    Revoke { service_account: String, id: String },
}

#[derive(Subcommand)]
enum SecretCmd {
    /// `clank secret set llm apiKey=-` reads the value from stdin; `apiKey=env:OPENAI_API_KEY`
    /// takes it from your environment. Literal values work too but land in shell history.
    Set { name: String, #[arg(required = true)] pairs: Vec<String> },
    List,
    Get { name: String },
    Delete { name: String },
}

#[derive(Subcommand)]
enum RunCmd {
    Start {
        workflow: String,
        /// JSON input, or @file.json
        #[arg(long, default_value = "null")]
        input: String,
        #[arg(long)]
        idempotency_key: Option<String>,
    },
    Get { id: String },
    List {
        #[arg(long)]
        workflow: Option<String>,
    },
    History { id: String },
    Signal {
        id: String,
        signal: String,
        #[arg(long, default_value = "null")]
        payload: String,
    },
    /// Decide the pending approval at `node` of run `id` (shortcut for `clank approvals`).
    Approve {
        id: String,
        node: String,
        #[arg(long)]
        reject: bool,
        #[arg(long, default_value = "")]
        comment: String,
    },
    Cancel {
        id: String,
        #[arg(long, default_value = "")]
        reason: String,
    },
    /// Settle a task whose outcome is unknown (unsafe side effect, worker lost).
    Resolve {
        id: String,
        task: String,
        #[arg(long, value_parser = ["completed", "retry", "failed"])]
        outcome: String,
        /// Result JSON when --outcome completed
        #[arg(long, default_value = "null")]
        result: String,
        #[arg(long)]
        error: Option<String>,
    },
}

fn default_owner() -> String {
    format!("{}-{}", std::env::var("HOSTNAME").unwrap_or_else(|_| "local".into()), std::process::id())
}

#[derive(Clone, Copy, ValueEnum, PartialEq)]
enum Output {
    Table,
    Yaml,
    Json,
}

#[derive(Clone)]
struct Api {
    http: Client,
    base: String,
    token: String,
}

impl Api {
    fn req(&self, m: reqwest::Method, path: &str) -> RequestBuilder {
        self.http.request(m, format!("{}{path}", self.base)).bearer_auth(&self.token)
    }
}

fn main() {
    if let Err(e) = run(Cli::parse()) {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    match &cli.cmd {
        Cmd::Login { issuer, client_id, scope } => {
            oidc::login(&oidc::session_path(), issuer, client_id, scope)?;
            eprintln!("Logged in. Check with: clank whoami");
            return Ok(());
        }
        Cmd::Logout => {
            match std::fs::remove_file(oidc::session_path()) {
                Ok(()) => eprintln!("Logged out."),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => eprintln!("Not logged in."),
                Err(e) => return Err(e.into()),
            }
            return Ok(());
        }
        _ => {}
    }
    // no overall timeout: watches stream indefinitely
    let mut builder = Client::builder().timeout(None);
    if cli.server.starts_with("https://") {
        let path = cli.ca.clone().unwrap_or_else(|| {
            let data = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").expect("HOME set")).join(".local/share"));
            data.join("clankr/pki/ca.crt")
        });
        let pem = std::fs::read(&path).with_context(|| format!("read cluster CA {} (pass --ca or set CLANKR_CA)", path.display()))?;
        // trust exactly the cluster CA
        builder = builder.tls_certs_only([reqwest::Certificate::from_pem(&pem)?]);
    }
    let api = Api {
        http: builder.build()?,
        base: cli.server.trim_end_matches('/').to_owned(),
        token: token()?,
    };
    match cli.cmd {
        Cmd::Apply { file, dry_run } => file.iter().try_for_each(|f| apply(&api, f, dry_run)),
        Cmd::Get { kind, name, namespace, all_namespaces, selector, output, watch } => {
            let p = resolve(&kind)?;
            match name {
                Some(n) => {
                    let r: Resource = ok(api.req(reqwest::Method::GET, &format!("/v1/namespaces/{namespace}/{p}/{n}")).send()?)?.json()?;
                    print_items(&[r], output, false)
                }
                None => {
                    let base = if all_namespaces { format!("/v1/{p}") } else { format!("/v1/namespaces/{namespace}/{p}") };
                    let mut q = vec![];
                    if let Some(s) = &selector {
                        q.push(("labelSelector", s.as_str()));
                    }
                    if watch {
                        q.push(("watch", "true"));
                        return watch_print(ok(api.req(reqwest::Method::GET, &base).query(&q).send()?)?, output, all_namespaces);
                    }
                    let list: Value = ok(api.req(reqwest::Method::GET, &base).query(&q).send()?)?.json()?;
                    let items: Vec<Resource> = serde_json::from_value(list["items"].clone())?;
                    print_items(&items, output, all_namespaces)
                }
            }
        }
        Cmd::Run { namespace, cmd } => run_cmd(&api, &namespace, cmd),
        Cmd::Node { action, name } => {
            ok(api.req(reqwest::Method::POST, &format!("/v1/nodes/{name}/{action}")).send()?)?;
            println!("node/{name} {action}ed");
            if action == "drain" {
                println!("instances move off within a scheduler pass; `clank nodes` shows anything blocked (e.g. local volumes)");
            }
            Ok(())
        }
        Cmd::Nodes => {
            let v: Value = ok(api.req(reqwest::Method::GET, "/v1/nodes").send()?)?.json()?;
            let inst = v["instances"].as_array().cloned().unwrap_or_default();
            let rows: Vec<Vec<String>> = v["nodes"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|n| {
                    let name = n["name"].as_str().unwrap_or("?").to_owned();
                    let c = &n["capacity"];
                    let here: Vec<String> = inst.iter().filter(|i| i["node"] == name.as_str()).filter_map(|i| i["id"].as_str().map(str::to_owned)).collect();
                    let mut status = if n["ready"] == true { "Ready".to_owned() } else { "NotReady".to_owned() };
                    if n["draining"] == true {
                        status += ",Draining";
                    } else if n["cordoned"] == true {
                        status += ",Cordoned";
                    }
                    vec![
                        name,
                        status,
                        format!("{}Mi", c["memoryMiB"]),
                        c["cpus"].to_string(),
                        c["runtimes"].as_array().map(|r| r.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(",")).unwrap_or_default(),
                        if here.is_empty() { "-".into() } else { here.join(",") },
                    ]
                })
                .collect();
            if rows.is_empty() {
                eprintln!("No nodes joined.");
            } else {
                print_table(&["NAME", "STATUS", "MEMORY", "CPUS", "RUNTIMES", "INSTANCES"].map(String::from), &rows);
            }
            for i in &inst {
                if let Some(note) = i["note"].as_str() {
                    let at = i["node"].as_str().unwrap_or("unplaced");
                    println!("{} ({at}): {note}", i["id"].as_str().unwrap_or("?"));
                }
            }
            Ok(())
        }
        Cmd::Secret { namespace, cmd } => secret_cmd(&api, &namespace, cmd),
        Cmd::Whoami => {
            let v: Value = ok(api.req(reqwest::Method::GET, "/v1/whoami").send()?)?.json()?;
            println!("{}", serde_json::to_string_pretty(&v)?);
            Ok(())
        }
        Cmd::Token { namespace, cmd } => token_cmd(&api, &namespace, cmd),
        Cmd::Approvals { namespace, cmd } => approvals_cmd(&api, &namespace, cmd),
        Cmd::Image { namespace, cmd } => image_cmd(&api, &namespace, cmd),
        Cmd::Eval { cmd: EvalCmd::Run { suite, report, baseline, approver_token } } => {
            let approver = approver_token.map(|token| Api { token, ..api.clone() });
            eval::run(&api, approver.as_ref().unwrap_or(&api), &suite, report.as_deref(), baseline.as_deref())
        }
        Cmd::Eval { cmd: EvalCmd::Trace { run, namespace } } => {
            let t: Vec<Value> = ok(api.req(reqwest::Method::GET, &format!("/v1/namespaces/{namespace}/runs/{run}/traces")).send()?)?.json()?;
            anyhow::ensure!(!t.is_empty(), "run {run} has no recorded model calls (record: true on its ProviderConnection?)");
            let script: Vec<Value> = t
                .iter()
                .map(|c| serde_json::json!({"promptSha256": c["promptSha256"], "reply": c["response"], "inputTokens": c["inputTokens"], "outputTokens": c["outputTokens"], "stopReason": c["stopReason"]}))
                .collect();
            print!("{}", serde_yaml_ng::to_string(&serde_json::json!({"script": script}))?);
            Ok(())
        }
        Cmd::Login { .. } | Cmd::Logout => unreachable!("handled before connecting"),
        Cmd::Usage { namespace, output } => usage(&api, &namespace, output),
        Cmd::Mailbox { namespace, cmd } => mailbox_cmd(&api, &namespace, cmd),
        Cmd::Worker { namespace, queue, exec, owner, ttl_seconds, max_tasks } => worker(&api, &namespace, &queue, &exec, &owner, ttl_seconds, max_tasks),
        Cmd::Delete { kind, name, namespace } => {
            let p = resolve(&kind)?;
            let r: Resource = ok(api.req(reqwest::Method::DELETE, &format!("/v1/namespaces/{namespace}/{p}/{name}")).send()?)?.json()?;
            let what = if r.metadata.deletion_timestamp.is_some() && !r.metadata.finalizers.is_empty() { "terminating" } else { "deleted" };
            println!("{}/{name} {what}", r.kind.to_lowercase());
            Ok(())
        }
    }
}

/// CLANKR_TOKEN, else your `clank login` session, else the local owner token file.
fn token() -> Result<String> {
    if let Ok(t) = std::env::var("CLANKR_TOKEN") {
        return Ok(t);
    }
    if let Some(t) = oidc::bearer(&oidc::session_path())? {
        return Ok(t);
    }
    let dir = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").expect("HOME set")).join(".config"));
    let path = dir.join("clankr/token");
    Ok(std::fs::read_to_string(&path).with_context(|| format!("read {} (start clankrd once, or set CLANKR_TOKEN)", path.display()))?.trim().to_owned())
}

fn resolve(kind: &str) -> Result<String> {
    Ok(plural(kind_for(kind).with_context(|| format!("unknown kind {kind:?}"))?))
}

/// Passes 2xx through; turns the server's ApiError body into a readable error.
fn ok(r: Response) -> Result<Response> {
    if r.status().is_success() {
        return Ok(r);
    }
    let status = r.status();
    let body = r.text()?;
    match serde_json::from_str::<ApiError>(&body) {
        Ok(e) => bail!("{e}"),
        Err(_) => bail!("HTTP {status}: {body}"),
    }
}

fn apply(api: &Api, file: &PathBuf, dry_run: bool) -> Result<()> {
    let mut text = String::new();
    if file.as_os_str() == "-" {
        std::io::stdin().read_to_string(&mut text)?;
    } else {
        text = std::fs::read_to_string(file).with_context(|| format!("read {}", file.display()))?;
    }
    for doc in serde_yaml_ng::Deserializer::from_str(&text) {
        let v: Value = serde::Deserialize::deserialize(doc).with_context(|| format!("parse {}", file.display()))?;
        if v.is_null() {
            continue; // empty document between `---`
        }
        let mut r: Resource = serde_json::from_value(v).with_context(|| format!("{}: not a Clankr resource", file.display()))?;
        let p = resolve(&r.kind)?;
        let (ns, name) = (r.metadata.namespace.clone(), r.metadata.name.clone());
        let item = format!("/v1/namespaces/{ns}/{p}/{name}");
        let dry = if dry_run { "?dryRun=true" } else { "" };
        let label = format!("{}/{name}", r.kind.to_lowercase());

        let cur = api.req(reqwest::Method::GET, &item).send()?;
        let verb = if cur.status() == StatusCode::NOT_FOUND {
            ok(api.req(reqwest::Method::POST, &format!("/v1/namespaces/{ns}/{p}{dry}")).json(&r).send()?)?;
            "created"
        } else {
            let cur: Resource = ok(cur)?.json()?;
            let m = (&r.metadata, &cur.metadata);
            if r.spec == cur.spec && m.0.labels == m.1.labels && m.0.annotations == m.1.annotations && m.0.finalizers == m.1.finalizers {
                "unchanged"
            } else {
                r.metadata.resource_version = cur.metadata.resource_version;
                ok(api.req(reqwest::Method::PUT, &format!("{item}{dry}")).json(&r).send()?)?;
                "configured"
            }
        };
        println!("{label} {verb}{}", if dry_run { " (dry run)" } else { "" });
    }
    Ok(())
}

fn print_items(items: &[Resource], output: Output, show_ns: bool) -> Result<()> {
    match output {
        Output::Json if items.len() == 1 => println!("{}", serde_json::to_string_pretty(&items[0])?),
        Output::Json => println!("{}", serde_json::to_string_pretty(items)?),
        Output::Yaml => {
            for (i, r) in items.iter().enumerate() {
                if i > 0 {
                    println!("---");
                }
                print!("{}", serde_yaml_ng::to_string(r)?);
            }
        }
        Output::Table => {
            if items.is_empty() {
                eprintln!("No resources found.");
                return Ok(());
            }
            let rows: Vec<_> = items.iter().map(|r| row(r, show_ns)).collect();
            print_table(&header(show_ns), &rows);
        }
    }
    Ok(())
}

fn header(show_ns: bool) -> Vec<String> {
    let mut h = vec!["NAME", "GEN", "PHASE", "AGE"];
    if show_ns {
        h.insert(0, "NAMESPACE");
    }
    h.into_iter().map(String::from).collect()
}

fn row(r: &Resource, show_ns: bool) -> Vec<String> {
    let phase = r.status.as_ref().and_then(|s| s["phase"].as_str()).unwrap_or("-");
    let phase = if r.metadata.deletion_timestamp.is_some() { "Terminating" } else { phase };
    let mut row = vec![r.metadata.name.clone(), r.metadata.generation.to_string(), phase.to_owned(), age(r)];
    if show_ns {
        row.insert(0, r.metadata.namespace.clone());
    }
    row
}

fn age(r: &Resource) -> String {
    let Some(t) = r.metadata.creation_timestamp else { return "-".into() };
    let s = (chrono::Utc::now() - t).num_seconds().max(0);
    if s < 120 {
        format!("{s}s")
    } else if s < 7200 {
        format!("{}m", s / 60)
    } else if s < 172_800 {
        format!("{}h", s / 3600)
    } else {
        format!("{}d", s / 86400)
    }
}

fn print_table(header: &[String], rows: &[Vec<String>]) {
    let widths: Vec<usize> = (0..header.len()).map(|i| rows.iter().map(|r| r[i].len()).chain([header[i].len()]).max().unwrap()).collect();
    for r in std::iter::once(header).chain(rows.iter().map(Vec::as_slice)) {
        let line: Vec<String> = r.iter().zip(&widths).map(|(c, w)| format!("{c:<w$}")).collect();
        println!("{}", line.join("   ").trim_end());
    }
}

/// Reads the SSE watch stream and prints one line (or document) per event.
fn watch_print(resp: Response, output: Output, show_ns: bool) -> Result<()> {
    let (mut event, mut data) = (String::new(), String::new());
    if output == Output::Table {
        let mut h = header(show_ns);
        h.insert(0, "EVENT".into());
        println!("{}", h.join("   "));
    }
    for line in BufReader::new(resp).lines() {
        let line = line?;
        if let Some(e) = line.strip_prefix("event:") {
            event = e.trim().to_owned();
        } else if let Some(d) = line.strip_prefix("data:") {
            data.push_str(d.trim());
        } else if line.is_empty() {
            match event.as_str() {
                "error" => bail!("{}", serde_json::from_str::<ApiError>(&data)?),
                _ if !data.is_empty() => {
                    let ev: clankr_types::WatchEvent = serde_json::from_str(&data)?;
                    match output {
                        Output::Table => {
                            let mut r = row(&ev.object, show_ns);
                            r.insert(0, format!("{:?}", ev.kind));
                            println!("{}", r.join("   "));
                        }
                        Output::Json => println!("{data}"),
                        Output::Yaml => print!("---\n{}", serde_yaml_ng::to_string(&ev)?),
                    }
                }
                _ => {} // keepalive / bookmark
            }
            event.clear();
            data.clear();
        }
    }
    Ok(())
}

fn json_arg(s: &str) -> Result<Value> {
    let text = match s.strip_prefix('@') {
        Some(path) => std::fs::read_to_string(path).with_context(|| format!("read {path}"))?,
        None => s.to_owned(),
    };
    serde_json::from_str(&text).with_context(|| format!("not valid JSON: {text}"))
}

fn run_summary(r: &Value) {
    let s = &r["state"];
    let pending = |k: &str| s[k].as_object().map(|o| o.keys().cloned().collect::<Vec<_>>().join(",")).unwrap_or_default();
    println!("run       {}", r["runId"].as_str().unwrap_or("?"));
    println!("workflow  {} (gen {})", r["workflow"].as_str().unwrap_or("?"), r["workflowGeneration"]);
    println!("status    {}", r["status"].as_str().unwrap_or("?"));
    println!("events    {}", r["seq"]);
    for (label, key) in [("tasks", "tasks"), ("timers", "timers"), ("waiting", "waits"), ("unknown", "unknown")] {
        let p = pending(key);
        if !p.is_empty() {
            println!("{label:<9} {p}");
        }
    }
    if let Some(e) = s["error"].as_str() {
        println!("error     {e}");
    }
}

fn run_cmd(api: &Api, ns: &str, cmd: RunCmd) -> Result<()> {
    use reqwest::Method as M;
    let runs = format!("/v1/namespaces/{ns}/runs");
    match cmd {
        RunCmd::Start { workflow, input, idempotency_key } => {
            let mut body = serde_json::json!({ "input": json_arg(&input)? });
            if let Some(k) = idempotency_key {
                body["idempotencyKey"] = k.into();
            }
            let r: Value = ok(api.req(M::POST, &format!("/v1/namespaces/{ns}/workflows/{workflow}/runs")).json(&body).send()?)?.json()?;
            run_summary(&r);
        }
        RunCmd::Get { id } => run_summary(&ok(api.req(M::GET, &format!("{runs}/{id}")).send()?)?.json()?),
        RunCmd::List { workflow } => {
            let path = match workflow {
                Some(w) => format!("/v1/namespaces/{ns}/workflows/{w}/runs"),
                None => runs,
            };
            let list: Vec<Value> = ok(api.req(M::GET, &path).send()?)?.json()?;
            let rows: Vec<Vec<String>> = list
                .iter()
                .map(|r| {
                    let s = |k: &str| r[k].as_str().unwrap_or("-").to_owned();
                    vec![s("runId"), s("workflow"), s("status"), r["seq"].to_string(), s("createdAt")]
                })
                .collect();
            if rows.is_empty() {
                eprintln!("No runs found.");
            } else {
                print_table(&["RUN", "WORKFLOW", "STATUS", "EVENTS", "CREATED"].map(String::from), &rows);
            }
        }
        RunCmd::History { id } => {
            let h: Vec<Value> = ok(api.req(M::GET, &format!("{runs}/{id}/history")).send()?)?.json()?;
            for e in h {
                let mut ev = e["event"].clone();
                let ty = ev.as_object_mut().and_then(|o| o.remove("type")).unwrap_or_default();
                println!("{:>4}  {}  {:<15} {}", e["seq"], e["at"].as_str().unwrap_or(""), ty.as_str().unwrap_or("?"), ev);
            }
        }
        RunCmd::Signal { id, signal, payload } => {
            let body = serde_json::json!({ "signal": signal, "payload": json_arg(&payload)? });
            run_summary(&ok(api.req(M::POST, &format!("{runs}/{id}/signal")).json(&body).send()?)?.json()?);
        }
        RunCmd::Approve { id, node, reject, comment } => {
            let pending: Vec<Value> = ok(api.req(M::GET, &format!("/v1/namespaces/{ns}/approvals?state=pending")).send()?)?.json()?;
            let a = pending.iter().find(|a| a["runId"] == id.as_str() && a["node"] == node.as_str()).with_context(|| format!("no pending approval at {node} in run {id}"))?;
            decide_approval(api, ns, a["id"].as_str().unwrap_or_default(), !reject, &comment)?;
            run_summary(&ok(api.req(M::GET, &format!("{runs}/{id}")).send()?)?.json()?);
        }
        RunCmd::Resolve { id, task, outcome, result, error } => {
            let body = serde_json::json!({ "signal": format!("resolve.{task}"), "payload": { "outcome": outcome, "result": json_arg(&result)?, "error": error } });
            run_summary(&ok(api.req(M::POST, &format!("{runs}/{id}/signal")).json(&body).send()?)?.json()?);
        }
        RunCmd::Cancel { id, reason } => {
            run_summary(&ok(api.req(M::POST, &format!("{runs}/{id}/cancel")).json(&serde_json::json!({ "reason": reason })).send()?)?.json()?);
        }
    }
    Ok(())
}

/// EX_DATAERR from sysexits.h: the task's input is bad, retrying won't help.
const EXIT_PERMANENT: i32 = 65;

// ponytail: one task at a time; run several workers for concurrency.
fn worker(api: &Api, ns: &str, queue: &str, exec: &str, owner: &str, ttl: u64, max_tasks: u64) -> Result<()> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let tasks = format!("/v1/namespaces/{ns}/tasks");
    eprintln!("worker {owner}: leasing from {ns}/{queue}");
    let mut done = 0;
    while max_tasks == 0 || done < max_tasks {
        let body = serde_json::json!({ "queue": queue, "owner": owner, "ttlSeconds": ttl, "waitSeconds": 30 });
        let resp = ok(api.req(reqwest::Method::POST, &format!("{tasks}/lease")).json(&body).send()?)?;
        if resp.status() == StatusCode::NO_CONTENT {
            continue;
        }
        let t: Value = resp.json()?;
        let (run_id, task, epoch) = (t["runId"].clone(), t["task"].as_str().unwrap_or_default().to_owned(), t["epoch"].clone());
        eprintln!("  {task} (run {}, epoch {epoch})", run_id.as_str().unwrap_or("?"));

        // heartbeat at a third of the TTL while the command runs; stop if fenced
        let running = Arc::new(AtomicBool::new(true));
        let hb = {
            let (api, running, url) = (api.clone(), running.clone(), format!("{tasks}/heartbeat"));
            let body = serde_json::json!({ "runId": run_id, "task": task, "epoch": epoch, "ttlSeconds": ttl });
            std::thread::spawn(move || {
                let tick = std::time::Duration::from_millis(200);
                let mut since = std::time::Duration::ZERO;
                while running.load(Ordering::Relaxed) {
                    std::thread::sleep(tick);
                    since += tick;
                    if since.as_secs() >= (ttl / 3).max(1) {
                        since = std::time::Duration::ZERO;
                        if let Err(e) = api.req(reqwest::Method::POST, &url).json(&body).send().map_err(anyhow::Error::from).and_then(ok) {
                            eprintln!("  heartbeat failed: {e:#}");
                        }
                    }
                }
            })
        };

        let mut child = Command::new("sh")
            .arg("-c")
            .arg(exec)
            .env("CLANKR_NAMESPACE", ns)
            .env("CLANKR_RUN_ID", run_id.as_str().unwrap_or_default())
            .env("CLANKR_TASK", &task)
            .env("CLANKR_NODE", t["node"].as_str().unwrap_or_default())
            .env("CLANKR_ATTEMPT", t["attempt"].to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .with_context(|| format!("spawn {exec:?}"))?;
        child.stdin.take().unwrap().write_all(t["input"].to_string().as_bytes())?;
        let out = child.wait_with_output()?;
        running.store(false, Ordering::Relaxed);
        hb.join().ok();

        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        let (path, body) = match out.status.code() {
            Some(0) => {
                let result = serde_json::from_str(&stdout).unwrap_or(Value::String(stdout));
                ("complete", serde_json::json!({ "runId": run_id, "task": task, "epoch": epoch, "result": result }))
            }
            code => {
                let error = format!("command exited with {}: {}", code.map_or("signal".into(), |c| c.to_string()), stdout);
                let retryable = code != Some(EXIT_PERMANENT);
                ("fail", serde_json::json!({ "runId": run_id, "task": task, "epoch": epoch, "error": error, "retryable": retryable }))
            }
        };
        match api.req(reqwest::Method::POST, &format!("{tasks}/{path}")).json(&body).send().map_err(anyhow::Error::from).and_then(ok) {
            Ok(_) => eprintln!("  {task} -> {path}"),
            // fenced: another worker owns this task now; our result is discarded by design
            Err(e) => eprintln!("  {task} result rejected: {e:#}"),
        }
        done += 1;
    }
    Ok(())
}

fn image_cmd(api: &Api, ns: &str, cmd: ImageCmd) -> Result<()> {
    use reqwest::Method as M;
    let base = |image: &str| format!("/v1/namespaces/{ns}/agentimages/{image}");
    let short = |d: &Value| d.as_str().map(|d| d.chars().take(19).collect::<String>()).unwrap_or_else(|| "-".into());
    let done = |r: &Value| {
        let agents = r["agentsUpdated"].as_array().map(|a| a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(", ")).unwrap_or_default();
        println!("{} {} → {}{}", r["stage"].as_str().unwrap_or("?"), short(&r["previousDigest"]), short(&r["digest"]), if agents.is_empty() { String::new() } else { format!(" · rolling out to {agents}") });
    };
    match cmd {
        ImageCmd::Promote { image, stage, digest, reason, eval_report } => {
            let evidence = match eval_report {
                Some(p) => {
                    let bytes = std::fs::read(&p).with_context(|| format!("read {}", p.display()))?;
                    let r: Value = serde_json::from_slice(&bytes)?;
                    use sha2::Digest;
                    Some(serde_json::json!({"eval": {"suite": r["suite"], "passRate": r["passRate"], "avgCostUSD": r["avgCostUsd"], "at": r["at"], "reportSha256": hex::encode(sha2::Sha256::digest(&bytes))}}))
                }
                None => None,
            };
            let body = serde_json::json!({"stage": stage, "digest": digest, "reason": reason, "evidence": evidence});
            done(&ok(api.req(M::POST, &format!("{}/promotions", base(&image))).json(&body).send()?)?.json()?);
        }
        ImageCmd::Rollback { image, stage, reason } => {
            done(&ok(api.req(M::POST, &format!("{}/rollback", base(&image))).json(&serde_json::json!({"stage": stage, "reason": reason})).send()?)?.json()?);
        }
        ImageCmd::History { image, stage } => {
            let q = stage.map(|s| format!("?stage={s}")).unwrap_or_default();
            let h: Vec<Value> = ok(api.req(M::GET, &format!("{}/promotions{q}", base(&image))).send()?)?.json()?;
            let s = |v: &Value| v.as_str().unwrap_or("-").to_owned();
            let rows: Vec<Vec<String>> = h
                .iter()
                .map(|p| {
                    let ev = p["evidence"]["eval"]["passRate"].as_f64().map(|r| format!("eval {:.0}%", r * 100.0)).unwrap_or_default();
                    vec![s(&p["at"]).chars().take(19).collect(), s(&p["stage"]), short(&p["digest"]), if p["rollback"] == true { "rollback".into() } else { "promote".into() }, s(&p["promotedBy"]), ev, s(&p["reason"])]
                })
                .collect();
            print_table(&["AT", "STAGE", "DIGEST", "ACTION", "BY", "EVIDENCE", "REASON"].map(String::from), &rows);
        }
    }
    Ok(())
}

fn approvals_cmd(api: &Api, ns: &str, cmd: ApprovalCmd) -> Result<()> {
    use reqwest::Method as M;
    let base = format!("/v1/namespaces/{ns}/approvals");
    match cmd {
        ApprovalCmd::Ls { all, state } => {
            let mine = if all { "" } else { "&mine=true" };
            let l: Vec<Value> = ok(api.req(M::GET, &format!("{base}?state={state}{mine}")).send()?)?.json()?;
            let s = |v: &Value| v.as_str().unwrap_or("-").to_owned();
            let rows: Vec<Vec<String>> = l
                .iter()
                .map(|a| vec![s(&a["id"]), s(&a["runId"]), s(&a["node"]), s(&a["approverGroup"]), s(&a["requestedBy"]), s(&a["state"]), s(&a["createdAt"]), s(&a["deadline"])])
                .collect();
            if rows.is_empty() {
                eprintln!("No {state} approvals{}.", if all { "" } else { " for you" });
            } else {
                print_table(&["ID", "RUN", "NODE", "GROUP", "REQUESTED BY", "STATE", "CREATED", "DEADLINE"].map(String::from), &rows);
            }
        }
        ApprovalCmd::Show { id } => {
            let a: Value = ok(api.req(M::GET, &format!("{base}/{id}")).send()?)?.json()?;
            println!("{}", serde_yaml_ng::to_string(&a)?);
        }
        ApprovalCmd::Approve { id, comment } => decide_approval(api, ns, &id, true, &comment)?,
        ApprovalCmd::Reject { id, comment } => decide_approval(api, ns, &id, false, &comment)?,
    }
    Ok(())
}

/// Fetches the request and decides it, bound to the digest of what was fetched.
fn decide_approval(api: &Api, ns: &str, id: &str, approve: bool, comment: &str) -> Result<()> {
    use reqwest::Method as M;
    let base = format!("/v1/namespaces/{ns}/approvals/{id}");
    let a: Value = ok(api.req(M::GET, &base).send()?)?.json()?;
    let action = if approve { "approve" } else { "reject" };
    let body = serde_json::json!({ "digest": a["digest"], "comment": comment });
    let d: Value = ok(api.req(M::POST, &format!("{base}/{action}")).json(&body).send()?)?.json()?;
    println!("approval {id} ({} in run {}) {}", a["node"].as_str().unwrap_or("?"), a["runId"].as_str().unwrap_or("?"), d["state"].as_str().unwrap_or("?"));
    Ok(())
}

fn usage(api: &Api, ns: &str, output: Output) -> Result<()> {
    let u: Value = ok(api.req(reqwest::Method::GET, &format!("/v1/namespaces/{ns}/usage")).send()?)?.json()?;
    match output {
        Output::Json => {
            println!("{}", serde_json::to_string_pretty(&u)?);
            return Ok(());
        }
        Output::Yaml => {
            print!("{}", serde_yaml_ng::to_string(&u)?);
            return Ok(());
        }
        Output::Table => {}
    }
    let pct = |used: f64, limit: f64| if limit > 0.0 { format!("{:.0}%", 100.0 * used / limit) } else { "-".into() };
    let num = |v: &Value| v.as_f64().unwrap_or(0.0);
    let mut rows = vec![];
    for q in u["quotas"].as_array().into_iter().flatten() {
        let (used, hard) = (num(&q["used"]), num(&q["hard"]));
        rows.push(vec!["quota".into(), q["resource"].as_str().unwrap_or("?").into(), format!("{used}"), format!("{hard}"), pct(used, hard)]);
    }
    for b in u["budgets"].as_array().into_iter().flatten() {
        let name = format!("{} ({})", b["name"].as_str().unwrap_or("?"), b["window"].as_str().unwrap_or("?"));
        if let Some(t) = b.get("tokens") {
            let used = num(&t["used"]) + num(&t["reserved"]);
            rows.push(vec!["budget".into(), format!("{name} tokens"), format!("{used}"), format!("{}", num(&t["limit"])), pct(used, num(&t["limit"]))]);
        }
        if let Some(c) = b.get("costUSD") {
            let used = num(&c["used"]) + num(&c["reserved"]);
            rows.push(vec!["budget".into(), format!("{name} cost"), format!("${used:.4}"), format!("${:.2}", num(&c["limit"])), pct(used, num(&c["limit"]))]);
        }
    }
    if rows.is_empty() {
        eprintln!("No quotas or budgets in {ns}.");
    } else {
        print_table(&["TYPE", "NAME", "USED", "LIMIT", "%"].map(String::from), &rows);
    }
    let spend: Vec<Vec<String>> = u["modelSpendThisMonth"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| {
            let cost = c["costUSD"].as_f64().map_or("-".into(), |v| format!("${v:.4}"));
            vec![c["claim"].as_str().unwrap_or("?").into(), c["calls"].to_string(), c["inputTokens"].to_string(), c["outputTokens"].to_string(), cost]
        })
        .collect();
    if !spend.is_empty() {
        println!();
        print_table(&["CLAIM", "CALLS", "IN TOKENS", "OUT TOKENS", "COST (MONTH)"].map(String::from), &spend);
    }
    Ok(())
}

fn token_cmd(api: &Api, ns: &str, cmd: TokenCmd) -> Result<()> {
    use reqwest::Method as M;
    let base = |sa: &str| format!("/v1/namespaces/{ns}/serviceaccounts/{sa}/tokens");
    match cmd {
        TokenCmd::Create { service_account, description, ttl_seconds } => {
            let body = serde_json::json!({ "description": description, "ttlSeconds": ttl_seconds });
            let t: Value = ok(api.req(M::POST, &base(&service_account)).json(&body).send()?)?.json()?;
            eprintln!("token {} for serviceaccount {ns}/{service_account}; store it now, it can't be shown again:", t["id"].as_str().unwrap_or("?"));
            println!("{}", t["token"].as_str().unwrap_or_default());
        }
        TokenCmd::List { service_account } => {
            let l: Vec<Value> = ok(api.req(M::GET, &base(&service_account)).send()?)?.json()?;
            let s = |v: &Value| v.as_str().unwrap_or("-").to_owned();
            let rows: Vec<Vec<String>> = l
                .iter()
                .map(|t| {
                    let state = if t["revokedAt"].is_string() { "revoked" } else { "active" };
                    vec![s(&t["id"]), state.into(), s(&t["createdAt"]), s(&t["expiresAt"]), s(&t["description"])]
                })
                .collect();
            print_table(&["ID", "STATE", "CREATED", "EXPIRES", "DESCRIPTION"].map(String::from), &rows);
        }
        TokenCmd::Revoke { service_account, id } => {
            ok(api.req(M::DELETE, &format!("{}/{id}", base(&service_account))).send()?)?;
            println!("token {id} revoked");
        }
    }
    Ok(())
}

fn secret_cmd(api: &Api, ns: &str, cmd: SecretCmd) -> Result<()> {
    use reqwest::Method as M;
    let base = format!("/v1/namespaces/{ns}/secrets");
    let show = |m: &Value| println!("{}  v{}  keys: {}", m["name"].as_str().unwrap_or("?"), m["version"], m["keys"].as_array().map(|k| k.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(",")).unwrap_or_default());
    match cmd {
        SecretCmd::Set { name, pairs } => {
            let mut data = serde_json::Map::new();
            for p in pairs {
                let (k, v) = p.split_once('=').with_context(|| format!("{p:?}: want KEY=VALUE, KEY=- or KEY=env:VAR"))?;
                let v = if v == "-" {
                    let mut s = String::new();
                    std::io::stdin().read_line(&mut s)?;
                    s.trim_end_matches(['\r', '\n']).to_owned()
                } else if let Some(var) = v.strip_prefix("env:") {
                    std::env::var(var).with_context(|| format!("${var} is not set"))?
                } else {
                    v.to_owned()
                };
                data.insert(k.to_owned(), Value::String(v));
            }
            show(&ok(api.req(M::PUT, &format!("{base}/{name}")).json(&serde_json::json!({ "data": data })).send()?)?.json()?);
        }
        SecretCmd::List => {
            let l: Vec<Value> = ok(api.req(M::GET, &base).send()?)?.json()?;
            l.iter().for_each(show);
        }
        SecretCmd::Get { name } => show(&ok(api.req(M::GET, &format!("{base}/{name}")).send()?)?.json()?),
        SecretCmd::Delete { name } => {
            ok(api.req(M::DELETE, &format!("{base}/{name}")).send()?)?;
            println!("secret/{name} deleted");
        }
    }
    Ok(())
}

fn mailbox_cmd(api: &Api, ns: &str, cmd: MailboxCmd) -> Result<()> {
    use reqwest::Method as M;
    match cmd {
        MailboxCmd::Ls { mailbox, state } => {
            let mut q = vec![];
            if let Some(st) = &state {
                q.push(("state", st.as_str()));
            }
            let msgs: Vec<Value> = ok(api.req(M::GET, &format!("/v1/namespaces/{ns}/mailboxes/{mailbox}/messages")).query(&q).send()?)?.json()?;
            let rows: Vec<Vec<String>> = msgs
                .iter()
                .map(|m| {
                    let s = |k: &str| m[k].as_str().unwrap_or("-").to_owned();
                    let state = match m["deadReason"].as_str() {
                        Some(r) => format!("dead ({r})"),
                        None => s("state"),
                    };
                    vec![s("id"), m["seq"].to_string(), state, m["deliveries"].to_string(), s("sender"), s("correlationId")]
                })
                .collect();
            if rows.is_empty() {
                eprintln!("No messages.");
            } else {
                print_table(&["ID", "SEQ", "STATE", "DELIVERIES", "SENDER", "CORRELATION"].map(String::from), &rows);
            }
        }
        MailboxCmd::Send { mailbox, payload, correlation_id, dedup_key } => {
            let body = serde_json::json!({ "payload": json_arg(&payload)?, "correlationId": correlation_id, "dedupKey": dedup_key });
            let r: Value = ok(api.req(M::POST, &format!("/v1/namespaces/{ns}/mailboxes/{mailbox}/messages")).json(&body).send()?)?.json()?;
            let dup = if r["duplicate"] == true { " (duplicate of an earlier send)" } else { "" };
            println!("{}{dup}", r["message"]["id"].as_str().unwrap_or("?"));
        }
        MailboxCmd::Requeue { mailbox, id } => {
            ok(api.req(M::POST, &format!("/v1/namespaces/{ns}/mailboxes/{mailbox}/messages/{id}/requeue")).send()?)?;
            println!("message {id} requeued");
        }
    }
    Ok(())
}
