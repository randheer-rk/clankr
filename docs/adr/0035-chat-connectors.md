# 0035 — Chat connectors: Telegram and Slack

Status: accepted (2026-11-04)

**Context.** Agents could talk to each other (ADR 0033) and people could reach them over the REST API, but not from where people actually are: a chat. Approvals waited in an inbox nobody was looking at.

**Decision.** A `ChatConnector` resource and a controller in clankrd, built on services and approvals so nothing new has to be made durable or audited.

```yaml
kind: ChatConnector
spec:
  kind: telegram            # or slack
  tokenRef: {name: tg, key: token}
  appTokenRef: {name: slack, key: app}   # Slack only: the xapp- token for Socket Mode
  service: assistant        # inbound messages become requests to this AgentService
  allowedChats: ["42"]      # chat ids (Telegram) or channel ids (Slack); empty = nobody
  replyTimeoutSeconds: 120
  approvals: {chat: "42", group: editors, users: ["7"]}
```

- **Inbound.** A message from an allowed chat is a `services.call` as sender `<platform>:<chat>` with payload `{text, chat, from, platform, messageId}` and dedup key `<platform>:<chat>:<messageId>`, so a redelivered update never sends twice. The request is in the queue before the platform is told we've seen it (Telegram's offset, Slack's envelope ack). The reply, `"text"` or `{text}`, goes back to the chat; past the timeout the chat is told the request is still queued.
- **Refusals.** A chat not in `allowedChats` gets a one-line refusal and an audit denial under `<platform>:<chat>`. `allowedChats` is empty by default because bots are public.
- **Approval cards.** Pending approvals for `approvals.group` are posted to `approvals.chat` with Approve/Reject buttons. A press by a user in `approvals.users` is decided through `Engine::decide_approval` as actor `<platform>:<user>` with the connector's group, never break-glass; anyone else is told so and audited. The card is edited to the outcome. Which cards were posted is in the connector's status, so a restart posts none twice.
- **Outbound.** Tool executor `chat {connector}` with one method, `send {chat, text}`. The token is brokered by the gateway; risk and approval gating apply like any tool.
- **Transport.** Telegram: `getUpdates` long-polling; Slack: Socket Mode over a websocket. Neither needs a public URL. One replica runs connectors (leader lease `chat`), one task per connector, restarted on a new spec generation and after any failure (status carries `connected` and `lastError`).

**Consequences.**
- Tokens live in secrets and are read under the connector's actor, so every read is in the audit log.
- Slack messages from bots (`bot_id`) and with a `subtype` are ignored, so a connector can't talk to itself.
- Not yet: threads (replies go to the channel, not a thread), file attachments, Discord and WhatsApp (same shape, different poll loop), slash commands, and mapping chat users to Clankr principals beyond the explicit `approvals.users` list.
