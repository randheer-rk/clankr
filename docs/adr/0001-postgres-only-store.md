# 0001 — Postgres is the only metadata store

Status: accepted (2026-11-01)

**Decision.** All authoritative state (resources, workflow events, tasks/leases, timers, audit) lives in Postgres from M0. No SQLite backend. Local dev and tests run a private server through `postgresql_embedded` (`clankrd --dev`, `DevPostgres` in tests); production sets `DATABASE_URL`.

**Why.** One SQL dialect and one set of locking semantics (`FOR UPDATE`, `SKIP LOCKED`, `LISTEN/NOTIFY`). The PRD invariants (atomic transition + dispatch, fenced leases) get tested against the engine production actually runs.

**Consequences.**
- First `--dev` start downloads Postgres binaries (~/.theseus); after that it works offline.
- Watch revisions come from a single-row counter bumped inside each write transaction. That serializes resource writes so commit order matches revision order, and a watch cursor can never skip a late-committing write. Throughput ceiling is one writer at a time on resource mutations; shard the counter per namespace if that ever binds.
- HA of the database is a deployment concern (PRD §36), not Clankr's.
