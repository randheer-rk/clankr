//! M1 exit / DEMO-02 (local): kill -9 the real daemon over and over while workers drive several
//! runs to completion, then check the durability invariants against the database.
//!
//! Each engine step is one Postgres transaction, so a kill lands either before commit (rolled
//! back) or after it (durable; the client sees a dropped connection and retries, which the
//! epoch-checked ack makes idempotent). Many random kills cover both sides.

use std::collections::BTreeMap;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use clankr_engine::{Engine, Event, RunStatus};
use clankr_store::Store;
use clankr_store::embedded::DevPostgres;
use serde_json::{Value, json};
use uuid::Uuid;

const TOKEN: &str = "crash-token";
const NS: &str = "chaos";
const RUNS: usize = 6;
const BRANCHES: usize = 6;

struct Daemon {
    child: Option<Child>,
    url: String,
    listen: String,
}

impl Daemon {
    fn spawn(&mut self) {
        let child = Command::new(env!("CARGO_BIN_EXE_clankrd"))
            .env("DATABASE_URL", &self.url)
            .env("CLANKR_LISTEN", &self.listen)
            .env("CLANKR_TOKEN", TOKEN)
            .env("CLANKR_MASTER_KEY", "00".repeat(32))
            .env("CLANKR_INSECURE_HTTP", "true")
            .env("RUST_LOG", "warn")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        self.child = Some(child);
    }

    fn kill9(&mut self) {
        if let Some(mut c) = self.child.take() {
            c.kill().unwrap();
            c.wait().unwrap();
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.kill9();
    }
}

#[derive(Clone)]
struct Client {
    http: reqwest::Client,
    base: String,
}

impl Client {
    /// POST with retries across daemon restarts. Returns the final (status, body).
    async fn post(&self, path: &str, body: &Value) -> (u16, Value) {
        self.send(reqwest::Method::POST, path, Some(body)).await
    }

    async fn get(&self, path: &str) -> (u16, Value) {
        self.send(reqwest::Method::GET, path, None).await
    }

