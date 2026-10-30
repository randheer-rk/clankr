//! Traces: a caller's traceparent is continued, and engine/model spans join the same trace with
//! run and usage attributes (OBS-01, CL-34-04).

use axum::body::Body;
use axum::http::Request;
use clankr_store::Store;
use clankr_store::embedded::DevPostgres;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SpanData};
use serde_json::{Value, json};
use tower::ServiceExt;
use tracing_subscriber::layer::SubscriberExt;

const TRACE: &str = "4bf92f3577b34da6a3ce929d0e0e4736";

fn attr(s: &SpanData, key: &str) -> Option<String> {
    s.attributes.iter().find(|kv| kv.key.as_str() == key).map(|kv| kv.value.to_string())
}

#[tokio::test(flavor = "current_thread")]
async fn spans_continue_the_callers_trace_and_carry_run_context() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder().with_simple_exporter(exporter.clone()).build();
    let layer = tracing_opentelemetry::layer().with_tracer(provider.tracer("test"));
    let _guard = tracing::subscriber::set_default(tracing_subscriber::registry().with(layer));

    let pg = DevPostgres::start(None).await.unwrap();
    let store = Store::connect(&pg.database("clankr").await.unwrap()).await.unwrap();
    let state = clankrd::AppState::new(store, "tok".into(), clankr_store::secrets::MasterKey::generate()).await.unwrap();
    let app = clankrd::router(state);
    let call = |method: &str, path: &str, body: Value| {
        Request::builder()
            .method(method)
            .uri(path)
            .header("authorization", "Bearer tok")
            .header("content-type", "application/json")
            .header("traceparent", format!("00-{TRACE}-00f067aa0ba902b7-01"))
            .body(Body::from(body.to_string()))
            .unwrap()
    };
    let res = |kind: &str, name: &str, spec: Value| json!({"apiVersion": "clankr.io/v1alpha1", "kind": kind, "metadata": {"name": name, "namespace": "t"}, "spec": spec});
    for (plural, body) in [
        ("providerconnections", res("ProviderConnection", "mock", json!({"provider": "mock", "allowedModels": ["m"]}))),
        ("modelclaims", res("ModelClaim", "default", json!({"providerConnection": "mock", "model": "m"}))),
        ("workflows", res("Workflow", "wf", json!({"entrypoint": "a", "nodes": {"a": {"type": "agent", "agentRef": "x"}}}))),
    ] {
        let r = app.clone().oneshot(call("POST", &format!("/v1/namespaces/t/{plural}"), body)).await.unwrap();
        assert_eq!(r.status(), 201);
    }
    let r = app.clone().oneshot(call("POST", "/v1/namespaces/t/workflows/wf/runs", json!({"input": 1}))).await.unwrap();
    assert_eq!(r.status(), 201);
    let r = app.clone().oneshot(call("POST", "/v1/namespaces/t/models/default/generate", json!({"messages": [{"role": "user", "content": "hi"}]}))).await.unwrap();
    assert_eq!(r.status(), 200);
    let _ = provider.force_flush();

    // only clankrd's spans; the embedded Postgres used by the test emits its own setup spans
    let spans: Vec<SpanData> = exporter
        .get_finished_spans()
        .unwrap()
        .into_iter()
        .filter(|s| s.name.starts_with("POST ") || s.name.starts_with("engine.") || s.name.starts_with("model.") || s.name.starts_with("task."))
        .collect();
    let names: Vec<&str> = spans.iter().map(|s| s.name.as_ref()).collect();
    assert!(spans.iter().all(|s| s.span_context.trace_id().to_string() == TRACE), "every span joins the caller's trace: {names:?}");

    let http = spans.iter().find(|s| s.name == "POST /v1/namespaces/{ns}/workflows/{name}/runs").expect("http span named by route");
    assert_eq!(attr(http, "http.status").as_deref(), Some("201"));
    assert_eq!(attr(http, "actor").as_deref(), Some("owner"));
    assert!(attr(http, "request_id").is_some());

    let step = spans.iter().find(|s| s.name == "engine.step").expect("engine.step span");
    assert_eq!(attr(step, "input").as_deref(), Some("RunStarted"));
    assert!(attr(step, "run_id").is_some_and(|r| r.len() == 36));
    assert_eq!(step.parent_span_id, http.span_context.span_id(), "engine step is a child of its request");

    let model = spans.iter().find(|s| s.name == "model.generate").expect("model.generate span");
    assert_eq!(attr(model, "provider").as_deref(), Some("mock"));
    assert_eq!(attr(model, "outcome").as_deref(), Some("ok"));
    assert!(attr(model, "output_tokens").is_some_and(|t| t.parse::<i64>().unwrap() > 0));
    // prompts never land in span attributes (CL-34-05)
    assert!(!spans.iter().flat_map(|s| s.attributes.iter()).any(|kv| kv.value.to_string().contains("hi")));
}
