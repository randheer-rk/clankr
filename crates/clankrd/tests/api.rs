use std::time::Duration;

use clankr_store::Store;
use clankr_store::embedded::DevPostgres;
use futures_util::StreamExt;
use reqwest::StatusCode;
use serde_json::{Value, json};

const TOKEN: &str = "test-token";

struct Server {
    _pg: DevPostgres,
    base: String,
    http: reqwest::Client,
}

impl Server {
    async fn start() -> Self {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let pg = DevPostgres::start(None).await.unwrap();
        let store = Store::connect(&pg.database("clankr").await.unwrap()).await.unwrap();
        let state = clankrd::AppState::new(store, TOKEN.into(), clankr_store::secrets::MasterKey::generate()).await.unwrap();
        clankrd::spawn_ticker(state.engine.clone());
        let app = clankrd::router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { _pg: pg, base, http: reqwest::Client::new() }
    }

    fn req(&self, m: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.http.request(m, format!("{}{path}", self.base)).bearer_auth(TOKEN)
    }
}

/// Valid Agent; `image` picks a distinct digest so spec changes are visible.
fn agent(name: &str, image: &str) -> Value {
    let digest = format!("{:0>64}", image.bytes().map(|b| format!("{:x}", b % 16)).collect::<String>());
    json!({"apiVersion": "clankr.io/v1alpha1", "kind": "Agent",
           "metadata": {"name": name, "namespace": "research", "labels": {"team": "a"}},
           "spec": {"runtime": {"type": "oci", "image": format!("registry.example/{name}@sha256:{digest}")}}})
}

const AGENTS: &str = "/v1/namespaces/research/agents";

#[tokio::test]
async fn crud_errors_and_auth() {
    let s = Server::start().await;
    use reqwest::Method as M;

    // unauthenticated, then wrong token
    let r = s.http.get(format!("{}{AGENTS}", s.base)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    let r = s.http.get(format!("{}{AGENTS}", s.base)).bearer_auth("nope").send().await.unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(s.http.get(format!("{}/healthz", s.base)).send().await.unwrap().status(), StatusCode::OK);
    assert_eq!(s.http.get(format!("{}/readyz", s.base)).send().await.unwrap().status(), StatusCode::OK);
    // the web UI owns every non-API path; unknown API paths stay JSON 404s, never the app shell
    let r = s.http.get(format!("{}/v1/nope/at/all", s.base)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    assert_eq!(r.json::<Value>().await.unwrap()["code"], "NotFound");

    // dry run persists nothing
    let r = s.req(M::POST, &format!("{AGENTS}?dryRun=true")).json(&agent("a", "x@1")).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(s.req(M::GET, &format!("{AGENTS}/a")).send().await.unwrap().status(), StatusCode::NOT_FOUND);

    let r = s.req(M::POST, AGENTS).json(&agent("a", "x@1")).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);
    let created: Value = r.json().await.unwrap();

    // body/URL mismatch and unknown kind
    let r = s.req(M::POST, "/v1/namespaces/other/agents").json(&agent("b", "x@1")).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(r.json::<Value>().await.unwrap()["field"], "metadata.namespace");
    assert_eq!(s.req(M::GET, "/v1/namespaces/research/widgets").send().await.unwrap().status(), StatusCode::NOT_FOUND);

    // CAS: update with current rv succeeds, replaying the old rv conflicts
    let mut upd = created;
    upd["spec"] = agent("a", "x@2")["spec"].clone();
    let r = s.req(M::PUT, &format!("{AGENTS}/a")).json(&upd).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.json::<Value>().await.unwrap()["metadata"]["generation"], 2);
    let r = s.req(M::PUT, &format!("{AGENTS}/a")).json(&upd).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::CONFLICT);
    assert_eq!(r.json::<Value>().await.unwrap()["code"], "Conflict");

    // label selector, and cross-namespace list
    let l: Value = s.req(M::GET, &format!("{AGENTS}?labelSelector=team=a")).send().await.unwrap().json().await.unwrap();
    assert_eq!(l["items"].as_array().unwrap().len(), 1);
    let l: Value = s.req(M::GET, "/v1/agents?labelSelector=team=zzz").send().await.unwrap().json().await.unwrap();
    assert!(l["items"].as_array().unwrap().is_empty());

    let r = s.req(M::DELETE, &format!("{AGENTS}/a")).header("x-request-id", "del-1").send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.headers()["x-request-id"], "del-1");
    assert_eq!(s.req(M::GET, &format!("{AGENTS}/a")).send().await.unwrap().status(), StatusCode::NOT_FOUND);

    // audit: two denied auth attempts, then create/update/delete; dry runs and failures leave nothing
    let log: Vec<Value> = s.req(M::GET, "/v1/audit").send().await.unwrap().json().await.unwrap();
    let got: Vec<_> = log.iter().map(|e| (e["action"].as_str().unwrap(), e["decision"].as_str().unwrap(), e["actor"].as_str().unwrap())).collect();
    assert_eq!(
        got,
        [
            ("GET /v1/namespaces/research/agents", "deny", "anonymous"),
            ("GET /v1/namespaces/research/agents", "deny", "anonymous"),
            ("create", "allow", "owner"),
            ("update", "allow", "owner"),
            ("delete", "allow", "owner"),
        ]
    );
    assert_eq!(log[4]["requestId"], "del-1");
}

