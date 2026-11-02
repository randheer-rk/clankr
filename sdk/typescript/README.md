# @clankr/sdk (TypeScript)

The agent SDK for Node.js, speaking agent runtime ABI v1.1 to the local clankr node. It has no dependencies, and Node 22.18+ runs the `.ts` sources directly (type stripping), so there's no build step.

```ts
import { AgentApp, Permanent } from "@clankr/sdk"

const app = new AgentApp()

app.task("research", async (task, ctx) => {
  const mem = ctx.context("memory")
  const prev = await mem.get("progress")                       // null if missing
  const reply = await ctx.models.generate("default", [{ role: "user", content: task.input.input.topic }])
  await mem.put("progress", { summary: reply.text }, { ifVersion: prev?.version ?? 0 })
  return { summary: reply.text }
})

app.onHibernate(async () => ({ cursor: 42 }))                    // comes back as app.checkpoint
app.run()
```

These behave the same as the Python SDK:
- Throwing `Permanent` fails the task without a retry. Any other error fails it retryably.
- Delivery is at-least-once, so handlers must be idempotent.
- `ctx.tools.call` derives a stable idempotency key from the task, so a retried task gets the recorded result.
- The agent holds no credentials. Every call goes through `$CLANKR_SOCKET` to the node, which checks it against the Agent spec.
