# Clankr — Implementation Plan

Source: `Clankr_Comprehensive_PRD_v2.0.docx` (v2.0, 2026-11-01)

## 0. Reading of the PRD

The PRD lists the whole long-term inventory: about 60 resource kinds, about 300 traceable capabilities, and 40+ `CL-xx` requirement groups. The PRD itself says that "included" means tracked, not committed, so this plan does not try to build all of it. It builds **Tier 0 → Tier 1** (CL-18-01/02) as vertical slices. Each slice ends at a demo from §42 or the E2E matrix. Everything else stays backlog until a gate pulls it in.

The core invariants that cannot be cut (§9.2, §36):

1. A run's event sequence is unique and strictly increasing.
2. The state transition, event append and dispatch intent commit in **one transaction**.
3. A stale lease epoch cannot commit.
4. Replay never calls models or tools.
5. Provider keys never reach the image, env, logs or prompt.
6. The node, gateway and store revalidate every privileged call. The SDK is untrusted.

### Simplifications the PRD allows, which save the most work

| PRD wording | Simplification |
|---|---|
| "Task scheduling and event commit use an outbox or equivalent" | The `tasks` table lives in the same DB as `workflow_events`, so the task row **is** the outbox. Insert it in the same txn. No separate publisher until a broker (NATS) exists, and none is needed for Tier 0/1. |
| Deterministic replay (DUR-06, CL-21-07) | Graphs are **declarative YAML**, not user code. Run state = `fold(events)`, so replay is a pure function. There's no Temporal-style determinism sandbox and no code-version pinning beyond `graph_digest`. A code-first DSL (DEV-017) comes later and compiles to the same graph. |
| Sandbox (§32, open decision 17.4) | **Decided: embed `youki`'s `libcontainer` crate** in `clankr-node`. No external runtime binary. We still generate the OCI runtime spec (`oci-spec` crate) and let libcontainer do rootless userns, mount/pid/ipc/net namespaces, seccomp, caps drop, no-new-privs and cgroup v2. The **same launcher runs build `RUN` steps**, so the builder and runtime share one sandbox code path. Risk: libcontainer is less battle-tested than crun. Mitigated by the M2 escape suite running against it on every CI run. |
| Builder (§14, §24) | **Decided: our own builder** (see §2a). It pulls the base through `oci-client`, unpacks layers into a CAS, runs each `RUN` in a libcontainer sandbox over a rootless overlayfs, tars the upper dir into a layer, and writes an OCI image layout. No buildah and no Docker. |
| Registry | No registry of our own. Use a local OCI image-layout dir + the `oci-client` crate for push/pull to any OCI registry. Signing uses `sigstore` (cosign format) at M4. |
| Wire protocols (decision 12.1) | Public API: REST/JSON + SSE (axum). Agent↔node: length-prefixed JSON frames over UDS. Node↔control: HTTP long-poll at M3. Add gRPC/protobuf only when a measured need shows up. |
| Metadata DB (decision 12.1) | **Decided: Postgres from day one, one backend only.** Dispatch uses `FOR UPDATE SKIP LOCKED` from M1, the leader/lease epochs are rows, and `LISTEN/NOTIFY` wakes the engine and watchers. Local dev and tests use the `postgresql_embedded` crate (downloads and runs a private PG, no Docker), started by `clankrd --dev`. Pointing at a real cluster just means setting `DATABASE_URL`. |

## 1. Workspace layout (start with 7 crates, not 28)

The PRD's §3.3 lists 28 crates and calls them "design candidates". Start with these 7 and split a crate only when it hurts.

