# 0026 — Node.js and browser agent base images

Status: accepted (2026-11-02)

**Decision.** There are two new base images next to `clankr/agent-python`, both built with kennel from `sdk/typescript`:

- **`clankr/agent-node`** (`Kennelfile`): `node:24-slim` pinned by digest, plus the TypeScript SDK.
  - The SDK is importable as `@clankr/sdk` from anywhere through `/node_modules`, because ESM ignores NODE_PATH. Node runs the `.ts` sources directly.
  - The image runs as nobody, and a smoke test imports the SDK inside the sandbox.
  - Agents derive `FROM clankr/agent-node@sha256:…` and set `AGENT_ENTRYPOINT ["node", "/app/agent.ts"]`.
- **`clankr/agent-browser`** (`Kennelfile.browser`): the same, plus Debian's Chromium and `@clankr/sdk/browser`.
  - `@clankr/sdk/browser` is a dependency-free DevTools-protocol driver (launch, newPage, goto, evaluate, screenshot) over Node's built-in WebSocket.
  - Chromium's own sandbox is off, since kennel is the sandbox.
  - A smoke test renders a page inside the sandbox at build time.
  - The apt layer follows Debian's current packages: rebuild to pick up Chromium fixes. The resolved version is recorded in `/opt/clankr/chromium-version`.

**kennel fixes these needed** (kennel `4c32a48`, `072549f`):
- A relative `--root` broke `RUN`.
- A container's own network namespace had loopback down. A `createContainer` hook now brings it up, without giving the workload any capability.
- Build `RUN` steps can now chown and override permissions among mapped ids, so package installs work.
- After each step, files owned by other container ids are handed back to root, so rootless layer snapshots can read them.

**Boundary.** Agents have no network. A browser agent can render local or `data:` content, but browsing the internet needs an egress path (an allowlisting proxy through the node), which doesn't exist yet.

**Tests.** `oci_node_e2e` runs a TypeScript agent from `clankr/agent-node` under kennel and checks it's read-only, has no network, runs as nobody on Node 24, and that models reach it through the node. The browser image verifies itself at build time.
