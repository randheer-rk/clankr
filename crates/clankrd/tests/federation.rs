//! Multi-cluster placement between two real control planes: cluster A starts a run on cluster
//! B only when residency allows, B holds the identical workflow and the required secrets.

use clankr_store::Store;
use clankr_store::embedded::DevPostgres;
use serde_json::{Value, json};

struct Cp {
    _pg: DevPostgres,
    store: Store,
    base: String,
    token: &'static str,
}

async fn control_plane(token: &'static str) -> Cp {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let pg = DevPostgres::start(None).await.unwrap();
    let store = Store::connect(&pg.database("clankr").await.unwrap()).await.unwrap();
    let state = clankrd::AppState::new(store.clone(), token.into(), clankr_store::secrets::MasterKey::generate()).await.unwrap();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, clankrd::router(state)).await.unwrap() });
    Cp { _pg: pg, store, base, token }
}

impl Cp {
    async fn req(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> (u16, Value) {
        let mut rb = reqwest::Client::new().request(method, format!("{}{path}", self.base)).bearer_auth(self.token);
        if let Some(b) = body {
            rb = rb.json(&b);
        }
        let r = rb.send().await.unwrap();
        let st = r.status().as_u16();
        (st, r.json().await.unwrap_or(Value::Null))
    }
    async fn apply(&self, kind: &str, ns: &str, name: &str, spec: Value) {
        let plural = clankr_types::plural(kind);
        let body = json!({"apiVersion": "clankr.io/v1alpha1", "kind": kind, "metadata": {"name": name, "namespace": ns}, "spec": spec});
        let (st, cur) = self.req(reqwest::Method::GET, &format!("/v1/namespaces/{ns}/{plural}/{name}"), None).await;
        let (st, out) = if st == 200 {
            let mut b = body;
            b["metadata"]["resourceVersion"] = cur["metadata"]["resourceVersion"].clone();
            self.req(reqwest::Method::PUT, &format!("/v1/namespaces/{ns}/{plural}/{name}"), Some(b)).await
        } else {
            self.req(reqwest::Method::POST, &format!("/v1/namespaces/{ns}/{plural}"), Some(body)).await
        };
        assert!(st == 200 || st == 201, "apply {kind} {name}: {st} {out}");
    }
    async fn secret(&self, ns: &str, name: &str, data: Value) {
        let (st, out) = self.req(reqwest::Method::PUT, &format!("/v1/namespaces/{ns}/secrets/{name}"), Some(json!({"data": data}))).await;
        assert_eq!(st, 200, "{out}");
    }
    async fn start(&self, cluster: &str, key: Option<&str>) -> (u16, Value) {
        self.req(reqwest::Method::POST, "/v1/namespaces/acme/workflows/wf/runs", Some(json!({"input": {"q": 1}, "cluster": cluster, "idempotencyKey": key}))).await
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn runs_are_placed_on_remote_clusters_only_under_policy() {
    let (a, b) = (control_plane("owner-a").await, control_plane("owner-b").await);
    let wf = json!({"entrypoint": "w", "nodes": {"w": {"type": "agent", "agentRef": "worker"}}});
    a.apply("Workflow", "acme", "wf", wf.clone()).await;
    b.apply("Workflow", "acme", "wf", wf.clone()).await;
    a.secret("clankr-system", "eu-token", json!({"token": b.token})).await;
    for (name, region) in [("eu-1", "eu-west"), ("us-1", "us-east")] {
        a.apply("Cluster", "clankr-system", name, json!({"endpoint": b.base, "region": region, "tokenSecretRef": {"name": "eu-token", "key": "token"}})).await;
    }
    a.apply("PlacementPolicy", "acme", "residency", json!({"allowedRegions": ["eu-west"], "requiredSecrets": ["db"]})).await;

    let (st, e) = a.start("us-1", None).await;
    assert_eq!(st, 403, "{e}");
    assert!(e["message"].as_str().unwrap().contains("us-east"), "residency refuses the US cluster: {e}");
    let (st, e) = a.start("eu-1", None).await;
    assert_eq!(st, 403);
    assert!(e["message"].as_str().unwrap().contains("lacks required secret acme/db"), "{e}");
    let (st, e) = a.start("nowhere", None).await;
    assert_eq!(st, 404, "{e}");

    b.secret("acme", "db", json!({"url": "postgres://eu"})).await;
    let (st, run) = a.start("eu-1", Some("order-42")).await;
    assert_eq!(st, 201, "{run}");
    assert_eq!(run["cluster"], json!({"name": "eu-1", "region": "eu-west"}));
    let id = run["runId"].as_str().unwrap().to_owned();
    let on_b = clankr_engine::Engine::new(b.store.clone()).get("acme", id.parse().unwrap()).await.unwrap();
    assert_eq!(on_b.state.data["input"], json!({"q": 1}), "the run lives on cluster B");
    assert!(clankr_engine::Engine::new(a.store.clone()).get("acme", id.parse().unwrap()).await.is_err(), "and not on A");

    let (_, again) = a.start("eu-1", Some("order-42")).await;
    assert_eq!(again["runId"], run["runId"], "a retried start is the same remote run");
    let (_, list) = a.req(reqwest::Method::GET, "/v1/namespaces/acme/remoteruns", None).await;
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["cluster"], "eu-1");
    let (st, live) = a.req(reqwest::Method::GET, &format!("/v1/namespaces/acme/remoteruns/{id}"), None).await;
    assert_eq!((st, live["status"].as_str()), (200, Some("Running")), "{live}");

    // B's workflow drifts: A refuses rather than run something else under the same name
    b.apply("Workflow", "acme", "wf", json!({"entrypoint": "w", "nodes": {"w": {"type": "agent", "agentRef": "other"}}})).await;
    let (st, e) = a.start("eu-1", None).await;
    assert_eq!(st, 409, "{e}");
    assert!(e["message"].as_str().unwrap().contains("differs"), "{e}");

    let denied: i64 = sqlx::query_scalar("select count(*) from audit_events where decision = 'deny' and action like 'start acme/wf on cluster%'").fetch_one(a.store.pool()).await.unwrap();
    assert_eq!(denied, 2, "policy refusals are audited");
}
