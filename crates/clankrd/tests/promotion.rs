//! Image promotion by digest (BLD-17): stage order, evaluation gates, admin-only, tracking
//! Agents rewritten, instances restarted on the new digest, rollback and history.

use clankr_store::Store;
use clankr_store::embedded::DevPostgres;
use reqwest::{Method as M, StatusCode};
use serde_json::{Value, json};

const OWNER: &str = "owner-token";
const NS: &str = "img";
const REPO: &str = "registry.example/team/researcher";

fn digest(c: char) -> String {
    format!("sha256:{}", c.to_string().repeat(64))
}

struct Server {
    _pg: DevPostgres,
    store: Store,
    base: String,
    http: reqwest::Client,
}

impl Server {
    async fn start() -> Self {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let pg = DevPostgres::start(None).await.unwrap();
        let store = Store::connect(&pg.database("clankr").await.unwrap()).await.unwrap();
        let state = clankrd::AppState::new(store.clone(), OWNER.into(), clankr_store::secrets::MasterKey::generate()).await.unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = clankrd::router(state);
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { _pg: pg, store, base, http: reqwest::Client::new() }
    }

    async fn call(&self, token: &str, m: M, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut r = self.http.request(m, format!("{}/v1/namespaces/{NS}{path}", self.base)).bearer_auth(token);
        if let Some(b) = body {
            r = r.json(&b);
        }
        let r = r.send().await.unwrap();
        (r.status(), r.json().await.unwrap_or(Value::Null))
    }

    async fn promote(&self, stage: &str, d: &str, evidence: Option<Value>) -> (StatusCode, Value) {
        self.call(OWNER, M::POST, "/agentimages/researcher/promotions", Some(json!({"stage": stage, "digest": d, "reason": format!("ship {stage}"), "evidence": evidence}))).await
    }
}

fn res(kind: &str, name: &str, spec: Value) -> Value {
    json!({"apiVersion": "clankr.io/v1alpha1", "kind": kind, "metadata": {"name": name, "namespace": NS}, "spec": spec})
}

fn agent(image: &str) -> Value {
    res("Agent", "researcher", json!({"runtime": {"type": "oci", "image": image}, "track": {"image": "researcher", "stage": "production"}, "resources": {"memoryMiB": 256, "cpus": 0.5}}))
}

