# clankr.dev/sdk (Go)

The agent SDK for Go, speaking agent runtime ABI v1.1 to the local clankr node. It uses the standard library only.

```go
app := clankr.New()
app.Task("research", func(ctx context.Context, t *clankr.Task, c *clankr.Client) (any, error) {
    mem := c.Context("memory")
    prev, err := mem.Get(ctx, "progress")                 // nil, nil if missing
    if err != nil { return nil, err }
    reply, err := c.Generate(ctx, "default", []clankr.Message{{Role: "user", Content: "hi"}}, nil)
    if err != nil { return nil, err }
    ifv := int64(0)
    if prev != nil { ifv = prev.Version }
    if _, err := mem.Put(ctx, "progress", map[string]any{"summary": reply.Text}, &ifv); err != nil { return nil, err }
    return map[string]any{"summary": reply.Text}, nil
})
app.OnHibernate(func(ctx context.Context, c *clankr.Client) (any, error) { return map[string]any{"cursor": 42}, nil })
app.Run()
```

These behave the same as the Python SDK:
- Returning `clankr.Permanent(err)` fails the task without a retry. Any other error fails it retryably.
- `c.CallTool` derives a stable idempotency key from the task carried in `ctx`.
- Errors from the node are `*clankr.Error`, with a stable `Code`.
