# 0008 — Cluster membership, scheduling and leader election

Status: accepted (2026-11-01)

**Leader election.** A `leader_leases` row per role (`scheduler`) holds holder, epoch and expiry. `acquire_leadership` renews for the current holder and only hands over to a new holder once the lease has expired, bumping the epoch. Every fenced transaction starts with `Store::fence`, which locks the row and checks holder + epoch + expiry, so a paused or partitioned ex-leader fails with StaleLease before it writes anything (CL-23-05, CL-36-03). Every clankrd runs the loop; one wins.

**Transport and node identity (SEC-04).**
- clankrd serves HTTPS by default with its own cluster CA (`<data-dir>/pki`, key 0600). `--insecure-http` is an explicit, logged opt-out, and nodes can't join in that mode.
- The CLI and nodes pin the cluster CA instead of using system roots.
- A node joins with the cluster join token plus a CSR for a key it generated itself; the key never leaves the node and lives in memory only.
- clankrd signs a 30-day client-auth certificate with CN = node name, on its own terms: requested extensions are ignored. It stores that certificate's fingerprint, so only the most recently issued certificate authenticates the node, and re-joining revokes older ones.
- After joining, the node authenticates every call with mTLS.
- It heartbeats every 2s and gets its assignments back in the reply. After 10s without a heartbeat (`NODE_TIMEOUT_SECS`) the scheduler marks it NotReady.

**Workload identity (CL-31-02).**
- Every heartbeat reply carries a fresh token per assignment: claims are namespace, agent, instance, node, placement epoch and a 120s expiry, signed with HMAC-SHA256 under a key derived from the master key. Only clankrd issues and verifies tokens.
- Every call a node makes for an agent must carry that agent's token in `x-clankr-workload`. The server verifies the signature and expiry, that the claims match this node, namespace and agent, and that the instance is still placed on this node at that epoch.
- So once an instance is moved, the old node's task, model and context calls fail with StaleLease immediately, not just when its task leases expire. The trusted `x-clankr-on-behalf-of` header is gone for nodes.

**Node-scoped authorization (least privilege).** A node certificate can only:
- heartbeat as itself

Everything else requires a workload token for a hosted agent:
- reading that Agent
- serving its `agent/<name>` task queue
- brokering its model and context calls

Everything else is Forbidden and audited.

**Scheduling.**
- `AgentDeployment {agent, replicas, nodeSelector}`. Each pass is one fenced transaction.
- Filter nodes by Ready, runtime support, labels, and free memory/CPU (capacity minus instances already placed).
- Score by most free memory, which spreads load. Ties break by name, so placement is deterministic.
- Instances on NotReady nodes are reassigned with `epoch + 1`. Scale down removes unplaced and NotReady instances first. Deleted deployments lose their instances.
- `status` reports scheduled and pending counts, plus a filter-reason summary when something can't be placed (e.g. "2 node(s) insufficient memory").

**Volumes (local-path) and maintenance.**
- `VolumeClaim` supports `storageClass: local-path`, ReadWriteOnce only, `reclaimPolicy: Retain` only. Shared modes are refused with UnsupportedCapability (CL-29-04).
- A claim binds to the node that first runs an instance using it (`volume_bindings`), and its data lives at `<node work-dir>/volumes/<ns>/<claim>`. From then on every instance using the claim is pinned to that node.
- Only one instance cluster-wide holds a ReadWriteOnce claim at a time; others stay unplaced with a note.
- If the claim's node is NotReady, the instance stays unplaced with an explicit "data doesn't move between nodes" reason, and returns when the node does. It is never restarted on empty storage elsewhere (CL-20, CL-23-10, CL-29-06).
- `clank node cordon|uncordon|drain`: cordon stops new placements; drain also moves movable instances off.
- Volume-pinned instances on a draining node keep running and carry a `drain blocked: local volume …` note (E2E-14). There is no implicit data migration; snapshot/restore-based migration comes later.
- Capacity isn't enforced for plain directories.

**Fleet nodes.** `clankr-node --join-token …` runs exactly its assignments. An instance whose assignment disappears or comes back with a different epoch is stopped. If the node can't heartbeat for longer than the node timeout it stops everything itself, because the scheduler has moved those instances by then. Duplicate work during the overlap is still harmless: task leases are fenced (ADR 0001 store, CL-22-02).

**Certificate rotation.**
- Fleet nodes renew when less than `renew_before` (default 10 days) of the 30-day validity remains: a new key, a CSR sent over the current mTLS identity to `POST /v1/nodes/{name}/renew`, then the node's shared HTTP client is swapped under all its instances.
- The previous certificate stays valid for 5 minutes so in-flight requests on pooled connections finish. Without that overlap, a stress test lost task completions mid-rotation.
- Re-join still revokes older certificates immediately.

**Not yet (next steps):**
- Preemption, priorities and taints.
- Node drain or cordon.
- Placement that accounts for storage locality (VolumeClaims).
