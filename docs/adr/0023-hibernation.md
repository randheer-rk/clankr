# 0023 — Agent hibernation

Status: accepted (2026-11-02)

**Decision.** `AgentDeployment.spec.hibernate: true` (or `clank hibernate <deployment>`) suspends a deployment at safe points:

1. The scheduler marks each instance `hibernate`. The node stops leasing for it and lets the in-flight task finish. It then sends the agent `Hibernate` (ABI 1.1).
2. The agent answers `Checkpoint {state}`. In Python, that's the value of the `@app.on_hibernate` hook (JSON, ≤256 KiB). The node keeps brokering the agent's calls meanwhile, so the hook can still write context.
3. The node hands the checkpoint in under the instance's own workload identity (`POST …/instances/{id}/checkpoint`, current epoch only). It's stored per **slot**: a stable index of the instance within its deployment. Each hibernation bumps the slot's version.
4. The scheduler deletes the instance, which releases its CPU and memory. Volume bindings stay, so durable claims are retained and local data stays on its node.

Setting `hibernate` back to false resumes. Each slot's next instance receives its checkpoint once, in `Welcome` (`app.checkpoint`, `app.checkpoint_version`). Work queued while the deployment was hibernated waits for the resume.

**No false success (CL-40-02).** Two cases mean an agent gave nothing usable:
- It didn't answer within 30 s (an ABI 1.0 SDK, or a hung hook).
- Its node went away, or it didn't finish within 90 s.

Either way, the slot is recorded as `checkpointed: false`, and a resumed instance gets no checkpoint rather than a stale one. A hook-less agent that answers with no state *is* a successful, empty checkpoint. `status.hibernation` lists every slot with its version and whether it checkpointed. `status.phase` is `Hibernating` or `Hibernated`.

**Boundaries.** Only what the app returns is portable. Process memory, open sockets and browser sessions aren't migrated; an agent that holds them must rebuild them from its checkpoint. Instances keep the same checkpoint across their own restarts (it's their starting state); a new one is taken only at the next hibernation.