```
clankr/
  crates/
    clankr-types/     resource envelope, kinds, conditions, errors (CL-35-06 codes), event + frame schemas
    clankr-store/     sqlx migrations, resources CAS, events, tasks/leases, timers, audit, context_objects
    clankr-engine/    graph validator, interpreter (fold), reducers, scheduler-of-tasks, timer loop, controllers
    clankr-node/      node daemon: OCI spec gen, libcontainer launch, UDS bridge, Hello/Welcome, lease heartbeats
    clankr-gateway/   model gateway (OpenAI-compatible + Anthropic adapters), budgets, tool gateway, policy check
    (image build + sandbox live in the separate kennel project, ADR 0007)
    clankrd/          binary: axum API + engine + gateway + (local) node in one process for Tier 0
    clank/            binary: CLI (clap)
  sdk/python/clankr/  AgentApp, Event, Context — talks UDS frames
  base-images/        base-minimal, base-python (Containerfiles + clankr-init + clankr-agent-host)
  tests/e2e/          scenario tests mirroring E2E/DEMO IDs
  docs/adr/           one ADR per decision in §12.1 / §17.4 before schemas freeze
```

Key deps: `tokio`, `axum`, `sqlx` (postgres), `postgresql_embedded` (dev/test), `libcontainer` (youki), `oci-spec`, `oci-client`, `tar`, `flate2`/`zstd`, `serde`/`serde_yaml`, `clap`, `reqwest`, `tracing` + `opentelemetry`, `proptest`, `sha2`, `ulid`, `fail`.

## 2. Milestones (vertical slices)

These replace the three slightly conflicting milestone tables (§11, §17.1, §39), which this plan merges. Each milestone has one exit demo, and the milestone isn't done until that demo runs in CI.

### M0 — Contracts (foundation)
- `clankr-types`: envelope (`apiVersion/kind/metadata/spec/status`), conditions, the stable error codes (CL-35-06), `EventEnvelope`, and UDS frame types `Hello/Welcome/Ready/Status/Deliver/Ack/Nack/Checkpoint` with ABI major/minor (CL-35-04).
- `clankr-store`: tables `resources`, `workflow_runs`, `workflow_events`, `tasks` (with `fencing_epoch`), `timers`, `audit_events`. Constraints: `UNIQUE(run_id, seq)`, `UNIQUE(ns, kind, name)`, CAS on `rv`.
- `clankrd`: generic resource CRUD + `?watch=true&cursor=` via SSE, dry-run, and status subresource (CL-19-01..04, 19-02).
- `clank apply/get/describe/delete`.
- Auth bootstrap: a single local owner token plus an explicit "dev mode" flag (SEC-01 isolated-dev exception).
- ADRs recording the decided items: Postgres-only store, wire protocol, youki/libcontainer sandbox, native builder, YAML graph DSL.
- **Exit:** proptest shows that concurrent CAS gives exactly one winner, watch resume works and reports history expiry, and golden manifests parse.

### M1 — Durable single-node engine (Tier 0, CL-18-01)
- Graph validator: node types `agent`, `activity`, `condition`, `parallel/join(all|any|quorum)`, `map`, `timer`, `signalWait`, `humanApproval`, `terminal`. Cycles need `maxIterations`, and reducers must be declared (CL-21-01/05/06).
- Interpreter: `apply(state, event) -> (state, Vec<Command>)`. Every command is persisted in the same txn as the event (invariant 2).
- Queue: lease with `fencing_epoch = epoch+1`, then heartbeat, then complete or fail. A stale epoch gets `StaleLease`, and a repeat ack is idempotent (CL-22-02/03). Retry policy, DLQ table, and `OutcomeUnknown` (CL-22-04/05/07).
- Durable timers table plus a poll loop (CL-22-06). Signals and the approval API `runs/{id}:signal`.
- Node (local): unpack image from the CAS → OCI spec → libcontainer rootless, non-root, read-only rootfs, no-new-privs, seccomp default, cgroup v2 limits → UDS at `/run/clankr/agent.sock` → Hello/Welcome → Ready → deliver tasks.
- `base-python` image: `clankr-init` (a tiny Rust PID 1 for reaping and signals), `clankr-agent-host` (Rust, the frame bridge), Python SDK (`AgentApp.on`, `event.ack/checkpoint`, `context.get/put`, `models.generate`).
- Model gateway v0: `ProviderConnection` + `ModelClaim`, OpenAI-compatible + Anthropic adapters, key held only in the daemon, agent gets a claim handle. Token budget reserved before the call and reconciled after (CL-27-06). Includes a **mock provider** for tests (DEV-011).
- Context v0: `ContextClaim` → `context_objects` KV with version + CAS (CL-29-08). No vectors.
- `clank build` (native builder v1, §2a): single-stage, `FROM@digest`, `COPY`, `RUN`, `WORKDIR`, `ENV`, `USER`, `LABEL`, `SKILL`, `AGENT_ENTRYPOINT`, `REQUIRE_RUNTIME_ABI`. Writes an OCI layout + ABI descriptor + skill digests. Rejects secret `ARG`s and unpinned `FROM`.
- `clank init --template python`, `clank run --local`, `clank logs`, `clank workflow run`, `clank replay` (offline fold).
- **Exit (= CL-18-01, DEMO-02 local):** two-agent graph, `kill -9 clankrd` at every txn boundary (a failpoint harness, not a sleep), restart → no lost or double transition, approval after restart resumes exactly once, and the replay output is identical with the network off. A grep of the image layers finds no provider key (E2E-07).

