//! ResourceQuota admission and the usage report (PLA-015, CL-31-12): limits hold under
//! concurrent writes, only increases are refused, and running workflows are capped.

use clankr_store::Store;
use clankr_store::embedded::DevPostgres;
use reqwest::{Method as M, StatusCode};
use serde_json::{Value, json};

const OWNER: &str = "owner-token";
const NS: &str = "q";

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
        let state = clankrd::AppState::new(store, OWNER.into(), clankr_store::secrets::MasterKey::generate()).await.unwrap();
        let app = clankrd::router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { _pg: pg, base, http: reqwest::Client::new() }
    }

    async fn call(&self, m: M, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut r = self.http.request(m, format!("{}/v1/namespaces/{NS}{path}", self.base)).bearer_auth(OWNER);
        if let Some(b) = body {
            r = r.json(&b);
        }
        let r = r.send().await.unwrap();
        (r.status(), r.json().await.unwrap_or(Value::Null))
    }

    async fn put(&self, kind: &str, name: &str, spec: Value) -> StatusCode {
        let plural = clankr_types::plural(kind);
        let (st, cur) = self.call(M::GET, &format!("/{plural}/{name}"), None).await;
        let mut r = res(kind, name, spec);
        if st == StatusCode::OK {
            r["metadata"]["resourceVersion"] = cur["metadata"]["resourceVersion"].clone();
            self.call(M::PUT, &format!("/{plural}/{name}"), Some(r)).await.0
        } else {
            self.call(M::POST, &format!("/{plural}"), Some(r)).await.0
        }
    }
}

fn res(kind: &str, name: &str, spec: Value) -> Value {
    json!({"apiVersion": "clankr.io/v1alpha1", "kind": kind, "metadata": {"name": name, "namespace": NS}, "spec": spec})
}

fn agent(cpus: f64) -> Value {
    json!({"runtime": {"type": "oci", "image": format!("r.example/a@sha256:{}", "a".repeat(64))}, "resources": {"cpus": cpus, "memoryMiB": 256}})
}

#[tokio::test]
async fn quotas_hold_under_concurrency_and_cap_runs() {
    let s = Server::start().await;
    const TOO_MANY: StatusCode = StatusCode::TOO_MANY_REQUESTS;
    let (st, _) = s.call(M::POST, "/resourcequotas", Some(res("ResourceQuota", "q", json!({"hard": {"gpus": 1}})))).await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "unknown quota keys are refused");
    assert_eq!(s.put("ResourceQuota", "q", json!({"hard": {"count/agents": 3, "cpus": 2, "volumeMiB": 100, "activeRuns": 1}})).await, StatusCode::CREATED);

    // ten racing creates, three slots
    let creates = (0..10).map(|i| s.call(M::POST, "/agents", Some(res("Agent", &format!("a{i}"), agent(0.5)))));
    let codes: Vec<StatusCode> = futures_util::future::join_all(creates).await.into_iter().map(|r| r.0).collect();
    assert_eq!(codes.iter().filter(|c| **c == StatusCode::CREATED).count(), 3, "{codes:?}");
    assert!(codes.iter().all(|c| *c == StatusCode::CREATED || *c == TOO_MANY), "{codes:?}");
    let (_, list) = s.call(M::GET, "/agents", None).await;
    let first = list["items"][0]["metadata"]["name"].as_str().unwrap().to_owned();

    // compute: replicas x Agent resources, through either object
    let dep = |n: u32| json!({"agent": first, "replicas": n});
    assert_eq!(s.put("AgentDeployment", "d", dep(4)).await, StatusCode::CREATED, "4 x 0.5 = 2 cpus fits");
    assert_eq!(s.put("AgentDeployment", "d", dep(5)).await, TOO_MANY);
    let (st, err) = s.call(M::PUT, "/agentdeployments/d?dryRun=true", Some(res("AgentDeployment", "d", dep(5)))).await;
    assert_eq!(st, TOO_MANY, "dry runs are quota-checked too: {err}");
    assert!(err["message"].as_str().unwrap().contains("ResourceQuota cpus: 2.5 would exceed the limit 2"), "{err}");
    assert_eq!(s.put("Agent", &first, agent(1.0)).await, TOO_MANY, "growing the Agent counts against the deployment");

    // lowering the quota doesn't wedge the namespace: shrinking is still allowed
    assert_eq!(s.put("ResourceQuota", "q", json!({"hard": {"count/agents": 3, "cpus": 1, "volumeMiB": 100, "activeRuns": 1}})).await, StatusCode::OK);
    assert_eq!(s.put("AgentDeployment", "d", dep(3)).await, StatusCode::OK, "1.5 is over 1 but less than before");
    assert_eq!(s.put("AgentDeployment", "d", dep(4)).await, TOO_MANY);

    assert_eq!(s.put("VolumeClaim", "v1", json!({"capacityMiB": 64})).await, StatusCode::CREATED);
    assert_eq!(s.put("VolumeClaim", "v2", json!({"capacityMiB": 64})).await, TOO_MANY);

    // running workflows: no worker leases the agent task, so runs stay Running
    assert_eq!(s.put("Workflow", "w", json!({"entrypoint": "x", "nodes": {"x": {"type": "agent", "agentRef": first}}})).await, StatusCode::CREATED);
    let (st, run) = s.call(M::POST, "/workflows/w/runs", Some(json!({"idempotencyKey": "one"}))).await;
    assert_eq!(st, StatusCode::CREATED, "{run}");
    let (st, err) = s.call(M::POST, "/workflows/w/runs", Some(json!({}))).await;
    assert_eq!(st, TOO_MANY, "{err}");
    let (st, again) = s.call(M::POST, "/workflows/w/runs", Some(json!({"idempotencyKey": "one"}))).await;
    assert_eq!((st, &again["runId"]), (StatusCode::CREATED, &run["runId"]), "a retried start still returns its run at the limit");
    let id = run["runId"].as_str().unwrap();
    assert!(s.call(M::POST, &format!("/runs/{id}/cancel"), Some(json!({"reason": "test"}))).await.0.is_success());
    assert_eq!(s.call(M::POST, "/workflows/w/runs", Some(json!({}))).await.0, StatusCode::CREATED);

    assert_eq!(s.put("AgentBudget", "monthly", json!({"period": "month", "maxTokens": 1000, "maxCostUSD": 5})).await, StatusCode::CREATED);
    let (st, u) = s.call(M::GET, "/usage", None).await;
    assert_eq!(st, StatusCode::OK);
    let q = |k: &str| u["quotas"].as_array().unwrap().iter().find(|q| q["resource"] == k).cloned().unwrap();
    assert_eq!((q("count/agents")["used"].as_f64(), q("count/agents")["hard"].as_f64()), (Some(3.0), Some(3.0)));
    assert_eq!(q("cpus")["used"].as_f64(), Some(1.5));
    assert_eq!(q("volumeMiB")["used"].as_f64(), Some(64.0));
    assert_eq!(q("activeRuns")["used"].as_f64(), Some(1.0));
    let b = &u["budgets"][0];
    assert_eq!((b["name"].as_str(), b["tokens"]["limit"].as_i64(), b["costUSD"]["used"].as_f64()), (Some("monthly"), Some(1000), Some(0.0)));
    assert!(b["window"].as_str().unwrap().starts_with("month:"));
}
