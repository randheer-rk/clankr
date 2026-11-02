# 0028 — Container tools run on the caller's node

Status: accepted (2026-11-03)

**Problem.** `sandbox` and `mcpStdio` tools (ADR 0021) ran as kennel containers on the control plane host. That's the wrong place for untrusted code, and it puts tool load on the control plane.

**Decision.** Calls from a node are two-phase:

1. The node calls `POST …/toolclaims/{claim}/call` with `x-clankr-executor: node`.
   - The gateway does every check, as for a local call: grant, schema, approval, per-run budget, and the idempotency claim (a replay returns the record).
   - For a container tool it then answers 202 with `{"delegate": {callId, timeoutSeconds, work}}` instead of running it, and records `delegated_to`, `delegated_at`, the timeout and whether the method is unsafe.
2. The node runs the work with its own kennel (`clankr-sandbox`, shared with the gateway), under the namespace's ImagePolicies.
3. The node reports through `POST …/toolcalls/{id}/complete {output | error, durationMs}`.
   - Only the node it was handed to may report, and only once.
   - The gateway finalizes the call through the same path as local execution: bounding, sanitizing, digest, audit, and on failure the key is freed.
4. Late reports and missing ones are settled like timeouts.
   - A report after the timeout counts as a timeout, so an unsafe method becomes OutcomeUnknown.
   - A 10 s sweep settles calls whose node never reported: unsafe ones become `unknown`, the rest are freed for a retry.

**Trust.** A reported result becomes the recorded outcome, so delegation is offered only to callers with a node's workload identity, or to a standalone node running with the owner token. Users calling tools directly always get control-plane execution. A node that can't run containers (no kennel installed) doesn't send the header.

**Unchanged.** MCP over HTTP and plain `http` tools still run in the gateway, where credentials are brokered. `clankrd --tool-kennel` remains for calls that don't come from a node.