### M2 — Secure packaging (supply chain + policy)
- Skill bundles: `skill.yaml` schema, path-traversal/symlink checks, digest pinning, and capability requests compared against `ToolClaim`/`AgentPolicy` at build, admit and call time (CL-26-01..04).
- Tool gateway: `ToolClaim` allow/deny/methods/hosts, side-effect class, idempotency key, `approvalRequired` → `ApprovalTask` bound to an action digest (CL-28-02/06/07, CL-31-09).
- Policy engine v0: deny-dominant intersection of cluster > namespace > agent (§13.3), evaluated at admission, node launch, gateway and context. Audit row on every decision (CL-31-04/05/10).
- Egress: agent netns has **no route out** except the gateway socket. This is the simplest default-deny and holds even if the SDK is replaced (CL-32-04/08). Per-host allowlists go through the gateway proxy.
- ImagePolicy: digest pinning, base lineage allowlist, ABI min, signature (cosign format via `sigstore` crate). SBOM: own package-db reader (dpkg/apk status + Python `dist-info` + npm lock) emitting SPDX JSON. A vuln scan via an external DB (OSV) is optional at this point.
- Push/pull through `oci-client`, with digest verification on pull (CL-33-04).
- **Exit (DEMO-01 + DEMO-04):** build Python agent + git/ffmpeg + 2 skills → scan/sign/push → deploy by digest. A malicious skill that reads a secret and dials an external host is denied and audited, and tampering with one layer byte is rejected.

### M3 — Distributed runtime (Tier 1, CL-18-02)
- Postgres is already in place. Add a leader lease row with an epoch for the single active scheduler (§23 "first distributed release").
- Node enrollment with an mTLS cert issued by `clankrd` (rcgen), heartbeats, `Node` resource, Suspect → lease expiry → reassign (CL-20 failure policy).
- Scheduler: hard filter (arch, runtime class, claims, resources, taints) → score (capacity, locality) → reserve, with an explainable decision stored in status (CL-23-03/04/05).
- VolumeClaim: `local-path` provisioner only, RWO with an attachment fence, and local volumes pin placement or report `BlockedMigration` (CL-29-02/03/06).
- Per-instance workload identity (short-lived JWT bound to ns/revision/instance/node/epoch), refreshed over UDS (CL-31-02).
- AgentMailbox v0 on the same tasks/lease machinery (ack, redelivery, TTL, DLQ) (CL-30-02).
- OTel traces/logs with run/invocation/session/model-call IDs and redaction (CL-34-04/05).
- **Exit (CL-18-02, DEMO-03, E2E-04/05):** a 3-worker cluster runs concurrent graphs, `kill -9` on a worker leads to reassignment with the stale epoch rejected, a partitioned worker cannot commit, and a second RWO attach is denied until fenced.

### M4 — Operations beta
RBAC/OIDC, namespaces + quotas + budgets dashboard, approval inbox, CLI polish, a minimal UI (run graph + timeline + approvals only), backups/restore runbook, an eval harness on golden traces, image promotion. **Exit:** DEMO-05, DEMO-06, E2E-12/18.

