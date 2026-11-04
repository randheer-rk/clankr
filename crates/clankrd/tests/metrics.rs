//! `/metrics` (ADR 0039): Prometheus text from the control plane's own tables, behind the
//! audit-read permission.

use clankr_store::Store;
use clankr_store::embedded::DevPostgres;
use clankr_types::{RequestCtx, Resource};
use serde_json::{Value, json};

const NS: &str = "obs";

fn res(kind: &str, name: &str, spec: Value) -> Resource {
    serde_json::from_value(json!({"apiVersion": "clankr.io/v1alpha1", "kind": kind, "metadata": {"name": name, "namespace": NS}, "spec": spec})).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn metrics_come_from_the_tables() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let pg = DevPostgres::start(None).await.unwrap();
    let store = Store::connect(&pg.database("clankr").await.unwrap()).await.unwrap();
    let state = clankrd::AppState::new(store.clone(), "tok".into(), clankr_store::secrets::MasterKey::generate()).await.unwrap();
    let (engine, gateway) = (state.engine.clone(), state.gateway.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, clankrd::router(state)).await.unwrap() });
    let ctx = RequestCtx { actor: "owner".into(), request_id: "m".into() };

    store.create(&ctx, res("ProviderConnection", "mock", json!({"provider": "mock", "allowedModels": ["m"]}))).await.unwrap();
    store.create(&ctx, res("ModelClaim", "default", json!({"providerConnection": "mock", "model": "m", "budget": {"maxTokensTotal": 10000}}))).await.unwrap();
    store.create(&ctx, res("Workflow", "wait", json!({"entrypoint": "w", "nodes": {"w": {"type": "signal", "signal": "go"}}}))).await.unwrap();
    engine.start(&ctx, NS, "wait", json!({}), None).await.unwrap();
    let req = serde_json::from_value(json!({"messages": [{"role": "user", "content": "hello there"}]})).unwrap();
    gateway.generate(&ctx, NS, "default", req).await.unwrap();

    let http = reqwest::Client::new();
    assert_eq!(http.get(format!("{base}/metrics")).send().await.unwrap().status(), 401, "scrapers need a token");
    let r = http.get(format!("{base}/metrics")).bearer_auth("tok").send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert!(r.headers()["content-type"].to_str().unwrap().starts_with("text/plain; version=0.0.4"));
    let text = r.text().await.unwrap();
    let line = |needle: &str| text.lines().find(|l| l.starts_with(needle)).map(str::to_owned).unwrap_or_else(|| panic!("no {needle} in:\n{text}"));
    assert_eq!(line("clankr_runs{"), format!("clankr_runs{{namespace=\"{NS}\",status=\"Running\"}} 1"));
    assert_eq!(line("clankr_model_calls_total{"), format!("clankr_model_calls_total{{namespace=\"{NS}\",claim=\"default\",outcome=\"ok\"}} 1"));
    assert!(line("clankr_model_input_tokens_total{").rsplit(' ').next().unwrap().parse::<i64>().unwrap() > 0);
    assert!(line("clankr_budget_used{").contains("bucket=\"default\",window=\"total\""));
    assert!(text.contains("# TYPE clankr_audit_seq gauge"));
    assert!(line("clankr_audit_seq ").split(' ').nth(1).unwrap().parse::<i64>().unwrap() > 0);
    assert!(text.contains("clankr_build_info{version=\""));
}
