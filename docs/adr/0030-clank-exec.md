# 0030 — `clank exec`

Status: accepted (2026-11-03)

**Context.** Operators need a shell inside a running agent's sandbox for debugging, without SSH to nodes. Nodes reach the control plane, but it doesn't reach them: they're behind NAT, and only their mTLS client is trusted.

**Decision.** clankrd brokers a session between the user and the instance's node. Each side holds two plain HTTP streams; there are no WebSockets, so the node's existing mTLS client is unchanged.
- **Start:** `POST /v1/namespaces/{ns}/instances/{id}/exec {command, tty, rows, cols}` returns `{session}`.
  - Needs `InstancesExec`, which only admin and cluster-admin hold.
  - Only `oci` runtimes. A process-runtime agent is a host process, so a "shell into it" would be a host shell.
  - Every exec is audited before it starts: actor, instance, and the command line.
- **Rendezvous:** the node's next heartbeat lists the session. The node then opens `GET …/nodes/{name}/exec/{session}/input` and `POST …/output`, and the user's `clank` opens the mirror pair. Only the creating actor and the instance's node can join a session. A session that no node claims within 30s is dropped.
- **Bytes:** kennel's exec frames, `[kind u8, len u32 BE, data]`, which clankrd relays without parsing.
  - Down: `1` stdin, `3` end of stdin. The end of stdin is framed because Firecracker vsock has no half-close.
  - Up: `1` output, `2` exit status.
- **On the node:** `kennel exec <instance> [-t] -- cmd`, on a pty for `-t` or on pipes otherwise. It runs as the agent's user, in its namespaces, through kennel-init's exec service in all three runtime classes.
- **CLI:** `clank exec [-n ns] TARGET [-t] [-- cmd]`. TARGET is an instance id, or a deployment or agent name (its first ready instance). The command defaults to `sh`, and `-t` defaults to on when stdin and stdout are terminals (raw mode). The exit status is passed through.

**Consequences.**
- Sessions live in one clankrd replica's memory. With several replicas behind a load balancer, every leg must reach the same replica (sticky routing), or the session fails.
- Window resizes after the start aren't propagated.
- Output is relayed but not recorded; the audit row records who ran what.
