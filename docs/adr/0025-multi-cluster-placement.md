# 0025 — Multi-cluster placement

Status: accepted (2026-11-02)

**Decision.** Multi-cluster is explicit routing, not transparent migration (CL-23-11, CL-40-11):

- **`kind: Cluster`** (in `clankr-system`): a remote control plane. It has an endpoint (https), a region, an optional CA, and a ServiceAccount token on the remote, kept as a Secret here.
- **`kind: PlacementPolicy`** (per namespace): `allowedRegions`, `requiredSecrets` and `localOnly`. Every policy in the namespace applies.
- **Starting a run elsewhere:** `POST …/workflows/{wf}/runs {input, cluster}` (or `clank run start wf --cluster eu-1`). The start is refused unless all of these hold:
  1. The policies allow the cluster's region (else Forbidden, audited).
  2. The remote holds the Workflow with an identical spec digest. If it's missing, the start is Forbidden. If it differs, it's Conflict. Nothing is pushed implicitly.
  3. The remote holds every required secret in the same namespace (else Forbidden, audited).
- **What happens on success:** the run is started on the remote with an idempotency key, so retries return the same remote run. It's recorded in `remote_runs`, and the start is audited here. `GET …/remoteruns[/{id}]` lists those runs and fetches live status from the remote.

**Not included.**
- Moving a running run, its context or its volumes between clusters. That needs the explicit copy/verify flow of CL-40-11.
- Automatic cluster choice (scoring by region, cost or capacity). Callers name the cluster.
- Federated identity: the remote sees the federation ServiceAccount, and the local audit records who asked.