### M5+ — Backlog, gated (Tier 2/3)
HA failover, rolling upgrades, autoscaling, fork/time travel (needs context snapshots first, §39 dependency path), hibernation, semantic memory/vectors, multi-cluster, S2/S3 sandboxes (gVisor/Firecracker), the Node/Rust/browser base images, Go/TS SDKs, and everything in §40/Appendix A. Each one enters through the §40 stage gate (sim → shadow → canary → GA).

## 2a. Native builder design (`clankr-build`)

The pipeline runs in `clank build` locally and later in a BuilderPool worker. The code is the same in both.

1. **Context ingest:** walk the source dir honoring `.clankignore`, reject symlinks that escape it, and sha256 every file. The context digest is a Merkle hash of `(path, mode, digest)`.
2. **Parse/validate:** strict Clankfile grammar (pick one dialect, see §5). Requires `FROM <ref>@sha256:`, an instruction allowlist and an ABI version. `--secret id=x` mounts are only allowed on `RUN`, and `ARG`/`ENV` values that match a secret are refused (BLD-09).
3. **Base:** `oci-client` pulls the manifest and blobs, verifies the digests and stores blobs in a CAS at `~/.local/share/clankr/blobs/sha256/`. Each layer is unpacked once into `layers/<diffid>/`, with OCI whiteouts converted to overlay whiteouts.
4. **Steps:** each instruction gets a **cache key** = sha256(parent chain id, instruction text, referenced context-file digests, platform). On a hit, the step reuses the stored layer.
   - `COPY`/`SKILL`: write directly into a fresh upper dir with no sandbox, then chown to the image UID.
   - `RUN`: mount a rootless overlay (`lowerdir=parent layers, upperdir=new`) inside a user namespace (kernel ≥5.11, which this box has), then run the command through the **same libcontainer launcher as the node**. The run uses a builder seccomp profile, no host mounts, a CPU/mem/pids/time cap and network off by default. A `--network=allowlist` option routes through the gateway proxy with the BuildPolicy hosts (BLD-10/CL-24-11). Secrets are mounted on tmpfs only for that step.
   - `ENV/WORKDIR/USER/LABEL/AGENT_ENTRYPOINT`: config-only, no layer.
5. **Layer emit:** tar the upper dir deterministically (sorted entries, mtime clamped to `SOURCE_DATE_EPOCH`, uid/gid normalized, overlay whiteouts → `.wh.`), compress with zstd (gzip opt-in), and compute the diffid + blob digest into the CAS.
6. **Image:** OCI config (with `org.clankr.abi`, `org.clankr.skills`, entrypoint `clankr-init`), manifest and `index.json` in an OCI layout. Provenance JSON records the context digest, base digest, cache keys, builder identity and timestamps.
7. **Post-build gates:** skill manifest checks, an ABI smoke test (launch the image, expect Hello within N s), a secret-marker scan of all layers + build logs (E2E-07), then SBOM (M2), sign (M2), and push through `oci-client`.

Cache isolation (BLD-11): the cache dir is keyed per tenant/namespace, and keys include the base digest, so a poisoned cache in one namespace is invisible to the others.

Deferred: multi-stage builds (M2), multi-arch via binfmt/qemu (M4), and a remote BuilderPool (M3). Cross-arch is refused before M4.

## 3. Testing approach (from day one, not M4)
- **Failpoints** (`fail` crate) at each txn boundary in the engine. The crash suite loops `kill -9` over every point and checks invariants 1–3. This is the single most important test asset.
- **proptest**: CAS, epoch monotonicity, reducer commutativity for parallel joins, graph validator.
- **Golden histories**: committed `events.jsonl` + expected state; `clank replay` must reproduce them byte-for-byte.
- **Sandbox escape suite** (from M2): mount, `/proc`, raw socket, host route, `/run/clankr` credential harvest, fork bomb.
- E2E tests are named after PRD IDs so the traceability in CL-38-02 is just `grep`.

