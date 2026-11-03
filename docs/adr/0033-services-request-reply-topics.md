# 0033 — Agent services, request/reply and topics

Status: accepted (2026-11-03)

**Context.** Mailboxes (ADR 0009) gave agents durable queues, but callers had to know a mailbox, poll it, and invent their own reply scheme. PRD §30 asks for logical addressing, request/reply with timeouts, pub/sub with durable subscriptions, and delivery into handlers.

**Decision.** All three are built on the mailbox machinery, so every message keeps its guarantees: durable, at least once, fenced by epoch, dead-lettered by policy, and sent under the authenticated sender's name.

- **`AgentService {agent}`.** A stable address, `clankr://agents/<namespace>/<name>`, backed by the queue `svc:<name>`. Every instance of the serving agent competes for it, so callers never learn instances or nodes, and rollouts and moves don't matter to them.
  - **Sending:** agents that list the service in `spec.calls`, and people with WorkExecute.
  - **Receiving:** only the serving agent, checked by clankrd against the workload token.
- **Request/reply.** `services.call(name, payload, timeout)`:
  - **Sending:** the request carries `replyTo = reply:<id>` and expires at the call's timeout, so a request nobody took in time is dead-lettered and never delivered late.
  - **Waiting:** clankrd waits on the reply queue. On timeout it answers `OutcomeUnknown`, because a request already being handled may still finish.
  - **Late replies:** a reply that arrives after the caller gave up sits unread and is dead-lettered at the service's `retentionSeconds`, so it never resurrects the request.
  - **One reply per request:** a handler retried after replying doesn't reply twice.
- **Topics.** `publish(topic, payload)` puts a copy into the mailbox of every `EventSubscription {topic, mailbox}` in the namespace.
  - **Catching up:** subscribers that were down find their messages waiting.
  - **Who can do what:** publishing needs `spec.publishes`, and creating a subscription is a resource write, so an agent can't subscribe itself to topics it wasn't given.
  - **Duplicates:** a dedup key makes a republish a no-op per subscription.
- **Handlers instead of polling.** The Python and TypeScript SDKs have `app.service(name, fn)` and `app.subscribe(mailbox, fn)`. A long-polling loop leases one message at a time.
  - **Success:** the return value is the reply, and the message is acked.
  - **`Permanent`:** an error reply, so the caller doesn't wait for the timeout.
  - **Any other error:** a nack, so the message is redelivered, and dead-lettered at `maxDeliveries`.
- **Concurrent calls.** The node now brokers an agent's calls concurrently. A long service call or long-poll no longer holds up that agent's task heartbeats or its other calls.
- **Agent ABI 1.2** adds the `serviceCall`, `serviceReply` and `publish` ops.

**Consequences.**
- Only same-namespace addressing: tenant boundaries hold by construction.
- **Delivery model:** these are queues, not exactly-once. Handlers must stay idempotent, and replies are deduplicated per request.
- Not yet:
  - ordering partitions within a topic;
  - per-sender quotas and backpressure beyond the 256 KiB size cap (CL-30-08);
  - typed message schemas (CL-30-09);
  - scope and budget passing on delegation (CL-30-06);
  - the Go SDK's handler loop (it can call and publish through the mailbox and service HTTP API, but has no `service`/`subscribe` helpers yet).
