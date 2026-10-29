# 0009 — Agent mailboxes

Status: accepted (2026-11-01)

**Model.** An `AgentMailbox` resource (`maxDeliveries`, default 5; `retentionSeconds`, default 1 day) names a durable queue. Messages live in `mailbox_messages` with: sender, optional correlation ID, optional dedup key, a JSON payload (max 256 KiB; larger data goes by artifact reference), a TTL and a delivery count.

**Delivery contract (CL-30-04/05).** At-least-once, never "exactly once":
- `receive` leases messages in send order (FIFO per mailbox) for a visibility window and bumps a per-delivery epoch.
- `ack` needs the current epoch. A repeat from the same epoch is a no-op; older epochs get StaleLease, so a slow consumer can't ack a message that was redelivered elsewhere.
- `nack` redelivers after an optional delay. The `maxDeliveries`-th failure, or a lapsed lease at the limit, dead-letters the message with a reason. Expired TTLs are dead-lettered by the ticker and never delivered.
- An operator can requeue a dead letter (`clank mailbox requeue`), which is audited.
- A send with an existing dedup key returns the original message (`duplicate: true`), so retried sends don't double-deliver.

**Identity and authorization (CL-30-10).**
- The sender is the authenticated principal: `owner`, or `agent/<name>` from a node's workload token. It is never taken from the payload.
- An agent may only use mailboxes listed in `Agent.spec.mailboxes`. That's checked by the node broker and again by clankrd against the token's agent.
- Listing and requeue are owner-only.

**Not yet.** Pub/sub topics with durable subscriptions (CL-30-03), ordering partitions, per-sender quotas and backpressure beyond the size cap (CL-30-08), encrypted payload storage, and push delivery into SDK handlers (today agents pull with `receive`).