## 4. Decisions
1. **Store:** Postgres from the start, single backend. ✅ decided
2. **Sandbox:** embed `youki/libcontainer`. ✅ decided
3. **Builder:** native Clankr builder. ✅ decided
4. **Graph authoring:** YAML DSL only for M1 *(rec)*; a Python code-first DSL later. — pending
5. **Model providers for M1:** OpenAI-compatible + Anthropic + mock *(rec)*. — pending

## 5. PRD inconsistencies to fix in the doc
- `apiVersion` is `clankr.io/v1` in some places (§13.5, §14.4, §16) and `v1alpha1` in others. Pick `v1alpha1` until CL-18-05 gates pass.
- Crate prefix is `clank-*` (§3.3) vs `clankr-*` (§39).
- Two Clankfile dialects: `SYSTEM`/`AGENT_ENTRY`/`SKILL name path` (§14.4) vs `SYSTEM_PACKAGES`/`AGENT_ENTRYPOINT`/`SKILL path AS name`/`PYTHON_INSTALL` (§24). Pick one grammar.
- Base image names are `clankr/agent-python` (§15) vs `clankr/base-python` (§25).
- Three different milestone tables (§11, §17.1, §39). Merged above.
- ID collisions: `AGE-001..020` (agent identity) vs `AGE-001..026` (build/image), and `E2E-01..12` (§17.2) vs `E2E-01..18` (§38).
- §5 INT-01 says "ork-equivalent", which looks like a leftover from an earlier product name.
- Workflow state tables differ: `workflow_executions/workflow_snapshots` (§9.1) vs `workflow_runs/task_outbox/task_leases` (§36). The schema above follows §36 minus the separate outbox.

## 6. First week, concretely
1. `cargo new --lib` workspace with the 7 crates; CI runs `cargo test` + `clippy` against an embedded Postgres.
2. `clankr-types` envelope + error codes + frame enums (serde, golden JSON fixtures).
3. `clankr-store` migration v1 + `Resources::{create,get,list,update_cas,delete}` + proptest for CAS.
4. `clankrd` axum CRUD + SSE watch; `clank apply/get`.
5. ADRs 0001–0005 for the decisions in §4.
6. Spike: launch a busybox rootfs via `libcontainer` rootless from a test (proves the sandbox path both node and builder depend on).

## 7. Progress

**M0, done** (except the sandbox spike)
- [x] Workspace crates: `clankr-types`, `clankr-store`, `clankr-engine`, `clankrd`, `clank`, `clankr-node`
- [x] Resource envelope, kind registry, stable error codes, envelope + per-kind admission (Workflow)
- [x] Postgres store: CAS CRUD, status subresource, finalizers, gap-free global revisions, change log + compaction (`HistoryExpired`), LISTEN/NOTIFY
- [x] `clankrd` REST + SSE watch (cursor resume, bookmark), owner token (0600), `x-request-id`, `/healthz` `/readyz`, `--dev` embedded Postgres
- [x] Audit: every mutation and every auth denial, in the same txn, hash-chained, DB trigger blocks UPDATE/DELETE, `verify_audit_chain`, `GET /v1/audit`
- [x] `clank apply/get/delete` (+ `-w` watch)
- [x] ADRs 0001–0006
- [ ] libcontainer rootless spike: written; **skips itself** until `deploy/apparmor/clankr-dev` is installed (ADR 0006)

