# 0031 — Off-site audit export

Status: accepted (2026-11-03)

**Context.** The audit log is hash-chained and guarded by an append-only trigger, but anyone with superuser rights on the database can drop the trigger, then delete or rewrite rows and recompute the chain. Evidence the operator of the database can't touch has to live somewhere else.

**Decision.** `clankrd --audit-export <dest>` streams every audit row, in order, to a destination outside Postgres.
- **Destinations:** `file:///path.jsonl` (appended and fsynced; meant for append-only or WORM storage), or an `https://` receiver that gets NDJSON batches, with `$CLANKR_AUDIT_EXPORT_TOKEN` as a bearer token.
- **Who exports:** one replica at a time, under the `audit-export` leader lease, every 2 seconds.
- **Delivery:** at least once. The cursor (`audit_exports`) only avoids re-sending; receivers dedupe by `seq`.
- **Continuity:** before each batch the exporter checks that the database's chain continues from the last row it sent, and refuses to export if it doesn't. Rewritten history stops the export loudly instead of being shipped.
- **Checking:** `clankrd audit-check <export.jsonl>` verifies the export's own chain, then reports exported rows that are missing from the database or differ there. It exits 1 on any mismatch.

**Consequences.**
- Tampering is detectable for every row that was exported. A row rewritten in the seconds before its first export isn't covered, which is why the interval is short.
- The export is only as trustworthy as where it's kept: put the file on append-only storage, or use a receiver run by someone other than the database's operators.
