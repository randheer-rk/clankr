# 0022 — Context snapshots, run time travel and fork

Status: accepted (2026-11-02)

**Context history.** A trigger on `context_objects` appends every write and delete to `context_history`. A trigger covers every write path, present and future. Context can be read as of any moment (`?asOf=`) or a named snapshot (`?snapshot=`, created by `POST …/contexts/{claim}/snapshots`). A restore rewrites keys whose values changed (as new versions, so stale optimistic writers get Conflict) and deletes keys created since. The restore runs in one transaction and its audit record commits with it. Snapshots and restores need `ResourcesWrite`: they're an operator's tool, so agents (WorkExecute) can't roll shared memory back.

**Time travel.** `GET /runs/{id}/state?at=N` replays the history up to event N. Engine state is a pure fold, so this is exact. Together with `asOf=` on contexts, it answers "what did the run know at that point".

**Fork.** `POST /runs/{id}/fork {at, patch}` starts a new run as follows:
- `at` is extended to the end of its decision (an input plus its consequences), so a fork never starts half-applied.
- Events up to that point are copied, then a `RunForked {from, at, patch}` event merges `patch` into the data.
- Whatever was pending there is re-armed: tasks become ready again, timers keep their due time, and approvals are re-requested from the forker.
- The fork keeps the original's pinned graph. Forking a point where the run had already finished is refused.
- Starting a fork is admitted like any run start (the `activeRuns` quota).

**Consequences.**
- Forked tasks run under the fork's own idempotency keys (keys include the run id), so their tool calls execute again instead of replaying the original's recorded results.
- Context is namespace-shared, so a fork reads live context. To rerun against old memory, restore a snapshot (or read `asOf`) explicitly.
- Snapshot positions come from a sequence. A write that commits concurrently with a snapshot can land just inside it (single-statement window).