    async fn send(&self, m: reqwest::Method, path: &str, body: Option<&Value>) -> (u16, Value) {
        loop {
            let mut r = self.http.request(m.clone(), format!("{}{path}", self.base)).bearer_auth(TOKEN).timeout(Duration::from_secs(40));
            if let Some(b) = body {
                r = r.json(b);
            }
            if let Ok(r) = r.send().await {
                let status = r.status().as_u16();
                if status == 503 {
                    // storage hiccup while the daemon was being killed mid-transaction
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
                if let Ok(text) = r.text().await {
                    return (status, serde_json::from_str(&text).unwrap_or(Value::Null));
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn runs_survive_repeated_kill_9() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let pg = DevPostgres::start(None).await.unwrap();
    let url = pg.database("clankr").await.unwrap();
    let store = Store::connect(&url).await.unwrap();

    // fan out to BRANCHES researchers, join, approval, publish
    let mut nodes = serde_json::Map::new();
    let branches: Vec<String> = (0..BRANCHES).map(|i| format!("r{i}")).collect();
    nodes.insert("plan".into(), json!({"type": "agent", "agentRef": "planner", "next": branches}));
    for b in &branches {
        nodes.insert(b.clone(), json!({"type": "agent", "agentRef": "researcher", "next": "gather"}));
    }
    nodes.insert("gather".into(), json!({"type": "join", "next": "review"}));
    nodes.insert("review".into(), json!({"type": "approval", "approverGroup": "eds", "next": "publish"}));
    nodes.insert("publish".into(), json!({"type": "activity", "toolRef": "publisher"}));
    let wf = serde_json::from_value(json!({
        "apiVersion": "clankr.io/v1alpha1", "kind": "Workflow", "metadata": {"name": "report", "namespace": NS},
        "spec": {"entrypoint": "plan", "nodes": nodes}
    }))
    .unwrap();
    let ctx = clankr_types::RequestCtx { actor: "test".into(), request_id: "setup".into() };
    store.create(&ctx, wf).await.unwrap();

    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let listen = format!("127.0.0.1:{port}");
    let mut daemon = Daemon { child: None, url, listen: listen.clone() };
    daemon.spawn();
    let client = Client { http: reqwest::Client::new(), base: format!("http://{listen}") };

    // start runs (idempotency keys make retried starts safe)
    let mut ids = vec![];
    for i in 0..RUNS {
        let (status, run) = client.post(&format!("/v1/namespaces/{NS}/workflows/report/runs"), &json!({"input": {"i": i}, "idempotencyKey": format!("run-{i}")})).await;
        assert_eq!(status, 201, "{run}");
        ids.push(run["runId"].as_str().unwrap().parse::<Uuid>().unwrap());
    }

    let stop = Arc::new(AtomicBool::new(false));
    let mut workers = vec![];
    for (w, queue) in ["agent/planner", "agent/researcher", "agent/researcher", "agent/researcher", "tool/publisher"].iter().enumerate() {
        let (client, stop, queue) = (client.clone(), stop.clone(), queue.to_string());
        workers.push(tokio::spawn(async move {
            let mut accepted = 0u32;
            while !stop.load(Ordering::Relaxed) {
                let (status, t) = client.post(&format!("/v1/namespaces/{NS}/tasks/lease"), &json!({"queue": queue, "owner": format!("w{w}"), "ttlSeconds": 5, "waitSeconds": 1})).await;
                if status != 200 {
                    continue;
                }
                // simulated work; long enough that kills land mid-task
                tokio::time::sleep(Duration::from_millis(100 + fastrand_ms(200))).await;
                let body = json!({"runId": t["runId"], "task": t["task"], "epoch": t["epoch"], "result": {"by": w, "task": t["task"]}});
                let (status, resp) = client.post(&format!("/v1/namespaces/{NS}/tasks/complete"), &body).await;
                match status {
                    204 => accepted += 1,
                    // fenced (lease expired while the daemon was down) or run already finished: expected under chaos
                    409 => assert!(matches!(resp["code"].as_str(), Some("StaleLease" | "Conflict")), "{resp}"),
                    s => panic!("complete -> {s}: {resp}"),
                }
            }
            accepted
        }));
    }

    let approver = {
        let (client, stop) = (client.clone(), stop.clone());
        tokio::spawn(async move {
            while !stop.load(Ordering::Relaxed) {
                let (status, pending) = client.get(&format!("/v1/namespaces/{NS}/approvals?state=pending")).await;
                assert_eq!(status, 200, "{pending}");
                for a in pending.as_array().unwrap() {
                    let path = format!("/v1/namespaces/{NS}/approvals/{}/approve", a["id"].as_str().unwrap());
                    let (status, body) = client.post(&path, &json!({"digest": a["digest"]})).await;
                    assert!(status == 200 || status == 409, "approve -> {status} {body}");
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
    };

    // chaos: kill -9 and restart until every run is terminal
    let engine = Engine::new(store.clone());
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut kills = 0;
    loop {
        tokio::time::sleep(Duration::from_millis(100 + fastrand_ms(250))).await;
        let mut all_done = true;
        for id in &ids {
            all_done &= engine.get(NS, *id).await.unwrap().status.is_terminal();
        }
        if all_done {
            break;
        }
        assert!(Instant::now() < deadline, "runs did not finish under chaos ({kills} kills)");
        daemon.kill9();
        kills += 1;
        tokio::time::sleep(Duration::from_millis(fastrand_ms(200))).await;
        daemon.spawn();
    }
    stop.store(true, Ordering::Relaxed);
    let accepted: u32 = futures_util::future::join_all(workers).await.into_iter().map(|r| r.unwrap()).sum();
    approver.await.unwrap();
    eprintln!("{kills} kill -9s, {accepted} accepted completions");
    assert!(kills >= 5, "chaos too gentle: only {kills} kills");

    for id in &ids {
        let run = engine.get(NS, *id).await.unwrap();
        assert_eq!(run.status, RunStatus::Succeeded, "run {id}: {:?}", run.state.error);
        engine.verify(NS, *id).await.unwrap(); // gap-free history, replay == stored state
        let h = engine.history(NS, *id).await.unwrap();
        let mut completed: BTreeMap<String, u32> = BTreeMap::new();
        for e in &h {
            if let Event::TaskCompleted { task, .. } = &e.event {
                *completed.entry(task.split('#').next().unwrap().to_owned()).or_default() += 1;
            }
        }
        assert_eq!(completed.len(), BRANCHES + 2, "every task node completed: {completed:?}");
        assert!(completed.values().all(|n| *n == 1), "no node accepted twice: {completed:?}");
        assert_eq!(h.iter().filter(|e| matches!(e.event, Event::RunCompleted)).count(), 1);
        assert_eq!(h.iter().filter(|e| matches!(e.event, Event::SignalReceived { .. })).count(), 1, "approval resumed exactly once");
    }
    assert_eq!(accepted as usize, RUNS * (BRANCHES + 2), "each accepted ack is exactly one task");
    assert_eq!(store.verify_audit_chain().await.unwrap(), None);
}

/// Small pseudo-random delay without pulling in a rand crate for the test.
fn fastrand_ms(max: u64) -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(Instant::now().elapsed().as_nanos() ^ std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos());
    h.finish() % max.max(1)
}
