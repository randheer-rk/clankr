# 0020 — Canary releases with automatic rollback

Status: accepted (2026-11-02)

**Decision.** `AgentDeployment.spec.rollout.canary` (`replicas`, `analysisSeconds`, `progressDeadlineSeconds`, `maxFailureRatio`, `minTasks`) makes a new Agent generation run on `replicas` instances first. The rest of the deployment stays on the stable generation, which is whatever most instances ran when the canary started. A `rollouts` row per (deployment, generation) holds the phase: `Canary` → `Promoted` | `Failed`.

- **Promote:** every canary has been ready for `minReadySeconds + analysisSeconds`, and they've finished at least `minTasks` tasks. After that it's an ordinary rolling update.
- **Roll back:** the canaries miss `progressDeadlineSeconds`, or more than `maxFailureRatio` of the tasks they finished failed. Attribution goes through the lease owner `agent/<agent>/<instance>`. Every instance goes back to the stable generation, and that generation stays rolled back. Editing the Agent again starts a new canary.

**Instances record their spec.** `instances.agent_spec` is the exact Agent spec an instance runs. The node launches from it instead of fetching the Agent's current spec. That's what lets old instances keep the old release. Instances created before this change have no recorded spec. If a rollback would need one of those, the deployment freezes instead of guessing, and status says why.

**Status.** `status.canary = {generation, stableGeneration, phase, reason}`.

**Not done.** There's no traffic weighting: work is pulled from a shared queue, so canaries get roughly their share of tasks. Model-quality signals (eval scores on canary traces) aren't used yet. They would plug into the same verdict.
