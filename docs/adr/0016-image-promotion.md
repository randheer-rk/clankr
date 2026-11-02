# 0016 — Image promotion by digest

Status: accepted (2026-11-02)

## Promotion

- **The resource:** `kind: AgentImage` names a repository and its ordered `stages` (default dev → staging → production). Each stage can have a gate. The only gate so far is `minEvalPassRate`.
- **Promote:** `POST agentimages/{name}/promotions {stage, digest, reason, evidence}` moves a `sha256:` digest into a stage. Tags are refused.
- **Ordering:** a digest enters stage N only if it was promoted to stage N-1 at some point. Nobody can push straight to production.
- **Gates:** a gated stage needs `evidence.eval.passRate` at or above its minimum. `clank image promote --eval-report report.json` attaches a `clank eval` report: its suite, pass rate, cost and the sha256 of the report file.
  - The evidence is the caller's own claim. It's recorded in the promotion history and the audit log, but not verified.
  - Signed attestations are SEC-19 work.
- **Who:** promoting and rolling back need the new `ImagesPromote` permission. Admin and cluster-admin have it; editors don't.
- **Rollback:** `POST …/rollback {stage}` re-promotes the newest earlier digest of that stage. It is recorded as a promotion with `rollback: true`.
- **History:** `image_promotions` (newest first via `GET …/promotions`), plus an audit record per change. The current digest of a stage is its newest row.

## Rollout

An Agent opts in with `track: {image, stage}`.

- **Admission:** a tracking Agent's `runtime.image` must equal `repository@<stage digest>`, so hand-edits can't drift from the stage.
- **On promote or rollback:** every tracking Agent in the namespace is rewritten in the same request, as the promoter.
- **Scheduler change:** the scheduler now records the Agent generation each instance started from (`instances.agent_generation`). When the Agent changes, it bumps the instance's epoch in place. The node restarts the instance on the new spec, and the old process is fenced like any other move.
  - This also fixes a gap that existed before promotions: editing an Agent never rolled out to running instances.
  - After upgrading, every existing instance restarts once (their generation starts at 0).

**Rolling restarts** (added 2026-11-02):
- **Readiness:** nodes report which instances finished the agent handshake. A report only counts for the epoch it names.
- **Pacing:** an Agent change restarts at most `spec.rollout.maxUnavailable` instances (default 1) of each deployment at once. The next restart waits until a restarted instance has been ready for `minReadySeconds` (default 5).
- **Bad releases:** a release that never becomes ready stalls after the first instance, while the rest keep serving. The deployment shows `phase: RollingOut` with progress such as `1/3 on generation 3`. Fixing or rolling back the Agent resumes the rollout.
- **Not throttled:** restarts forced by node loss or drain are never held back.
- **Tested:** `crates/clankr-node/tests/rollout_e2e.rs`.

**Not yet:**
- canary stages with automatic rollback on error rate
- signature and SBOM verification at admission (ImagePolicy, SEC-07/19)
- registry-side promotion (copying or tagging in the registry)
