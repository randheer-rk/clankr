//! Inbound webhooks (ADR 0036): a signed delivery is published to its topic once; a bad or
//! missing signature is refused and audited; the raw secret never leaves the control plane.

use clankr_store::Store;
use clankr_store::embedded::DevPostgres;
use clankr_types::{RequestCtx, Resource};
use hmac::{KeyInit, Mac};
use reqwest::Method as M;
use serde_json::{Value, json};

const NS: &str = "ops";

fn res(kind: &str, name: &str, spec: Value) -> Resource {
    serde_json::from_value(json!({"apiVersion": "clankr.io/v1alpha1", "kind": kind, "metadata": {"name": name, "namespace": NS}, "spec": spec})).unwrap()
}

fn hmac_hex(secret: &str, msg: &[u8]) -> String {
    let mut m = hmac::Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    m.update(msg);
    hex::encode(m.finalize().into_bytes())
}

#[tokio::test(flavor = "multi_thread")]
async fn signed_deliveries_become_events() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let pg = DevPostgres::start(None).await.unwrap();
    let store = Store::connect(&pg.database("clankr").await.unwrap()).await.unwrap();
    let state = clankrd::AppState::new(store.clone(), "tok".into(), clankr_store::secrets::MasterKey::generate()).await.unwrap();
    let master = state.master.clone();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, clankrd::router(state)).await.unwrap() });
    let ctx = RequestCtx { actor: "owner".into(), request_id: "t".into() };
    let http = reqwest::Client::new();

    store.secret_put(&ctx, &master, NS, "gh", &[("secret".to_owned(), "octocat".to_owned())].into()).await.unwrap();
    store.secret_put(&ctx, &master, NS, "stripe", &[("secret".to_owned(), "whsec_x".to_owned())].into()).await.unwrap();
    store.create(&ctx, res("Webhook", "github", json!({"topic": "github.push", "secretRef": {"name": "gh", "key": "secret"}, "signature": "github", "maxBodyBytes": 200}))).await.unwrap();
    store.create(&ctx, res("Webhook", "stripe", json!({"topic": "payments", "secretRef": {"name": "stripe", "key": "secret"}, "signature": "stripe"}))).await.unwrap();
    store.create(&ctx, res("AgentMailbox", "inbox", json!({}))).await.unwrap();
    store.create(&ctx, res("EventSubscription", "pushes", json!({"topic": "github.push", "mailbox": "inbox"}))).await.unwrap();
    store.create(&ctx, res("Workflow", "settle", json!({"entrypoint": "w", "nodes": {"w": {"type": "signal", "signal": "go"}}}))).await.unwrap();
    store.create(&ctx, res("EventTrigger", "on-payment", json!({"topic": "payments", "workflow": "settle"}))).await.unwrap();

    // GitHub: a signed push lands in the subscriber's mailbox, with the delivery's headers
    let body = r#"{"ref":"refs/heads/main","after":"abc123"}"#;
    let gh = |sig: String, delivery: &str| {
        http.request(M::POST, format!("{base}/hooks/{NS}/github")).header("x-hub-signature-256", sig).header("x-github-delivery", delivery).header("x-github-event", "push").header("content-type", "application/json").body(body)
    };
    let r = gh(format!("sha256={}", hmac_hex("octocat", body.as_bytes())), "d-1").send().await.unwrap();
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["delivered"], 1, "{v}");
    let m = store.mailbox_receive(NS, "inbox", "t", 1, 10.0).await.unwrap().pop().expect("delivered");
    assert_eq!(m.sender, "webhook/github");
    assert_eq!(m.payload["body"]["after"], "abc123");
    assert_eq!(m.payload["headers"]["x-github-event"], "push");
    assert!(m.payload["headers"].get("x-hub-signature-256").is_none(), "signatures aren't forwarded");
    assert_eq!(gh(format!("sha256={}", hmac_hex("octocat", body.as_bytes())), "d-1").send().await.unwrap().status(), 200);
    assert!(store.mailbox_receive(NS, "inbox", "t", 1, 1.0).await.unwrap().is_empty(), "a retried delivery isn't delivered twice");
    // a bad signature is refused and audited; the secret isn't needed to say so
    let r = gh(format!("sha256={}", hmac_hex("wrong", body.as_bytes())), "d-2").send().await.unwrap();
    assert_eq!(r.status(), 401);
    let denied = store.audit_latest(10).await.unwrap().into_iter().find(|e| e.actor == format!("webhook/{NS}/github") && e.decision == "deny").expect("audit denial");
    assert_eq!(denied.reason.as_deref(), Some("bad signature"));
    // an unknown hook looks the same as a bad signature; an oversized body is refused
    assert_eq!(http.post(format!("{base}/hooks/{NS}/nope")).body("x").send().await.unwrap().status(), 401);
    let big = "x".repeat(300);
    assert_eq!(gh(format!("sha256={}", hmac_hex("octocat", big.as_bytes())), "d-3").body(big.clone()).send().await.unwrap().status(), 422);

    // Stripe: `t=…,v1=…` over `t.body`, deduplicated by the event id; the trigger starts a run
    let ev = r#"{"id":"evt_9","type":"payment_intent.succeeded"}"#;
    let t = chrono::Utc::now().timestamp();
    let sig = format!("t={t},v1={}", hmac_hex("whsec_x", format!("{t}.{ev}").as_bytes()));
    let r = http.post(format!("{base}/hooks/{NS}/stripe")).header("stripe-signature", &sig).body(ev).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    let run = v["runs"][0]["run"].as_str().expect("a run started").to_owned();
    let r: Value = http.post(format!("{base}/hooks/{NS}/stripe")).header("stripe-signature", &sig).body(ev).send().await.unwrap().json().await.unwrap();
    assert_eq!(r["runs"][0]["run"], run, "the same event starts no second run");
    let stale = format!("t={},v1={}", t - 3600, hmac_hex("whsec_x", format!("{}.{ev}", t - 3600).as_bytes()));
    assert_eq!(http.post(format!("{base}/hooks/{NS}/stripe")).header("stripe-signature", stale).body(ev).send().await.unwrap().status(), 401, "old timestamps are replays");
}