#[tokio::test]
async fn promote_gate_roll_out_and_roll_back() {
    let s = Server::start().await;
    let (d1, d2) = (digest('a'), digest('b'));
    let (st, err) = s.call(OWNER, M::POST, "/agentimages", Some(res("AgentImage", "bad", json!({"repository": "r.example/x:latest"})))).await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "tags are refused in the repository: {err}");
    let (st, _) = s.call(OWNER, M::POST, "/agentimages", Some(res("AgentImage", "researcher", json!({"repository": REPO, "gates": {"production": {"minEvalPassRate": 0.9}}})))).await;
    assert_eq!(st, StatusCode::CREATED);

    // digests only, stages in order, production gated on evaluation evidence
    assert_eq!(s.promote("dev", "latest", None).await.0, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(s.promote("dev", &d1, None).await.0, StatusCode::OK);
    let (st, e) = s.promote("production", &d1, None).await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    assert!(e["message"].as_str().unwrap().contains("never in staging"), "{e}");
    assert_eq!(s.promote("staging", &d1, None).await.0, StatusCode::OK);
    let (st, e) = s.promote("production", &d1, None).await;
    assert!(st == StatusCode::FORBIDDEN && e["message"].as_str().unwrap().contains("requires evaluation evidence"), "{e}");
    let (st, e) = s.promote("production", &d1, Some(json!({"eval": {"suite": "research", "passRate": 0.8}}))).await;
    assert!(st == StatusCode::FORBIDDEN && e["message"].as_str().unwrap().contains("evidence shows 80%"), "{e}");
    let good = json!({"eval": {"suite": "research", "passRate": 0.95}});
    assert_eq!(s.promote("production", &d1, Some(good.clone())).await.0, StatusCode::OK);
    assert_eq!(s.promote("production", &d1, Some(good.clone())).await.0, StatusCode::CONFLICT, "already there");

    // tracking Agents must run exactly what their stage holds
    let (st, e) = s.call(OWNER, M::POST, "/agents", Some(agent(&format!("{REPO}@{d2}")))).await;
    assert!(st == StatusCode::UNPROCESSABLE_ENTITY && e["message"].as_str().unwrap().contains("promote instead"), "{e}");
    assert_eq!(s.call(OWNER, M::POST, "/agents", Some(agent(&format!("{REPO}@{d1}")))).await.0, StatusCode::CREATED);

    // a running deployment of it
    s.store.register_node("node-a", "cred", &json!({}), &json!({"memoryMiB": 4096, "cpus": 4.0, "runtimes": ["oci"]})).await.unwrap();
    assert_eq!(s.call(OWNER, M::POST, "/agentdeployments", Some(res("AgentDeployment", "researcher", json!({"agent": "researcher", "replicas": 1})))).await.0, StatusCode::CREATED);
    let lead = s.store.acquire_leadership("scheduler", "test", 30.0).await.unwrap().unwrap();
    clankrd::scheduler::reconcile(&s.store, &lead).await.unwrap();
    let before = s.store.instances().await.unwrap().remove(0);
    assert_eq!(before.node.as_deref(), Some("node-a"));

    // only admins promote
    s.call(OWNER, M::POST, "/serviceaccounts", Some(res("ServiceAccount", "ci", json!({})))).await;
    s.call(OWNER, M::POST, "/rolebindings", Some(res("RoleBinding", "ci", json!({"role": "editor", "subjects": [format!("serviceaccount:{NS}/ci")]})))).await;
    let (_, t) = s.call(OWNER, M::POST, "/serviceaccounts/ci/tokens", Some(json!({}))).await;
    let ci = t["token"].as_str().unwrap();
    let (st, _) = s.call(ci, M::POST, "/agentimages/researcher/promotions", Some(json!({"stage": "dev", "digest": d2}))).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "editors can't promote");

    // promote d2 all the way: the tracking Agent is rewritten and its instance restarts on it
    for stage in ["dev", "staging"] {
        assert_eq!(s.promote(stage, &d2, None).await.0, StatusCode::OK);
    }
    let (st, p) = s.promote("production", &d2, Some(good)).await;
    assert_eq!((st, &p["agentsUpdated"], &p["previousDigest"]), (StatusCode::OK, &json!(["researcher"]), &json!(d1)));
    let (_, a) = s.call(OWNER, M::GET, "/agents/researcher", None).await;
    assert_eq!(a["spec"]["runtime"]["image"], format!("{REPO}@{d2}"));
    let r = clankrd::scheduler::reconcile(&s.store, &lead).await.unwrap();
    assert_eq!(r.restarted, 1);
    let after = s.store.instances().await.unwrap().remove(0);
    assert_eq!((after.id == before.id, after.node.as_deref(), after.epoch), (true, Some("node-a"), before.epoch + 1), "same instance, same node, new epoch");
    assert_eq!(clankrd::scheduler::reconcile(&s.store, &lead).await.unwrap().restarted, 0, "settles");

    // rollback goes to the previous production digest, and is history like anything else
    let (st, rb) = s.call(OWNER, M::POST, "/agentimages/researcher/rollback", Some(json!({"stage": "production"}))).await;
    assert_eq!((st, &rb["digest"], &rb["rollback"]), (StatusCode::OK, &json!(d1), &json!(true)));
    let (_, a) = s.call(OWNER, M::GET, "/agents/researcher", None).await;
    assert_eq!(a["spec"]["runtime"]["image"], format!("{REPO}@{d1}"));
    let (_, h) = s.call(OWNER, M::GET, "/agentimages/researcher/promotions?stage=production", None).await;
    let h = h.as_array().unwrap();
    assert_eq!(h.len(), 3);
    assert_eq!((&h[0]["rollback"], &h[0]["digest"], h[0]["reason"].as_str().unwrap().starts_with("rollback from sha256:bbbb")), (&json!(true), &json!(d1), true));
    assert_eq!(h[1]["evidence"]["eval"]["passRate"], json!(0.95));
    let audit = s.store.audit_since(0, 1000).await.unwrap();
    assert!(audit.iter().any(|e| e.kind.as_deref() == Some("AgentImage") && e.action.starts_with("rollback production")), "promotions are audited");
}
