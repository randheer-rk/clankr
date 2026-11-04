# 0036 — Inbound webhooks

Status: accepted (2026-11-04)

**Context.** EventTriggers (ADR 0034) start workflows from topics, but only agents and API clients could publish. The systems people want to react to, GitHub, Stripe, Linear, Sentry, their own services, speak webhooks.

**Decision.** A `Webhook` resource and one unauthenticated route, `POST /hooks/{ns}/{name}`, whose authentication is the delivery's signature.

```yaml
kind: Webhook
spec:
  topic: github.push
  secretRef: {name: gh, key: secret}
  signature: github        # github | stripe | generic
  header: X-Signature-256  # generic: where the hex HMAC-SHA256 travels
  prefix: "sha256="        # generic: text before the digest
  dedupHeader: X-Delivery  # generic: the delivery id, if the sender has one
  maxBodyBytes: 262144
```

- **Verification.** GitHub: `X-Hub-Signature-256: sha256=<hex>` over the body. Stripe: `Stripe-Signature: t=…,v1=…` over `<t>.<body>`, refused when `t` is more than five minutes off. Generic: `<header>: <prefix><hex>` over the body. All constant-time, all HMAC-SHA256 with the secret in `secretRef`, which is read under the actor `webhook/<ns>/<name>` (so every read is audited) and never returned.
- **Delivery.** A good delivery is published to `topic` with sender `webhook/<name>` and payload `{body, headers, webhook}`: the body as JSON when it parses, else as text; the headers limited to `content-type`, `user-agent` and `x-*` minus anything named like a signature or token. The dedup key is the sender's delivery id (`X-GitHub-Delivery`, Stripe's event `id`, `dedupHeader`) or the body's hash, so a retried delivery is delivered and triggers once.
- **Refusals.** 401 for a bad or missing signature and for an unknown hook (indistinguishable from outside), 422 over `maxBodyBytes`. Refusals are audit denials under the same budget as anonymous API denials, so a flood can't fill the audit log.

**Consequences.**
- No bearer tokens on the route, so the signature is the whole trust: rotate the secret by writing the Secret; the next delivery uses it.
- Not yet: `/hooks` paths behind a separate listener or hostname, IP allowlists, asymmetric (Ed25519) signatures as used by some senders, and replay windows for schemes without a timestamp (GitHub's dedup is by delivery id only).
