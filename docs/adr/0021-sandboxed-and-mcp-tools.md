# 0021 — Sandboxed tools and MCP servers as tools

Status: accepted (2026-11-02)

**Decision.** The tool gateway gains three executors. Every call through them still gets all the existing rules: grants, schemas, approvals, idempotent replay, budgets, bounded output and audit.

- `sandbox { container }`: a fresh kennel container per call. It has no network (its own netns with loopback only), a read-only root, a writable /tmp, and memory, CPU and pids caps. The request `{method, input, idempotencyKey, runId, task}` arrives as JSON on stdin. Stdout is the result (JSON when it parses). A non-zero exit fails the call with the tail of stderr. Shell and code runners are images plus a command, for example `python3 -c …` reading stdin.
- `mcpStdio { container }`: a stdio MCP server started in the same sandbox for each call. The gateway runs `initialize`, `notifications/initialized`, then `tools/call`, and lines that aren't the awaited response are skipped.
- `mcpHttp { url, secretRef, header }`: MCP over streamable HTTP. It keeps `Mcp-Session-Id`, accepts JSON or SSE responses, and ends the session afterwards. The credential is brokered, as with `http`.

A Tool method name is the MCP tool name. Output is `structuredContent`, else the single text block (parsed as JSON when possible), else the content blocks. `isError` and JSON-RPC errors fail the call.

**Where it runs.** Containers run on the control plane host, with kennel enabled by `clankrd --tool-kennel [--tool-kennel-root]`. Without it, these tools fail with a clear error. Images must be pinned by digest. The namespace's ImagePolicies apply on every call, and signatures are verified by kennel with `--verify-key`, the same as agent launches. A timeout drops the call, and `kill_on_drop` takes the container down with it.

**Known gaps.**
- Running tools on worker nodes instead of the control plane is future work (it needs node-side dispatch).
- MCP discovery (`tools/list` → Tool methods) is manual. Methods are declared explicitly, because discovery isn't permission.
- kennel's /sys shows host interface names. It's read-only and the network stays isolated, but kennel should mask it.