**M1, in progress**
- [x] Graph admission: refs, unreachable nodes, join in-degree, durations, unbounded cycles (Kahn over the unbounded subgraph)
- [x] Interpreter: `evolve` (pure, graph/clock-free) + `decide`; agent, activity, condition, join(all/any), timer, approval, signal; retries with deterministic backoff; maxSteps/maxIterations; cancel
- [x] Durable layer: one txn per step (events + task/timer rows + cached state + audit), fenced leases (`lease_epoch`), idempotent duplicate ack, `StaleLease`, lease expiry + timers via `tick`, idempotent start, namespace isolation, `verify` (replay == stored state)
- [x] API: runs (start/list/get/history/signal/cancel), tasks (lease long-poll on NOTIFY, heartbeat, complete, fail)
- [x] CLI: `clank run start|get|list|history|signal|approve|cancel`, `clank worker` (S0 dev tier: shell command per task)
- [x] **Crash test**: real `clankrd` killed with SIGKILL 10–12× per run while 6 runs × 8 tasks complete. All succeed, every task accepted exactly once, approval resumes once, replay == state, audit chain intact (stable over 6 consecutive runs)
- [x] `OutcomeUnknown` (CL-22-07/08): `sideEffects: unsafe` tasks aren't redelivered after a lost lease; a late answer from the original holder resolves them, otherwise an operator does (`clank run resolve`); stable `idempotencyKey` across attempts
- [x] Context v0: ContextClaim-scoped versioned KV, CAS (`ifVersion`, 0 = create-only), 256 KiB cap, writer provenance; 8 concurrent writers × 10 increments lose nothing
- [x] Secrets: AES-256-GCM at rest (master key in a 0600 file or `CLANKR_MASTER_KEY`), write-only API, every decrypt audited with its purpose, `clank secret`
- [x] Model gateway v0 (`clankr-gateway`): ProviderConnection/ModelClaim admission (https-only, loopback exception), mock + OpenAI-compatible + Anthropic Messages adapters, credential brokered (never sent to the agent), allowlist, per-run/total token budgets with atomic reservation, `model_calls` attribution ledger, no redirects; revoking the connection blocks the next call
- [x] Agent ABI v1 (`clankr_types::protocol`): length-prefixed JSON over a per-instance UDS; Hello/Welcome/Reject/Ready/Deliver/Complete/Fail/Call/Reply/Shutdown
- [x] Node daemon (`clankr-node`, process runtime = S0): handshake + ABI rejection, lease → deliver → complete/fail, heartbeats, node-side capability checks for model/context calls, token never passed to the agent, restart with backoff; crash mid-task → lease lapses → redelivered
- [x] Python SDK (`sdk/python/clankr`, stdlib only): `AgentApp`, `@app.task(node)`, `ctx.models.generate`, `ctx.context(claim).get/put/list`, `Permanent`, `ClankrError`
- [x] Native builder v1, **moved to the separate kennel project** (`~/Dev/rust/kennel`, ADR 0007): strict Clankfile (no ARG, secret-looking ENV refused, pinned FROM), `.clankignore`, symlinks never followed, deterministic gzip layers (same input → same digest), step cache, SKILL with versioned skill.yaml → digest label, ABI + source-digest labels, OCI image layout, digest re-verification on every blob read
- [x] **kennel** (own repo `~/Dev/rust/kennel`): `kennel pull` (Docker Hub/OCI registries, multi-arch index → linux/<arch>, digest-verified), `RUN [--network=host]` in a rootless sandbox (no network by default, whiteouts, cached), `kennel run` (shared read-only rootfs per image, writable /tmp tmpfs + volumes, own netns, signal forwarding, exit status passthrough), static `kennel-init` as PID 1 (reaps, forwards, dies with `kennel run`)
- [x] Clankr Python base image `sdk/python/Kennelfile` (python:3.12-slim pinned + SDK, build-time import smoke test)
- [x] Clankr node OCI runtime via `kennel run` (ADR 0007): agent containers get no network, only the instance socket dir is mounted; e2e passes (read-only root, no network, no token, kennel-init PID 1)
- [x] Hardening (kennel ADR 0004): seccomp filter, real non-root agents via subuid (base image `USER 65534`, no caps, host groups dropped by kennel-init; vendored one-line libcontainer setgroups fix), cgroup v2 limits from `Agent.spec.resources` (defaults 1 GiB / 1 CPU / 256 pids); container e2e asserts all of it
- [x] kennel: seccomp default-deny allowlist generated from moby's profile (`tools/gen-seccomp.py`) minus kennel's denials, AF_PACKET removed; `kennel push`/`login`/`logout` (byte-for-byte, digest preserved; tested against a real `registry:2` that kennel itself runs)

