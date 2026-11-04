//! The newer provider adapters, Bedrock (SigV4), Gemini, Vertex (service-account OAuth) and
//! Azure OpenAI, and the embeddings op, against fake endpoints that record what they were sent.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::extract::Path;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use clankr_gateway::{EmbedRequest, Gateway, GenerateRequest, Message};
use clankr_store::Store;
use clankr_store::embedded::DevPostgres;
use clankr_store::secrets::MasterKey;
use clankr_types::{ErrorCode, RequestCtx, Resource};
use serde_json::{Value, json};

const NS: &str = "lab";

fn ctx() -> RequestCtx {
    RequestCtx { actor: "owner".into(), request_id: "p".into() }
}

fn res(kind: &str, name: &str, spec: Value) -> Resource {
    serde_json::from_value(json!({"apiVersion": "clankr.io/v1alpha1", "kind": kind, "metadata": {"name": name, "namespace": NS}, "spec": spec})).unwrap()
}

/// Every request a fake endpoint saw: (path, headers, body).
type Seen = Arc<Mutex<Vec<(String, Vec<(String, String)>, Value)>>>;

/// A fake that answers every path with the reply chosen by `pick(path)`.
async fn fake(pick: impl Fn(&str) -> Value + Clone + Send + Sync + 'static) -> (String, Seen) {
    let seen: Seen = Default::default();
    let s = seen.clone();
    let app = axum::Router::new().route(
        "/{*path}",
        post(move |Path(path): Path<String>, h: HeaderMap, body: String| {
            let (s, pick) = (s.clone(), pick.clone());
            async move {
                let body: Value = serde_json::from_str(&body).unwrap_or(Value::String(body));
                s.lock().unwrap().push((format!("/{path}"), h.iter().map(|(k, v)| (k.to_string(), v.to_str().unwrap().to_owned())).collect(), body));
                (StatusCode::OK, Json(pick(&path)))
            }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (url, seen)
}

fn header<'a>(h: &'a [(String, String)], name: &str) -> Option<&'a str> {
    h.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
}

async fn setup() -> (DevPostgres, Store, Gateway) {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let pg = DevPostgres::start(None).await.unwrap();
    let store = Store::connect(&pg.database("clankr").await.unwrap()).await.unwrap();
    let gw = Gateway::new(store.clone(), MasterKey::generate());
    (pg, store, gw)
}

fn ask(text: &str) -> GenerateRequest {
    GenerateRequest { messages: vec![Message { role: "user".into(), content: text.into() }], system: Some("be brief".into()), max_output_tokens: Some(50), run_id: None, task: None }
}

#[tokio::test]
async fn bedrock_signs_with_sigv4_and_uses_converse() {
    let (_pg, s, _) = setup().await;
    let master = MasterKey::generate();
    let gw = Gateway::new(s.clone(), master.clone());
    s.secret_put(&ctx(), &master, NS, "aws", &BTreeMap::from([("creds".into(), "AKIDEXAMPLE:wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY:sess".into())])).await.unwrap();
    let (url, seen) = fake(|p| if p.ends_with("/converse") {
        json!({"output": {"message": {"role": "assistant", "content": [{"text": "Paris"}]}}, "stopReason": "end_turn", "usage": {"inputTokens": 9, "outputTokens": 2}})
    } else {
        json!({"embedding": [0.1, 0.2], "inputTextTokenCount": 3})
    })
    .await;
    s.create(&ctx(), res("ProviderConnection", "br", json!({"provider": "bedrock", "region": "eu-west-1", "endpoint": url, "secretRef": {"name": "aws", "key": "creds"}, "allowedModels": ["anthropic.claude-3-haiku-20240307-v1:0", "amazon.titan-embed-text-v2:0"]}))).await.unwrap();
    s.create(&ctx(), res("ModelClaim", "br", json!({"providerConnection": "br", "model": "anthropic.claude-3-haiku-20240307-v1:0"}))).await.unwrap();
    s.create(&ctx(), res("ModelClaim", "br-embed", json!({"providerConnection": "br", "model": "amazon.titan-embed-text-v2:0"}))).await.unwrap();

    let r = gw.generate(&ctx(), NS, "br", ask("capital of France?")).await.unwrap();
    assert_eq!((r.text.as_str(), r.usage.input_tokens, r.usage.output_tokens, r.stop_reason.as_deref()), ("Paris", 9, 2, Some("end_turn")));
    let (path, h, body) = seen.lock().unwrap()[0].clone();
    assert_eq!(path, "/model/anthropic.claude-3-haiku-20240307-v1:0/converse", "the model id is one path segment (decoded by the server)");
    let auth = header(&h, "authorization").unwrap();
    assert!(auth.starts_with("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/") && auth.contains("/eu-west-1/bedrock/aws4_request, SignedHeaders=content-type;host;x-amz-content-sha256;x-amz-date;x-amz-security-token, Signature="), "{auth}");
    assert_eq!(header(&h, "x-amz-security-token"), Some("sess"));
    assert!(header(&h, "x-amz-date").unwrap().ends_with('Z'));
    assert_eq!(body["system"], json!([{"text": "be brief"}]));
    assert_eq!(body["messages"], json!([{"role": "user", "content": [{"text": "capital of France?"}]}]));
    assert_eq!(body["inferenceConfig"]["maxTokens"], 50);
    assert!(!format!("{h:?}").contains("wJalrXUtnFEMI"), "the secret key never travels");

    let e = gw.embed(&ctx(), NS, "br-embed", EmbedRequest { inputs: vec!["a".into(), "b".into()], run_id: None, task: None }).await.unwrap();
    assert_eq!(e.embeddings, vec![vec![0.1f32, 0.2], vec![0.1, 0.2]]);
    assert_eq!(e.usage.input_tokens, 6, "Titan reports its own token counts");
    assert!(seen.lock().unwrap().iter().filter(|(p, _, _)| p.ends_with("/invoke")).count() == 2);

    // a secret that isn't KEY:SECRET is refused before any request goes out
    s.secret_put(&ctx(), &master, NS, "aws", &BTreeMap::from([("creds".into(), "nope".into())])).await.unwrap();
    assert_eq!(gw.generate(&ctx(), NS, "br", ask("x")).await.unwrap_err().code, ErrorCode::InvalidSpec);
}

#[tokio::test]
async fn gemini_and_vertex_share_the_body_and_differ_in_auth() {
    let (_pg, s, _) = setup().await;
    let master = MasterKey::generate();
    let gw = Gateway::new(s.clone(), master.clone());
    let reply = json!({"candidates": [{"content": {"role": "model", "parts": [{"text": "Par"}, {"text": "is"}]}, "finishReason": "STOP"}], "usageMetadata": {"promptTokenCount": 7, "candidatesTokenCount": 2}});
    let (url, seen) = {
        let reply = reply.clone();
        fake(move |p| match p {
            "token" => json!({"access_token": "ya29.minted", "expires_in": 3599, "token_type": "Bearer"}),
            p if p.ends_with(":batchEmbedContents") => json!({"embeddings": [{"values": [1.0, 2.0]}, {"values": [3.0, 4.0]}]}),
            _ => reply.clone(),
        })
        .await
    };
    s.secret_put(&ctx(), &master, NS, "google", &BTreeMap::from([("apiKey".into(), "AIza-SECRET".into())])).await.unwrap();
    s.create(&ctx(), res("ProviderConnection", "gem", json!({"provider": "gemini", "endpoint": url, "secretRef": {"name": "google", "key": "apiKey"}, "allowedModels": ["gemini-2.5-flash", "text-embedding-004"]}))).await.unwrap();
    s.create(&ctx(), res("ModelClaim", "gem", json!({"providerConnection": "gem", "model": "gemini-2.5-flash"}))).await.unwrap();
    s.create(&ctx(), res("ModelClaim", "gem-embed", json!({"providerConnection": "gem", "model": "text-embedding-004"}))).await.unwrap();

    let mut req = ask("capital of France?");
    req.messages.push(Message { role: "assistant".into(), content: "Hmm".into() });
    req.messages.push(Message { role: "user".into(), content: "well?".into() });
    let r = gw.generate(&ctx(), NS, "gem", req).await.unwrap();
    assert_eq!((r.text.as_str(), r.usage.input_tokens, r.stop_reason.as_deref()), ("Paris", 7, Some("stop")));
    let (path, h, body) = seen.lock().unwrap()[0].clone();
    assert_eq!(path, "/v1beta/models/gemini-2.5-flash:generateContent");
    assert_eq!(header(&h, "x-goog-api-key"), Some("AIza-SECRET"));
    assert_eq!(body["systemInstruction"]["parts"][0]["text"], "be brief");
    assert_eq!(body["contents"][1], json!({"role": "model", "parts": [{"text": "Hmm"}]}), "assistant turns are role model");
    assert_eq!(body["generationConfig"]["maxOutputTokens"], 50);
    let e = gw.embed(&ctx(), NS, "gem-embed", EmbedRequest { inputs: vec!["x".into(), "y".into()], run_id: None, task: None }).await.unwrap();
    assert_eq!(e.embeddings, vec![vec![1.0f32, 2.0], vec![3.0, 4.0]]);

    // Vertex: a service-account JSON is exchanged for an access token, once, then reused
    let sa = json!({"type": "service_account", "client_email": "bot@proj.iam.gserviceaccount.com", "private_key": include_str!("fixtures/test-sa.pem"), "token_uri": format!("{url}/token")});
    s.secret_put(&ctx(), &master, NS, "sa", &BTreeMap::from([("json".into(), sa.to_string())])).await.unwrap();
    s.create(&ctx(), res("ProviderConnection", "vx", json!({"provider": "vertex", "project": "proj", "region": "europe-west1", "endpoint": url, "secretRef": {"name": "sa", "key": "json"}, "allowedModels": ["gemini-2.5-pro"]}))).await.unwrap();
    s.create(&ctx(), res("ModelClaim", "vx", json!({"providerConnection": "vx", "model": "gemini-2.5-pro"}))).await.unwrap();
    for _ in 0..2 {
        let r = gw.generate(&ctx(), NS, "vx", ask("hi")).await.unwrap();
        assert_eq!(r.text, "Paris");
    }
    let calls = seen.lock().unwrap().clone();
    let tokens: Vec<_> = calls.iter().filter(|(p, _, _)| p == "/token").collect();
    assert_eq!(tokens.len(), 1, "the access token is cached");
    let form = tokens[0].2.as_str().unwrap();
    assert!(form.starts_with("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer&assertion=ey"), "{form}");
    let jwt = form.split("assertion=").nth(1).unwrap();
    let claims: Value = serde_json::from_slice(&base64_url(jwt.split('.').nth(1).unwrap())).unwrap();
    assert_eq!(claims["iss"], "bot@proj.iam.gserviceaccount.com");
    assert_eq!(claims["scope"], "https://www.googleapis.com/auth/cloud-platform");
    let (path, h, _) = calls.iter().find(|(p, _, _)| p.contains("/publishers/google/")).unwrap().clone();
    assert_eq!(path, "/v1/projects/proj/locations/europe-west1/publishers/google/models/gemini-2.5-pro:generateContent");
    assert_eq!(header(&h, "authorization"), Some("Bearer ya29.minted"));

    // admission: vertex without a project, bedrock without a region
    assert_eq!(clankr_gateway::admit_connection(&json!({"provider": "vertex", "region": "x", "secretRef": {"name": "a", "key": "b"}, "allowedModels": ["m"]})).unwrap_err().field.as_deref(), Some("spec.project"));
    assert_eq!(clankr_gateway::admit_connection(&json!({"provider": "bedrock", "secretRef": {"name": "a", "key": "b"}, "allowedModels": ["m"]})).unwrap_err().field.as_deref(), Some("spec.region"));
}

fn base64_url(s: &str) -> Vec<u8> {
    let mut v: Vec<u8> = s.bytes().map(|b| match b { b'-' => b'+', b'_' => b'/', x => x }).collect();
    while v.len() % 4 != 0 {
        v.push(b'=');
    }
    let table = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = vec![];
    for chunk in v.chunks(4) {
        let n: u32 = chunk.iter().fold(0, |acc, c| (acc << 6) | if *c == b'=' { 0 } else { table.iter().position(|t| t == c).unwrap() as u32 });
        let pad = chunk.iter().filter(|c| **c == b'=').count();
        out.extend_from_slice(&n.to_be_bytes()[1..4 - pad]);
    }
    out
}

#[tokio::test]
async fn azure_openai_and_embeddings_budgeting() {
    let (_pg, s, _) = setup().await;
    let master = MasterKey::generate();
    let gw = Gateway::new(s.clone(), master.clone());
    s.secret_put(&ctx(), &master, NS, "az", &BTreeMap::from([("key".into(), "az-SECRET".into())])).await.unwrap();
    let (url, seen) = fake(|p| if p.ends_with("/embeddings") {
        json!({"data": [{"embedding": [0.5, 0.5, 0.5]}], "usage": {"prompt_tokens": 4}})
    } else {
        json!({"choices": [{"message": {"content": "hi"}, "finish_reason": "stop"}], "usage": {"prompt_tokens": 3, "completion_tokens": 1}})
    })
    .await;
    s.create(&ctx(), res("ProviderConnection", "az", json!({"provider": "azure-openai", "endpoint": url, "secretRef": {"name": "az", "key": "key"}, "allowedModels": ["gpt-5-deploy", "embed-deploy"]}))).await.unwrap();
    s.create(&ctx(), res("ModelClaim", "az", json!({"providerConnection": "az", "model": "gpt-5-deploy"}))).await.unwrap();
    s.create(&ctx(), res("ModelClaim", "az-embed", json!({"providerConnection": "az", "model": "embed-deploy", "budget": {"maxTokensTotal": 10}}))).await.unwrap();
    let r = gw.generate(&ctx(), NS, "az", ask("hello")).await.unwrap();
    assert_eq!(r.text, "hi");
    let (path, h, _) = seen.lock().unwrap()[0].clone();
    assert_eq!(path, "/openai/deployments/gpt-5-deploy/chat/completions", "{path}");
    assert_eq!(header(&h, "api-key"), Some("az-SECRET"));

    // embeddings reserve by estimate, settle to the provider's count, and hit the budget
    let e = gw.embed(&ctx(), NS, "az-embed", EmbedRequest { inputs: vec!["hello world".into()], run_id: None, task: None }).await.unwrap();
    assert_eq!((e.embeddings.len(), e.usage.input_tokens, e.model.as_str()), (1, 4, "embed-deploy"));
    let (path, _, body) = seen.lock().unwrap()[1].clone();
    assert_eq!(path, "/openai/deployments/embed-deploy/embeddings");
    assert_eq!(body["input"], json!(["hello world"]));
    let (used, reserved): (i64, i64) = sqlx::query_as("select used, reserved from model_usage where claim = 'az-embed'").fetch_one(s.pool()).await.unwrap();
    assert_eq!((used, reserved), (4, 0));
    let big = EmbedRequest { inputs: vec!["x".repeat(100)], run_id: None, task: None };
    assert_eq!(gw.embed(&ctx(), NS, "az-embed", big).await.unwrap_err().code, ErrorCode::QuotaExceeded);
    let row: (String, i64, String) = sqlx::query_as("select stop_reason, input_tokens, outcome from model_calls where claim = 'az-embed'").fetch_one(s.pool()).await.unwrap();
    assert_eq!(row, ("embeddings".into(), 4, "ok".into()), "embeddings are in the same call record");

    // mock embeddings are deterministic; anthropic has none
    s.create(&ctx(), res("ProviderConnection", "mock", json!({"provider": "mock", "allowedModels": ["m"]}))).await.unwrap();
    s.create(&ctx(), res("ModelClaim", "mock", json!({"providerConnection": "mock", "model": "m"}))).await.unwrap();
    let a = gw.embed(&ctx(), NS, "mock", EmbedRequest { inputs: vec!["same".into(), "same".into(), "other".into()], run_id: None, task: None }).await.unwrap().embeddings;
    assert!(a[0] == a[1] && a[0] != a[2] && a[0].len() == 8);
    s.create(&ctx(), res("ProviderConnection", "anth", json!({"provider": "anthropic", "endpoint": url, "secretRef": {"name": "az", "key": "key"}, "allowedModels": ["c"]}))).await.unwrap();
    s.create(&ctx(), res("ModelClaim", "anth", json!({"providerConnection": "anth", "model": "c"}))).await.unwrap();
    assert_eq!(gw.embed(&ctx(), NS, "anth", EmbedRequest { inputs: vec!["x".into()], run_id: None, task: None }).await.unwrap_err().code, ErrorCode::UnsupportedCapability);
}
