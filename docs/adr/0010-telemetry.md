# 0010 — Tracing with OpenTelemetry

Status: accepted (2026-11-01)

- **Spans:**
  - `http` (named `METHOD /route`, with request_id, actor, status)
  - `engine.step` (run_id, input type, event count, resulting status)
  - `task.lease` / `task.finish` (run, task, epoch)
  - `model.generate` (provider, model, input/output tokens, outcome, run/task attribution)
  - `scheduler.reconcile` (leader epoch)
- **Trace continuation:** an incoming W3C `traceparent` is continued, so a caller's trace spans clankrd.
- **Export:** set `OTEL_EXPORTER_OTLP_ENDPOINT` (plus the standard `OTEL_EXPORTER_OTLP_*` / `OTEL_SERVICE_NAME` variables) to export OTLP/HTTP protobuf through a batch processor.
- **Filtering:** `RUST_LOG` governs log output only. Exported spans have their own filter (clankr crates at info), so quiet logs don't silence traces; that was a real bug the end-to-end check caught.
- **Privacy:** prompts, payloads and secrets never become span attributes (CL-34-05). The test asserts that prompt text is absent.
- **Not the record:** telemetry is separate from the durable event history and the audit log; dropped spans never affect correctness (§34).

**Not yet:** passing trace context into agents (the Deliver frame), metrics, and log export over OTLP.
