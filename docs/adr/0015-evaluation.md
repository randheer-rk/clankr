# 0015 — Evaluation harness

Status: accepted (2026-11-02)

The harness has three parts (CL-34-08, OBS-06/07).

## 1. Golden execution fixtures

`crates/clankr-engine/tests/golden/*.yaml`

- **What a fixture holds:** a graph, the external inputs fed to a run (with clock advances), and the exact event history and final state the interpreter must produce.
- **What they catch:** any change in task ids, ordering, backoff timestamps or terminal states, before it can make real runs replay differently.
- **Coverage:**
  - fan-out with join and approval
  - retry with backoff, then giving up
  - rejected approval routing back
  - a bounded condition loop
  - timer and cancel
  - any-join absorbing a late branch
  - an unknown outcome resolved by an operator
- **Intended changes:** regenerate with `UPDATE_GOLDEN=1` and review the diff like code.

## 2. Deterministic model responses (golden traces)

- **Recording:** a ProviderConnection with `record: true` stores each successful request and response in `model_traces`, keyed by `promptSha256`. The hash covers the system prompt and messages, nothing else.
  - Recording is opt-in because, unlike the `model_calls` ledger, it stores prompt content.
  - `GET runs/{id}/traces` needs secret-level access.
- **Export:** `clank eval trace <run>` prints the recording as a replay `script:`.
- **Replay:** the `replay` provider answers only from its script. It matches the exact prompt hash first, then `contains` rules for hand-written cases.
- **Drift:** a prompt with no recording is an error rather than a made-up answer. That error is the signal that an agent's prompts drifted.

## 3. `clank eval run suite.yaml`

See `examples/eval/` for a sample suite.

- **Running:** each case starts the workflow with its input. When `autoApprove` is set, approvals are made as a separate `--approver-token` principal, because nobody can approve their own run.
- **Scoring:** the run's output (`state.data`) is checked with `equals`, `contains`, `notContains`, `gte`, `lte` and `exists` on `$.path` expressions. Each case also has an expected terminal status, so safety cases can expect `Failed`.
- **Recorded per case:** cost and tokens (from `runs/{id}/spend`) and duration.
- **The gate fails (exit 1) when:**
  - the absolute thresholds are missed: pass rate, average cost, p95 duration; or
  - against `--baseline`: any case that passed now fails, the pass rate drops more than allowed, or average cost rises more than allowed.
- **Reports:** `--report` writes the JSON report, which becomes the next baseline.

To gate a release candidate in CI: deploy the candidate image, point its ModelClaims at a replay connection, run the suite with `--baseline`, and promote only on exit 0.

**Not yet:**
- running cases in parallel
- LLM-as-judge or semantic scorers (this needs model-graded scoring with its own budget)
- an evaluation console in the web UI
- recording tool calls (no tool gateway yet)
