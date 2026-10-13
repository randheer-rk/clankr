use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use clankr_gateway::{Gateway, GenerateRequest, Message, admit_connection};
use clankr_store::Store;
use clankr_store::embedded::DevPostgres;
use clankr_store::secrets::MasterKey;
use clankr_types::{ErrorCode, RequestCtx, Resource};
use serde_json::{Value, json};

const NS: &str = "research";
const KEY: &str = "sk-test-SECRET-do-not-leak";

fn ctx() -> RequestCtx {
    RequestCtx { actor: "owner".into(), request_id: "g".into() }
}

fn res(kind: &str, name: &str, spec: Value) -> Resource {
    serde_json::from_value(json!({"apiVersion": "clankr.io/v1alpha1", "kind": kind, "metadata": {"name": name, "namespace": NS}, "spec": spec})).unwrap()
}

/// What the fake provider saw on its last request.
#[derive(Default)]
struct Seen {
    headers: Vec<(String, String)>,
    body: Value,
}

/// Fake provider: records the request and answers with `reply` + `status`.
async fn fake(path: &'static str, status: StatusCode, reply: Value) -> (String, Arc<Mutex<Seen>>) {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let s = seen.clone();
    let app = axum::Router::new().route(
        path,
        post(move |h: HeaderMap, Json(body): Json<Value>| {
            let (s, reply) = (s.clone(), reply.clone());
            async move {
                *s.lock().unwrap() = Seen { headers: h.iter().map(|(k, v)| (k.to_string(), v.to_str().unwrap().to_owned())).collect(), body };
                (status, Json(reply))
            }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (url, seen)
}

fn header(s: &Seen, name: &str) -> Option<String> {
    s.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone())
}

async fn setup() -> (DevPostgres, Store, Gateway) {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let pg = DevPostgres::start(None).await.unwrap();
    let store = Store::connect(&pg.database("clankr").await.unwrap()).await.unwrap();
    let master = MasterKey::generate();
    store.secret_put(&ctx(), &master, NS, "llm", &BTreeMap::from([("apiKey".into(), KEY.into())])).await.unwrap();
    let gw = Gateway::new(store.clone(), master);
    (pg, store, gw)
}

fn ask(text: &str) -> GenerateRequest {
    GenerateRequest {
        messages: vec![Message { role: "user".into(), content: text.into() }],
        system: Some("be brief".into()),
        max_output_tokens: Some(100),
        run_id: None,
        task: None,
    }
}

#[tokio::test]
async fn anthropic_adapter_shapes_request_and_brokers_credential() {
    let (_pg, s, gw) = setup().await;
    let reply = json!({"type": "message", "content": [{"type": "text", "text": "Paris"}, {"type": "text", "text": "."}],
                       "stop_reason": "end_turn", "usage": {"input_tokens": 12, "output_tokens": 3}});
    let (url, seen) = fake("/v1/messages", StatusCode::OK, reply).await;
    s.create(&ctx(), res("ProviderConnection", "anth", json!({"provider": "anthropic", "endpoint": url, "secretRef": {"name": "llm", "key": "apiKey"}, "allowedModels": ["claude-opus-5"]}))).await.unwrap();
    s.create(&ctx(), res("ModelClaim", "reasoning", json!({"providerConnection": "anth", "model": "claude-opus-5", "limits": {"maxOutputTokens": 64}}))).await.unwrap();

    let r = gw.generate(&ctx(), NS, "reasoning", ask("capital of France?")).await.unwrap();
    assert_eq!((r.text.as_str(), r.usage.input_tokens, r.usage.output_tokens), ("Paris.", 12, 3));
    assert_eq!(r.model, "claude-opus-5");

    {
        let seen = seen.lock().unwrap();
        assert_eq!(header(&seen, "x-api-key").as_deref(), Some(KEY));
        assert_eq!(header(&seen, "anthropic-version").as_deref(), Some("2023-06-01"));
        assert_eq!(seen.body["system"], "be brief");
        assert_eq!(seen.body["max_tokens"], 64, "capped by the claim's maxOutputTokens");
        assert_eq!(seen.body["messages"], json!([{"role": "user", "content": "capital of France?"}]));
    }

    // the credential read is audited with its purpose; the value is in no audit field
    let audit = s.audit_since(0, 100).await.unwrap();
    let read = audit.iter().find(|e| e.action == "secret read apiKey").expect("secret read audited");
    assert_eq!(read.reason.as_deref(), Some("model gateway: ModelClaim reasoning"));
    assert!(!serde_json::to_string(&audit).unwrap().contains(KEY));
}

#[tokio::test]
async fn openai_compatible_adapter_and_error_mapping() {
    let (_pg, s, gw) = setup().await;
    let ok = json!({"choices": [{"message": {"role": "assistant", "content": "hi"}, "finish_reason": "stop"}], "usage": {"prompt_tokens": 5, "completion_tokens": 1}});
    let (url, seen) = fake("/chat/completions", StatusCode::OK, ok).await;
    s.create(&ctx(), res("ProviderConnection", "oa", json!({"provider": "openai-compatible", "endpoint": url, "secretRef": {"name": "llm", "key": "apiKey"}, "allowedModels": ["m"]}))).await.unwrap();
    s.create(&ctx(), res("ModelClaim", "fast", json!({"providerConnection": "oa", "model": "m"}))).await.unwrap();
    let r = gw.generate(&ctx(), NS, "fast", ask("hello")).await.unwrap();
    assert_eq!((r.text.as_str(), r.stop_reason.as_deref()), ("hi", Some("stop")));
    {
        let seen = seen.lock().unwrap();
        assert_eq!(header(&seen, "authorization").unwrap(), format!("Bearer {KEY}"));
        assert_eq!(seen.body["messages"][0], json!({"role": "system", "content": "be brief"}));
    }

    // upstream 429 -> QuotaExceeded, without leaking the key; the reservation is released
    let (url, _) = fake("/chat/completions", StatusCode::TOO_MANY_REQUESTS, json!({"error": {"message": "slow down"}})).await;
    s.create(&ctx(), res("ProviderConnection", "limited", json!({"provider": "openai-compatible", "endpoint": url, "secretRef": {"name": "llm", "key": "apiKey"}, "allowedModels": ["m"]}))).await.unwrap();
    s.create(&ctx(), res("ModelClaim", "lim", json!({"providerConnection": "limited", "model": "m", "budget": {"maxTokensTotal": 1000}}))).await.unwrap();
    let e = gw.generate(&ctx(), NS, "lim", ask("x")).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::QuotaExceeded);
    assert!(e.message.contains("slow down") && !e.message.contains(KEY));
    let (used, reserved): (i64, i64) = sqlx::query_as("select used, reserved from model_usage where claim = 'lim'").fetch_one(s.pool()).await.unwrap();
    assert_eq!((used, reserved), (0, 0));
}

#[tokio::test]
async fn budgets_allowlist_and_revocation() {
    let (_pg, s, gw) = setup().await;
    s.create(&ctx(), res("ProviderConnection", "mock", json!({"provider": "mock", "allowedModels": ["mock-1"]}))).await.unwrap();
    s.create(&ctx(), res("ModelClaim", "m", json!({"providerConnection": "mock", "model": "mock-1", "budget": {"maxTokensTotal": 110, "maxTokensPerRun": 105}}))).await.unwrap();

    // each call reserves ~1 input + 100 output tokens up front, then settles to actual use (~7);
    // a second call in the same run needs 7 + 101 > 105 per-run
    let run = uuid::Uuid::new_v4();
    let mut req = ask("hello");
    req.run_id = Some(run);
    let r = gw.generate(&ctx(), NS, "m", req.clone()).await.unwrap();
    assert_eq!(r.text, "mock(mock-1): hello");
    assert_eq!(gw.generate(&ctx(), NS, "m", req).await.unwrap_err().code, ErrorCode::QuotaExceeded);
    // a different run has its own allowance (7 + 101 <= 110 total), then the total ceiling bites (14 + 101 > 110)
    gw.generate(&ctx(), NS, "m", GenerateRequest { run_id: Some(uuid::Uuid::new_v4()), ..ask("again") }).await.unwrap();
    let e = gw.generate(&ctx(), NS, "m", GenerateRequest { run_id: Some(uuid::Uuid::new_v4()), ..ask("third") }).await.unwrap_err();
    assert!(e.message.contains("total"), "{e}");

    // model not in the connection's allowlist
    s.create(&ctx(), res("ModelClaim", "sneaky", json!({"providerConnection": "mock", "model": "gpt-big"}))).await.unwrap();
    assert_eq!(gw.generate(&ctx(), NS, "sneaky", ask("x")).await.unwrap_err().code, ErrorCode::Forbidden);

    // revoking the connection blocks the very next call
    s.delete(&ctx(), NS, "ProviderConnection", "mock", None).await.unwrap();
    assert_eq!(gw.generate(&ctx(), NS, "m", ask("x")).await.unwrap_err().code, ErrorCode::NotFound);

    let calls: i64 = sqlx::query_scalar("select count(*) from model_calls where outcome = 'ok'").fetch_one(s.pool()).await.unwrap();
    assert_eq!(calls, 2);
}

#[tokio::test]
async fn secrets_need_the_right_master_key() {
    let (_pg, s, _gw) = setup().await;
    let wrong = MasterKey::generate();
    let e = s.secret_read(&ctx(), &wrong, NS, "llm", "apiKey", "test").await.unwrap_err();
    assert!(e.message.contains("failed to decrypt"));
    let meta = s.secret_meta(NS, "llm").await.unwrap();
    assert_eq!(meta.keys, ["apiKey"]);
    let raw: Vec<u8> = sqlx::query_scalar("select ciphertext from secrets").fetch_one(s.pool()).await.unwrap();
    assert!(!String::from_utf8_lossy(&raw).contains(KEY), "stored encrypted");
}

#[test]
fn endpoint_admission() {
    let spec = |e: &str| json!({"provider": "openai-compatible", "endpoint": e, "secretRef": {"name": "s", "key": "k"}, "allowedModels": ["m"]});
    assert!(admit_connection(&spec("https://api.example.com/v1")).is_ok());
    assert!(admit_connection(&spec("http://localhost:11434/v1")).is_ok());
    assert_eq!(admit_connection(&spec("http://api.example.com/v1")).unwrap_err().field.as_deref(), Some("spec.endpoint"));
    let no_secret = json!({"provider": "anthropic", "endpoint": "https://api.anthropic.com", "allowedModels": ["claude-opus-5"]});
    assert_eq!(admit_connection(&no_secret).unwrap_err().field.as_deref(), Some("spec.secretRef"));
}
