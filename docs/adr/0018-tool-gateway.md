# 0018 — Tool gateway

Status: accepted (2026-11-02)

Agents call tools the way they call models: through a claim, via their node, never holding the credential (PRD §28).

## Resources

- **`kind: Tool`:**
  - `version` (immutable per version)
  - `executor`:
    - `http`: POSTs `{method, input, idempotencyKey, runId, task}` to an https URL (or http on loopback). The secret is sent under a configurable header. No redirects.
    - `mock`: fixed responses, for tests and evaluations.
  - `methods`, each with an input schema (JSON Schema subset), `sideEffects` (`readOnly | idempotent | unsafe`, default unsafe) and `risk` (`low | high`)
  - `timeoutSeconds` and `maxOutputBytes`
- **`kind: ToolClaim`:**
  - `allow` and `deny` grants by tool, methods and versions; deny wins
  - `approvalRequired` (`tool.method`)
  - `maxCallsPerRun`
- **Agents:** an Agent lists its claims in `spec.tools`.

## Calls

`POST /v1/namespaces/{ns}/toolclaims/{claim}/call`. The SDK call is `ctx.tools.call(claim, tool, method, input)`. Checks run in this order:

1. **Binding:** the node checks the Agent is bound to the claim. The server re-checks against the workload identity, so a compromised node can't borrow another agent's tools.
2. **Grant:** deny wins, the method must be granted, and the version must be allowed (CL-28-02).
3. **Input:** validated against the method schema. Undeclared fields are refused before anything runs.
4. **Idempotency:** a key already seen returns the recorded result (`replayed: true`). A different call reusing the key is a Conflict, and a call still in flight is a Conflict.
   - The SDK's default key is `<task idempotency key>:<tool>.<method>:<n>`. It's the same across task attempts, so a task that crashes after a call gets the result on retry instead of repeating the effect (CL-28-06).
5. **Budget:** `maxCallsPerRun`.
6. **Approval:** high-risk methods, and methods listed in `approvalRequired`, spend a granted approval from the same run. By default that's the newest unused one; a specific one can be named. A unique index makes each approval good for exactly one call (CL-28-07).
7. **Execution:** the key and approval are claimed before executing, then the call runs within the timeout.
   - A failure deletes the row, so the same key can retry.
   - A timeout on an `unsafe` method is recorded as `unknown`, and later calls with that key get OutcomeUnknown rather than a second charge.
8. **Output** (CL-28-08): control characters are stripped from every string. Oversized output becomes a flagged preview.
9. **Record** (CL-28-09/10): `tool_calls` keeps the tool version, input and output hashes, the output, duration and outcome. High-risk and unsafe calls are also audited, with the approval used.

**Not yet:**
- shell, code and browser sandboxes (CL-28-04/05)
- MCP servers (CL-28-03)
- storing large outputs as artifacts
- egress policy for http executors beyond https-only
