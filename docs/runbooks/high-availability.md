# Runbook: running clankrd highly available

Clankrd replicas are stateless: everything lives in Postgres (ADR 0001).

- **Scheduling:** exactly one replica holds the scheduler lease at a time. It is fenced by an epoch, so a replica that lost the lease can't commit a stale decision (ADR 0008).
- **Everything else:** API calls, approvals and the engine tick run on every replica. Concurrent ticks are safe.

## Topology

```
            ┌──────────────┐      ┌──────────────┐
 clients ─▶ │  clankrd-a   │      │  clankrd-b   │   …more replicas
 (LB)       └──────┬───────┘      └──────┬───────┘
                   └──────────┬──────────┘
                        Postgres (HA: primary + standby / managed)
 nodes: --server https://clankrd-a:7600,https://clankrd-b:7600
```

- **Postgres** is the availability floor. Use a managed instance or a primary with a synchronous standby, and point `DATABASE_URL` at its failover address.
- **Replicas:** run two or more, in different failure domains.
- **Nodes** list every replica in `--server`, comma-separated. They talk to the last replica that worked, and move on when a connection can't be made:
  - **Heartbeat and certificate renewal** retry across replicas within the same attempt.
  - **Agent calls** (lease, complete, model, context, mailbox) move only when the connection was never established, so a completion is never sent twice. If a request may have reached a dying replica, the call returns an error, the agent's normal retry handles it, and the next call goes elsewhere.
- **Clients** (`clank`, the web UI and CI) go through a load balancer or DNS name that covers the replicas. The certificate for that name needs `--tls-san <name>` on each replica.

## Every replica needs the same

| | How |
|---|---|
| `DATABASE_URL` | same database |
| Master key | `CLANKR_MASTER_KEY` from your secret manager (never generate one per replica) |
| Cluster CA | the same `pki/ca.crt` and `pki/ca.key` in every replica's data dir. Each replica mints its own server certificate from it at start. |
| Owner and join tokens | `CLANKR_TOKEN`, `CLANKR_JOIN_TOKEN` |
| OIDC settings | same issuer and audience |

Starting a replica that has `ca.crt` but no `ca.key` is refused, so a replica can't quietly mint its own CA (see the backup-restore runbook).

## Failure behaviour (tested: `crates/clankr-node/tests/ha_e2e.rs`)

The test kills the replica that is both the scheduler leader and the node's first address. It drops that replica's listener, open connections, scheduler and ticker at once.

- **Running instances:** not disturbed. Same node, same epoch, no fencing.
- **In-flight runs:** keep completing through the survivor. The first one finished about 1.1 s after the kill.
- **Scheduler lease:** moves to the survivor within the lease TTL (5 × the scheduler period). New deployments get placed again.
- **Nodes:** stay Ready. Their fence timeout (default 30 s) only matters if every replica is unreachable.

## Upgrading without downtime

1. **Migrations:** releases only ever add migrations; nothing in them drops or renames something an older release reads. That is the policy all migrations so far follow, and it means a new replica can run against a database an old replica is still serving.
   - Upgrade one replica at a time: stop it, start the new version (it applies pending migrations), check `/readyz`, move to the next.
   - Never run a release older than the database's migrations.
2. **Nodes:** upgrade one at a time. `clank node drain <name>` first, so stateless instances move away. Volume-pinned instances wait for their node; that's expected (ADR 0008).
3. **Before:** take a backup and run `clankrd verify` against it (backup-restore runbook).
