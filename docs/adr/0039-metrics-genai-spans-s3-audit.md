# 0039 — Prometheus metrics, GenAI span conventions, S3 audit export

Status: accepted (2026-11-04)

**Context.** Operators had traces (OTLP) and an audit log, but nothing to put on a dashboard or alert on, and the audit copy could only go to a file or an HTTP receiver.

**Decision.**

- **`GET /metrics`** answers Prometheus text, computed from the tables on each scrape (no in-process counters, so every replica reports the same truth): runs by status, tasks by state (ready = queue depth), approvals by state, instances per agent, nodes by readiness, queue messages by state (dead letters included), model calls, tokens and micro-USD spend per claim, budget counters per bucket and window (`model_usage`), tool calls per tool and state, who holds each leader lease, the newest audit seq and the export lag per destination. It needs the audit-read permission: a cluster-wide view, scraped with a ServiceAccount token.
- **GenAI span conventions.** `model.generate` and `model.embed` spans are named `chat {model}` / `embeddings {model}` and carry `gen_ai.operation.name`, `gen_ai.provider.name`, `gen_ai.request.model`, `gen_ai.request.max_tokens`, `gen_ai.usage.input_tokens`, `gen_ai.usage.output_tokens`, `gen_ai.response.finish_reasons` and `error.type`; tool calls are `execute_tool {tool}` with `gen_ai.tool.name` and `gen_ai.tool.call.id`. The older attributes (`provider`, `model`, `input_tokens`, `outcome`) stay. Prompts and outputs still never land in attributes (CL-34-05). The conventions are still marked Development upstream; the names follow the 2026 drafts that Langfuse, Grafana and Datadog read.
- **S3 audit export.** `--audit-export s3://bucket/prefix` writes each batch as one object, `prefix/audit-<first seq>-<last seq>.jsonl`, SigV4-signed with the usual `AWS_*` environment, in `AWS_REGION`; `CLANKR_AUDIT_S3_ENDPOINT` points at MinIO or any S3-compatible store (path-style). `clankrd audit-check s3://bucket/prefix` lists and reads the objects back and compares them with the database like a file export. Object Lock on the bucket makes the copy WORM, which is the point of an off-site copy.

**Consequences.**
- A scrape costs a dozen aggregate queries; at large table sizes, scrape every 30–60 s rather than every 5.
- Not yet: histograms (latency is in traces, not here), per-node metrics pushed from nodes, GCS/Azure Blob export (the same shape with a different signer), and alert delivery beyond the budget webhook.