/// Collects SSE `data:` payloads (skipping keepalives/bookmarks) until `n` arrive.
async fn sse_events(resp: reqwest::Response, n: usize) -> Vec<Value> {
    let mut body = resp.bytes_stream();
    let (mut buf, mut out) = (String::new(), vec![]);
    while out.len() < n {
        let chunk = tokio::time::timeout(Duration::from_secs(10), body.next()).await.expect("watch stalled").unwrap().unwrap();
        buf.push_str(std::str::from_utf8(&chunk).unwrap());
        while let Some(i) = buf.find("\n\n") {
            let frame: String = buf.drain(..i + 2).collect();
            if let Some(d) = frame.lines().find_map(|l| l.strip_prefix("data:")).map(str::trim).filter(|d| !d.is_empty()) {
                out.push(serde_json::from_str(d).unwrap());
            }
        }
    }
    out
}

#[tokio::test]
async fn watch_streams_live_changes_and_resumes_from_cursor() {
    let s = Server::start().await;
    use reqwest::Method as M;
    s.req(M::POST, AGENTS).json(&agent("pre", "x@1")).send().await.unwrap();

    // fresh watch: initial Added for existing, then live events woken by NOTIFY
    let w = s.req(M::GET, &format!("{AGENTS}?watch=true")).send().await.unwrap();
    let writer = {
        let (base, http) = (s.base.clone(), s.http.clone());
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            http.post(format!("{base}{AGENTS}")).bearer_auth(TOKEN).json(&agent("live", "x@1")).send().await.unwrap();
            http.delete(format!("{base}{AGENTS}/pre")).bearer_auth(TOKEN).send().await.unwrap();
        })
    };
    let ev = sse_events(w, 3).await;
    writer.await.unwrap();
    let got: Vec<_> = ev.iter().map(|e| (e["type"].as_str().unwrap(), e["object"]["metadata"]["name"].as_str().unwrap())).collect();
    assert_eq!(got, [("Added", "pre"), ("Added", "live"), ("Deleted", "pre")]);

    // resuming from the first live revision replays only what came after it
    let cursor = ev[1]["object"]["metadata"]["resourceVersion"].as_i64().unwrap();
    let w = s.req(M::GET, &format!("{AGENTS}?watch=true&cursor={cursor}")).send().await.unwrap();
    let ev = sse_events(w, 1).await;
    assert_eq!(ev[0]["type"], "Deleted");
}

