//! AgentCronJob and EventTrigger: workflows started on a schedule and by published events, and
//! resource kinds without a controller refused instead of silently stored.

use chrono::{TimeZone, Utc};
use clankr_store::Store;
use clankr_store::embedded::DevPostgres;
use reqwest::Method as M;
use serde_json::{Value, json};

const TOKEN: &str = "sched-token";
const NS: &str = "sched";

fn res(kind: &str, name: &str, spec: Value) -> Value {
    json!({"apiVersion": "clankr.io/v1alpha1", "kind": kind, "metadata": {"name": name, "namespace": NS}, "spec": spec})
}

#[tokio::test(flavor = "multi_thread")]
async fn cron_jobs_triggers_and_unbuilt_kinds() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let pg = DevPostgres::start(None).await.unwrap();
    let store = Store::connect(&pg.database("clankr").await.unwrap()).await.unwrap();
    let state = clankrd::AppState::new(store.clone(), TOKEN.into(), clankr_store::secrets::MasterKey::generate()).await.unwrap();
    let engine = state.engine.clone();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, clankrd::router(state)).await.unwrap() });
    let http = reqwest::Client::new();
    let api = |m: M, path: &str, body: Value| {
        let rb = http.request(m, format!("{base}{path}")).bearer_auth(TOKEN).json(&body);
        async move {
            let r = rb.send().await.unwrap();
            (r.status().as_u16(), r.json::<Value>().await.unwrap_or(Value::Null))
        }
    };
    let create = |kind: &str, name: &str, spec: Value| {
        let plural = clankr_types::plural(kind);
        let body = res(kind, name, spec);
        let path = format!("/v1/namespaces/{NS}/{plural}");
        async move { api(M::POST, &path, body).await }
    };

    // a workflow that waits for a signal, so its runs stay Running
    assert_eq!(create("Workflow", "report", json!({"entrypoint": "w", "nodes": {"w": {"type": "signal", "signal": "go"}}})).await.0, 201);

    // cron: an invalid schedule is refused; a valid one starts one run per due minute
    let (st, e) = create("AgentCronJob", "bad", json!({"schedule": "61 * * * *", "workflow": "report"})).await;
    assert_eq!((st, e["field"].as_str()), (422, Some("spec.schedule")), "{e}");
    assert_eq!(create("AgentCronJob", "nightly", json!({"schedule": "0 2 * * *", "workflow": "report", "input": {"kind": "nightly"}})).await.0, 201);
    assert_eq!(create("AgentCronJob", "often", json!({"schedule": "*/5 * * * *", "workflow": "report", "concurrencyPolicy": "Allow"})).await.0, 201);
    let t = |h, m| Utc.with_ymd_and_hms(2026, 11, 4, h, m, 17).unwrap();
    let run_due = |at| {
        let (s, e) = (store.clone(), engine.clone());
        async move { clankrd::cron::run_due(&s, &e, at).await.unwrap() }
    };
    assert_eq!(run_due(t(1, 59)).await, 0, "nothing due");
    assert_eq!(run_due(t(2, 0)).await, 2, "02:00 matches both");
    assert_eq!(run_due(t(2, 0)).await, 0, "the same minute again starts nothing");
    let status = |name: &'static str| {
        let s = store.clone();
        async move { s.get(NS, "AgentCronJob", name).await.unwrap().status.unwrap() }
    };
    let nightly_run: uuid::Uuid = status("nightly").await["lastRun"].as_str().unwrap().parse().unwrap();
    let run = engine.get(NS, nightly_run).await.unwrap();
    assert_eq!(run.state.data["input"], json!({"kind": "nightly"}));
    // the next day, the nightly run is still waiting: Forbid skips; Allow starts another
    let next = Utc.with_ymd_and_hms(2026, 11, 5, 2, 0, 0).unwrap();
    assert_eq!(run_due(next).await, 1, "only the Allow job (every 5 minutes) starts");
    let st = status("nightly").await;
    assert!(st["lastSkipped"]["reason"].as_str().unwrap().contains("Forbid"), "{st}");
    assert_eq!(st["lastRun"], json!(nightly_run), "no second nightly run");
    // suspended jobs don't run
    let mut job = store.get(NS, "AgentCronJob", "often").await.unwrap();
    job.spec["suspend"] = json!(true);
    store.update(&clankr_types::RequestCtx { actor: "owner".into(), request_id: "t".into() }, job).await.unwrap();
    assert_eq!(run_due(Utc.with_ymd_and_hms(2026, 11, 5, 2, 5, 0).unwrap()).await, 0);

    // triggers: a published event starts the workflow with the event as input, once per dedup key
    assert_eq!(create("EventTrigger", "on-order", json!({"topic": "orders.created", "workflow": "report", "input": {"from": "trigger"}})).await.0, 201);
    let publish = |key: &str| api(M::POST, &format!("/v1/namespaces/{NS}/topics/orders.created/publish"), json!({"payload": {"order": 7}, "dedupKey": key}));
    let (st, p1) = publish("o-7").await;
    assert_eq!(st, 200, "{p1}");
    let run_id: uuid::Uuid = p1["runs"][0]["run"].as_str().unwrap().parse().unwrap();
    let run = engine.get(NS, run_id).await.unwrap();
    assert_eq!(run.state.data["input"], json!({"from": "trigger", "event": {"order": 7}, "topic": "orders.created"}));
    let (_, p2) = publish("o-7").await;
    assert_eq!(p2["runs"][0]["run"], p1["runs"][0]["run"], "a republished event starts no second run");

    // kinds nothing acts on are refused, not silently stored
    let (st, e) = create("AgentPool", "p", json!({})).await;
    assert_eq!((st, e["code"].as_str()), (422, Some("UnsupportedCapability")), "{e}");
    assert!(e["message"].as_str().unwrap().contains("no controller yet"));
}
