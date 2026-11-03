# 0034 — Cron jobs, event triggers, and kinds without controllers

Status: accepted (2026-11-03)

**Context.** The type system names about sixty resource kinds from the PRD. Until now the API accepted all of them, but nothing acted on two thirds of them. Creating an `AgentPool` or a `SecurityPolicy` was stored and silently did nothing, which is worse than an error.

**Decision.**
- **`AgentCronJob {schedule, workflow, input, concurrencyPolicy, suspend}`** starts the workflow in each minute its 5-field cron schedule matches (UTC).
  - **Controller:** one replica runs it, under the `cron` leader lease, every 15 seconds.
  - **One run per minute:** each run's idempotency key is `cron/<job>/<minute>`, so replicas and restarts still start one run per due minute.
  - **Overlap:** `Forbid` (the default) skips a tick while the previous run is still going and records why in the job's status. `Allow` starts another run anyway.
  - **No backfill:** minutes missed while no controller ran are skipped.
- **`EventTrigger {topic, workflow, input}`** starts the workflow for each message published to the topic (ADR 0033).
  - **Input:** `{event, topic}` merged over `input`.
  - **Deduplication:** a republish with the same dedup key starts no second run.
  - **Audit:** the run is started as `eventtrigger/<ns>/<name> for <publisher>`.
- **Every other kind nothing acts on is refused at admission** with `UnsupportedCapability`, naming the kind. The list is `NOT_BUILT` in `clankrd`. A kind leaves the list when its controller lands.

**Consequences.** The API no longer accepts resources it can't honour, and the README's "not built yet" section and `NOT_BUILT` say the same thing.