#[tokio::test]
async fn workflow_run_over_http_with_long_poll_lease() {
    let s = Server::start().await;
    use reqwest::Method as M;
    let wf = |spec: Value| json!({"apiVersion": "clankr.io/v1alpha1", "kind": "Workflow", "metadata": {"name": "rr", "namespace": "research"}, "spec": spec});

    // admission rejects a broken graph with the field path
    let r = s.req(M::POST, "/v1/namespaces/research/workflows").json(&wf(json!({"entrypoint": "nope", "nodes": {}}))).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(r.json::<Value>().await.unwrap()["field"], "spec.entrypoint");

    let spec = json!({"entrypoint": "work", "nodes": {
        "work": {"type": "agent", "agentRef": "w", "next": "review"},
        "review": {"type": "approval", "approverGroup": "eds"}}});
    assert_eq!(s.req(M::POST, "/v1/namespaces/research/workflows").json(&wf(spec)).send().await.unwrap().status(), StatusCode::CREATED);

    // a worker long-polls before any run exists and is woken by the task NOTIFY
    let poll = {
        let (base, http) = (s.base.clone(), s.http.clone());
        tokio::spawn(async move {
            http.post(format!("{base}/v1/namespaces/research/tasks/lease")).bearer_auth(TOKEN)
                .json(&json!({"queue": "agent/w", "owner": "w1", "waitSeconds": 10})).send().await.unwrap()
        })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    let started = std::time::Instant::now();
    let run: Value = s.req(M::POST, "/v1/namespaces/research/workflows/rr/runs").header("idempotency-key", "k1")
        .json(&json!({"input": {"q": 1}})).send().await.unwrap().json().await.unwrap();
    let id = run["runId"].as_str().unwrap().to_owned();
    let leased = poll.await.unwrap();
    assert_eq!(leased.status(), StatusCode::OK);
    assert!(started.elapsed() < Duration::from_millis(900), "lease should wake on notify, not the 1s re-poll");
    let t: Value = leased.json().await.unwrap();
    assert_eq!(t["input"]["input"]["q"], 1);

    // same idempotency key -> same run
    let again: Value = s.req(M::POST, "/v1/namespaces/research/workflows/rr/runs").header("idempotency-key", "k1")
        .json(&json!({})).send().await.unwrap().json().await.unwrap();
    assert_eq!(again["runId"], run["runId"]);

    let done = json!({"runId": id, "task": t["task"], "epoch": t["epoch"], "result": {"draft": "d"}});
    assert_eq!(s.req(M::POST, "/v1/namespaces/research/tasks/complete").json(&done).send().await.unwrap().status(), StatusCode::NO_CONTENT);
    let stale = json!({"runId": id, "task": t["task"], "epoch": 0, "result": {}});
    let r = s.req(M::POST, "/v1/namespaces/research/tasks/complete").json(&stale).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::CONFLICT);
    assert_eq!(r.json::<Value>().await.unwrap()["code"], "StaleLease");

    // approvals go through the inbox, never raw signals
    let r = s.req(M::POST, &format!("/v1/namespaces/research/runs/{id}/signal")).json(&json!({"signal": "approve.review", "payload": {"approved": true}})).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    let pending: Vec<Value> = s.req(M::GET, "/v1/namespaces/research/approvals?state=pending").send().await.unwrap().json().await.unwrap();
    let a = &pending[0];
    let r = s.req(M::POST, &format!("/v1/namespaces/research/approvals/{}/approve", a["id"].as_str().unwrap())).json(&json!({"digest": a["digest"]})).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let r: Value = s.req(M::GET, &format!("/v1/namespaces/research/runs/{id}")).send().await.unwrap().json().await.unwrap();
    assert_eq!(r["status"], "Succeeded");
    let h: Vec<Value> = s.req(M::GET, &format!("/v1/namespaces/research/runs/{id}/history")).send().await.unwrap().json().await.unwrap();
    assert_eq!(h.last().unwrap()["event"]["type"], "RunCompleted");
    assert_eq!(s.req(M::GET, &format!("/v1/namespaces/other/runs/{id}")).send().await.unwrap().status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn context_api_requires_claim_and_enforces_cas() {
    let s = Server::start().await;
    use reqwest::Method as M;
    let key = "/v1/namespaces/research/contexts/mem/keys/research/progress";
    // no ContextClaim yet: the keyspace doesn't exist
    assert_eq!(s.req(M::GET, key).send().await.unwrap().status(), StatusCode::NOT_FOUND);
    let claim = json!({"apiVersion": "clankr.io/v1alpha1", "kind": "ContextClaim", "metadata": {"name": "mem", "namespace": "research"}, "spec": {"capabilities": ["keyValue"]}});
    s.req(M::POST, "/v1/namespaces/research/contextclaims").json(&claim).send().await.unwrap();

    let r: Value = s.req(M::PUT, key).header("x-clankr-on-behalf-of", "agent/researcher").json(&json!({"value": {"step": 1}, "ifVersion": 0})).send().await.unwrap().json().await.unwrap();
    assert_eq!(r["version"], 1);
    assert_eq!(r["writer"], "owner/agent/researcher");
    let r = s.req(M::PUT, key).json(&json!({"value": {"step": 2}, "ifVersion": 0})).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::CONFLICT);
    let r: Value = s.req(M::PUT, key).json(&json!({"value": {"step": 2}, "ifVersion": 1})).send().await.unwrap().json().await.unwrap();
    assert_eq!(r["version"], 2);
    let got: Value = s.req(M::GET, key).send().await.unwrap().json().await.unwrap();
    assert_eq!(got["value"]["step"], 2);
    let l: Vec<Value> = s.req(M::GET, "/v1/namespaces/research/contexts/mem/keys?prefix=research/").send().await.unwrap().json().await.unwrap();
    assert_eq!(l.len(), 1);
    assert_eq!(s.req(M::DELETE, &format!("{key}?ifVersion=2")).send().await.unwrap().status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn secrets_are_write_only_and_models_go_through_the_gateway() {
    let s = Server::start().await;
    use reqwest::Method as M;
    let r: Value = s.req(M::PUT, "/v1/namespaces/research/secrets/llm").json(&json!({"data": {"apiKey": "sk-very-secret"}})).send().await.unwrap().json().await.unwrap();
    assert_eq!(r["keys"], json!(["apiKey"]));
    let body = s.req(M::GET, "/v1/namespaces/research/secrets/llm").send().await.unwrap().text().await.unwrap();
    assert!(!body.contains("sk-very-secret"), "no API returns secret values");
    let body = s.req(M::GET, "/v1/namespaces/research/secrets").send().await.unwrap().text().await.unwrap();
    assert!(!body.contains("sk-very-secret"));

    // admission validates provider specs
    let pc = |spec: Value| json!({"apiVersion": "clankr.io/v1alpha1", "kind": "ProviderConnection", "metadata": {"name": "p", "namespace": "research"}, "spec": spec});
    let r = s.req(M::POST, "/v1/namespaces/research/providerconnections").json(&pc(json!({"provider": "openai-compatible", "endpoint": "http://evil.example/v1", "secretRef": {"name": "llm", "key": "apiKey"}, "allowedModels": ["m"]}))).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::UNPROCESSABLE_ENTITY);
    s.req(M::POST, "/v1/namespaces/research/providerconnections").json(&pc(json!({"provider": "mock", "allowedModels": ["mock-1"]}))).send().await.unwrap();
    let mc = json!({"apiVersion": "clankr.io/v1alpha1", "kind": "ModelClaim", "metadata": {"name": "default", "namespace": "research"}, "spec": {"providerConnection": "p", "model": "mock-1"}});
    s.req(M::POST, "/v1/namespaces/research/modelclaims").json(&mc).send().await.unwrap();

    let r: Value = s.req(M::POST, "/v1/namespaces/research/models/default/generate").header("x-clankr-on-behalf-of", "agent/researcher")
        .json(&json!({"messages": [{"role": "user", "content": "ping"}]})).send().await.unwrap().json().await.unwrap();
    assert_eq!(r["text"], "mock(mock-1): ping");
    assert_eq!(r["provider"], "mock");
    let r = s.req(M::POST, "/v1/namespaces/research/models/nope/generate").json(&json!({"messages": [{"role": "user", "content": "x"}]})).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn recorded_model_calls_export_per_run() {
    let s = Server::start().await;
    use reqwest::Method as M;
    let r = |kind: &str, name: &str, spec: Value| json!({"apiVersion": "clankr.io/v1alpha1", "kind": kind, "metadata": {"name": name, "namespace": "research"}, "spec": spec});
    for (plural, body) in [
        ("providerconnections", r("ProviderConnection", "rec", json!({"provider": "mock", "allowedModels": ["m"], "record": true}))),
        ("modelclaims", r("ModelClaim", "c", json!({"providerConnection": "rec", "model": "m"}))),
    ] {
        assert!(s.req(M::POST, &format!("/v1/namespaces/research/{plural}")).json(&body).send().await.unwrap().status().is_success());
    }
    let run = "6f1c1e0e-0000-4000-8000-000000000001";
    let g = s.req(M::POST, "/v1/namespaces/research/models/c/generate").json(&json!({"messages": [{"role": "user", "content": "hi"}], "runId": run})).send().await.unwrap();
    assert!(g.status().is_success());
    let t: Vec<Value> = s.req(M::GET, &format!("/v1/namespaces/research/runs/{run}/traces")).send().await.unwrap().json().await.unwrap();
    assert_eq!(t.len(), 1);
    assert_eq!((t[0]["response"].as_str(), t[0]["request"]["messages"][0]["content"].as_str()), (Some("mock(m): hi"), Some("hi")));
    assert_eq!(t[0]["promptSha256"].as_str().map(str::len), Some(64));
    let none: Vec<Value> = s.req(M::GET, "/v1/namespaces/research/runs/6f1c1e0e-0000-4000-8000-000000000002/traces").send().await.unwrap().json().await.unwrap();
    assert!(none.is_empty());
}
