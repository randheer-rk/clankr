# 0019 — Queue-driven autoscaling

Status: accepted (2026-11-02)

## How it works

- **Settings:** `AgentDeployment.spec.autoscale: {minReplicas, maxReplicas, targetTasksPerReplica, scaleDownDelaySeconds}` replaces `replicas` when present.
- **When it runs:** each pass of the scheduler leader counts the agent's queue (`agent/<agent>` tasks that are ready or leased).
- **The rule:** `need = clamp(ceil(depth / target), min, max)`.
  - **Up:** at once.
  - **Down:** only after demand has stayed below the current count for `scaleDownDelaySeconds` (default 300). A burst in between resets the clock.
- **State:** the count and the "below since" time live in `autoscale_state`. Only the fenced leader writes it, so concurrent replicas can't fight. The deployment status shows `autoscale: {replicas, queueDepth}`.
- **Scale to zero:** `minReplicas: 0` is allowed. Tasks wait in the durable queue until an instance is placed. That's a cold start, not lost work.
- **Interactions:**
  - Scale-down removes instances through the scheduler's normal path. Their in-flight leases expire and the tasks are redelivered.
  - Scaling changes the replica count, not the Agent, so it never triggers a rollout.

**Not yet:**
- signals beyond queue depth: latency, token throughput, budget headroom (CL-23-09)
- scale-down that prefers idle instances
- per-step rate limits on scale-up
