//! Tracing (OBS-01, CL-34-04): every request, engine step, task lease/complete, model call and
//! scheduler pass is a span carrying run/task/agent/request identifiers. Spans go to stdout logs
//! and, when OTEL_EXPORTER_OTLP_ENDPOINT is set, to an OpenTelemetry collector over OTLP/HTTP.
//! Telemetry is observability only: dropped spans never affect workflow correctness (§34).

use axum::extract::{MatchedPath, Request};
use axum::middleware::Next;
use axum::response::Response;
use opentelemetry::propagation::TextMapPropagator;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing::Instrument;
use tracing_opentelemetry::OpenTelemetrySpanExt;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// Installs the global subscriber. Returns the provider so main can flush it on shutdown.
pub fn init() -> anyhow::Result<Option<SdkTracerProvider>> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    let provider = match std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT") {
        Ok(_) => {
            // endpoint, headers, timeout come from the standard OTEL_EXPORTER_OTLP_* env vars
            let exporter = opentelemetry_otlp::SpanExporter::builder().with_http().build()?;
            Some(provider_with(exporter))
        }
        Err(_) => None,
    };
    // RUST_LOG governs log output only; exported spans are clankr's own at info, regardless of
    // how quiet the logs are (and without library noise)
    let spans = tracing_subscriber::filter::Targets::new()
        .with_target("clankrd", tracing::Level::INFO)
        .with_target("clankr_engine", tracing::Level::INFO)
        .with_target("clankr_gateway", tracing::Level::INFO);
    let otel = provider.as_ref().map(|p| tracing_opentelemetry::layer().with_tracer(p.tracer("clankrd")).with_filter(spans));
    tracing_subscriber::registry().with(tracing_subscriber::fmt::layer().with_filter(filter)).with(otel).try_init()?;
    Ok(provider)
}

/// Batch provider with service.name = clankrd (unless OTEL_SERVICE_NAME overrides).
pub fn provider_with<E: opentelemetry_sdk::trace::SpanExporter + 'static>(exporter: E) -> SdkTracerProvider {
    let resource = opentelemetry_sdk::Resource::builder().with_service_name(std::env::var("OTEL_SERVICE_NAME").unwrap_or_else(|_| "clankrd".into())).build();
    SdkTracerProvider::builder().with_resource(resource).with_batch_exporter(exporter).build()
}

struct Headers<'a>(&'a axum::http::HeaderMap);

impl opentelemetry::propagation::Extractor for Headers<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|v| v.to_str().ok())
    }
    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(|k| k.as_str()).collect()
    }
}

/// One span per HTTP request, continuing the caller's W3C `traceparent` if present.
pub async fn http_span(req: Request, next: Next) -> Response {
    let route = req.extensions().get::<MatchedPath>().map_or_else(|| req.uri().path().to_owned(), |p| p.as_str().to_owned());
    let span = tracing::info_span!(
        "http",
        otel.name = %format!("{} {route}", req.method()),
        http.method = %req.method(),
        http.route = %route,
        request_id = tracing::field::Empty,
        actor = tracing::field::Empty,
        http.status = tracing::field::Empty,
    );
    let parent = TraceContextPropagator::new().extract(&Headers(req.headers()));
    let _ = span.set_parent(parent);
    let resp = next.run(req).instrument(span.clone()).await;
    span.record("http.status", resp.status().as_u16());
    if let Some(id) = resp.headers().get("x-request-id").and_then(|v| v.to_str().ok()) {
        span.record("request_id", id);
    }
    resp
}