**M3, in progress** (ADR 0008)
- [x] Postgres leader lease with fencing epochs; stale leader's reconcile writes nothing
- [x] Node join (join token → per-node hashed credential), heartbeats, NotReady after 10s
- [x] Node-scoped authorization: own heartbeat, hosted Agents, their task queues, brokered model/context calls only; everything else Forbidden + audited
- [x] `AgentDeployment` + scheduler: filter (ready, runtime, nodeSelector, memory/cpu) → most-free-memory score → reassign from dead nodes with epoch bump; status with reasons
- [x] Fleet-mode `clankr-node --join-token`: runs its assignments, stops reassigned ones, self-fences when cut off
- [x] e2e: 2 nodes, spread, workflow, node loss → rescheduled in ~10s, workflow completes; `clank nodes`
- [x] TLS by default with a cluster CA (`clankr-tls`); nodes join with a CSR (key never leaves the node) and authenticate by mTLS; re-join revokes the old cert
- [x] Workload identity tokens per instance (node + epoch bound, 120s, HMAC); every node call for an agent requires one and is refused the moment the instance moves
- [x] Socket paths under `$XDG_RUNTIME_DIR` (found by the real-binary smoke test: long work dirs overflowed sun_path)
- [x] VolumeClaim local-path: bind-on-first-use, node pinning, RWO single attachment, no silent moves on node loss; mounts via kennel `-v` (process runtime gets `CLANKR_VOLUMES`); cordon/uncordon/drain with blocker notes; e2e covers persistence, RWO, drain, node loss + return
- [x] AgentMailbox v0 (ADR 0009): durable at-least-once FIFO, epoch-fenced ack, nack/redelivery, maxDeliveries + TTL dead-lettering, requeue, dedup keys, authenticated sender, `Agent.spec.mailboxes` authz (node + server), SDK `ctx.mailbox().send/receive`, `clank mailbox ls|send|requeue`
- [x] OpenTelemetry tracing (ADR 0010): request/engine/task/model/scheduler spans, traceparent continuation, OTLP/HTTP export verified against a live receiver
- [x] Node certificate rotation with a 5-minute overlap (stress test: work keeps completing across repeated rotations)

**M3 complete.**

M4 (operations beta):
- [x] Identity + RBAC (ADR 0011): ServiceAccount API tokens (hashed, TTL, revoke), OIDC ID-token login (JWKS discovery, iss/aud/exp/nbf, asymmetric only), RoleBindings with built-in roles (viewer/editor/approver/admin/cluster-admin), `clankr-system` bindings cluster-wide, central route→permission check, denials audited; `clank whoami`, `clank token create|list|revoke`
- [x] RBAC follow-ups: revision-keyed binding cache, JWKS rotation test, `clank login` (device flow + silent refresh) / `logout`
- [x] Quotas and budgets (ADR 0012): ResourceQuota (cpus/memoryMiB/instances/volumeMiB/activeRuns/count/<plural>) under a per-namespace advisory lock, increase-only refusal; AgentBudget (day/month/total, tokens + USD) reserved atomically with claim limits; ProviderConnection pricing, per-call cost ledger, claim cost limits; `GET .../usage` + `clank usage`
- [x] Approval inbox (ADR 0013): request per approval visit bound to run/node/visit/data digest, approver role + group, no self-approval, digest-bound decisions, decided once (same txn as resume), deadlines → expired/onReject, cancelled with the run, raw approve.* signals refused; `clank approvals ls|show|approve|reject`
- [x] Web UI (ADR 0014): "Vision" design signed off after 4 mockup rounds; Vite/React app in `ui/` embedded in clankrd; inbox, runs (live flow graph, activity, history, data, cost, cancel), agents, workflows (start run), budgets (spend chart + forecast, caps, quotas, spend by claim); token sign-in
- [x] Backup/restore (docs/runbooks/backup-restore.md): `clankrd backup|restore|verify`; snapshot-consistent pg_dump + manifest (revision, audit head, row counts, dump checksum, key fingerprint); keys escrowed separately by default; restore refuses wrong key / damaged dump / non-empty target / newer schema, then verifies audit chain, secrets, run replay; CA cert without key refuses to start; drill test incl. concurrent writers and tamper detection
- [ ] Eval harness (golden traces)
- [ ] Image promotion by digest
