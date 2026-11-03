# Runbook: backup and restore

**Who:** whoever operates the control plane.
**Drill:** monthly, and after any upgrade that adds a migration.
**Related:** ADR 0001 (Postgres is the only store). The tested procedure is `crates/clankrd/tests/backup.rs`.

## What holds state

| Thing | Where | Backed up by | Lose it and… |
|---|---|---|---|
| Resources, runs, history, tasks, approvals, audit log, secrets (encrypted), budgets | Postgres | `clankrd backup` / your Postgres backups | everything is gone |
| **Master key** (decrypts secrets, signs workload tokens) | `<data-dir>/master.key` or `CLANKR_MASTER_KEY` | **escrow** (never stored next to the backups) | every secret is unreadable; re-enter them all |
| **Cluster CA key** (signs node certificates) | `<data-dir>/pki/ca.key` | **escrow** | every node must re-join with a new CA |
| Cluster CA certificate | `<data-dir>/pki/ca.crt` | included in every backup | taken from the backup |
| Owner token, join token | `~/.config/clankr/` or env | your secret manager | rotate them, nothing else breaks |
| Local-path volumes | on each node | **not covered**: back up node disks yourself | that agent's data |
| Agent images | your OCI registry | the registry | rebuild with kennel |

Keep the escrowed keys somewhere other than the backups: a password manager, a KMS or a sealed envelope. A stolen backup that doesn't come with the keys exposes no secrets. `--include-keys` puts both keys into the backup directory. Use it only when the backup destination is itself encrypted and access-controlled.

## Taking backups

```sh
clankrd backup /backups/clankr-$(date -u +%Y%m%dT%H%MZ)    # DATABASE_URL, CLANKR_DATA_DIR from env
```

- **Snapshot:** this is a logical dump of a running cluster, taken from one Postgres snapshot. There is no pause and no lock on writers.
- **Manifest:** `manifest.json` records what the dump holds: schema version, resource revision, audit head, row count per table, the dump's checksum, and the master key's fingerprint (never the key itself).
- **Tools:** it needs `pg_dump` from the server's major version. Point at it with `--pg-bin` or `CLANKR_PG_BIN`.
- **Schedule:** at least daily. Keep 7 daily, 4 weekly and 12 monthly.
- **Smaller RPO:** for a recovery point under a day, also run Postgres continuous archiving (`pg_basebackup` + WAL archive, or your managed Postgres's point-in-time recovery). A PITR restore is checked the same way (`clankrd verify`, below), against the nearest logical backup's manifest. Expect counts to be newer, so treat that run's "row counts" and "revision" results as informational.

**Targets:**
- RPO = time since the last backup (daily logical) or the WAL archive lag (PITR).
- RTO is about 15 minutes, plus restore time. Restore time is roughly proportional to `db.dump` size, so measure it in your drill.

## Restoring

1. **Stop every clankrd.** Leave the nodes running. They keep their instances running, fence themselves when the control plane goes quiet (ADR 0008), and re-attach on their next heartbeat.
2. **Create an empty database.** Restore never writes over an existing clankr schema.
3. **Restore and verify.** If the master key wasn't included in the backup, pass the escrowed one (or set `CLANKR_MASTER_KEY`):
   ```sh
   DATABASE_URL=postgres://…/clankr_new clankrd --data-dir /var/lib/clankr \
     restore /backups/clankr-20261101T0300Z --master-key-file /secure/master.key
   ```
   The command refuses to start if:
   - the key isn't the one the backup was taken with,
   - the dump's checksum doesn't match the manifest,
   - the target database isn't empty, or
   - the backup is from a newer schema than this binary.

   After loading, it verifies the data and prints one line per check. Each must say `ok`:
   - `schema`, `revision`, `audit head` and `row counts` match the manifest exactly.
   - `audit chain`: every audit record's hash links to the one before.
   - `secrets`: every secret decrypts with the key.
   - `run replay`: every run's event history folds back to its stored state.
4. **Restore the CA key.** Put the escrowed `ca.key` at `<data-dir>/pki/ca.key`. If `ca.crt` is present without its key, clankrd refuses to start, so it never silently replaces your CA. Only if the key is truly lost: move `ca.crt` aside, start clankrd (it creates a new CA), then re-join every node and redistribute `ca.crt` to clients.
5. **Point clankrd at the restored database and start it.** Newer migrations apply on start. Check that:
   - `clank nodes` shows nodes returning to Ready;
   - `clank usage` and the web UI look right;
   - `clankrd verify <backup>` can be re-run at any time before you put the database back into service.

## Rotating the master key

1. **Stop every clankrd.** Running ones hold the old key in memory.
2. **Rotate:** `clankrd --data-dir <dir> rotate-master-key [--new-key-file new.key]`. It re-encrypts every secret in one transaction (all or nothing), then swaps `master.key`. The old key stays as `master.key.<fingerprint>.old`: backups taken before the rotation still need it. Escrow the new key.
3. **Start clankrd again.** Workload tokens are signed with a key derived from the master key, so nodes get new ones on their next heartbeat (they live 120s anyway).

A crash mid-rotation is settled at the next start or rotation: if the secrets decrypt with `master.key.new`, the rotation committed and the swap is finished; otherwise the pending key is discarded. With `CLANKR_MASTER_KEY`, move the key into `<data-dir>/master.key` first (rotation refuses to run, since it can't rewrite your environment).

## What happens to work in flight

Everything after the backup point is gone: runs started after it, events after it, approvals decided after it, and resources changed after it. Within what was restored:

- **Leased tasks:** leases expire on the first tick and the tasks are redelivered. Agents must already tolerate that, since delivery is at least once.
- **Unsafe tasks** (`sideEffects: unsafe`) whose lease lapsed become *outcome unknown* and wait for an operator (`clank run resolve`). Check the inbox and runs list for them. They may have run after the backup, so look at the external system before choosing `completed` or `retry`.
- **Timers and approval deadlines** that passed while the cluster was down fire, or expire, on the first tick.
- **Model budgets** restore the counters as of the backup. Spend made after the backup isn't counted again, so for one budget window the cap can be exceeded by at most that amount.
- **Instance placement:** placements restore as of the backup. A node still running an instance that the restored database places elsewhere, or no longer has, can't act for it: its workload token no longer matches the node and epoch. It stops the instance on its next heartbeat.

## The drill

1. Restore last night's backup into a scratch database using the steps above, without stopping production. Use a separate data directory and the escrowed keys.
2. Record:
   - the time each step took (this is your measured RTO);
   - the backup's age (your effective RPO);
   - the verification output.
3. Point a scratch clankrd at the restored database. Start one run, decide one approval and check one secret through a model call.
4. Drop the scratch database. File the record in the ops log.

`cargo test -p clankrd --test backup` runs the same drill automatically. It backs up while writers are active, restores, and checks each of the refusals above. It also confirms a restored run continues, a secret still decrypts, and tampering with the restored audit log is detected.
