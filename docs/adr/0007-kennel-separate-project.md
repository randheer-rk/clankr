# 0007 — Image building and sandboxing live in a separate project: kennel

Status: accepted (2026-11-01). Supersedes 0003, 0004 and 0006 as far as Clankr is concerned.

**Decision.** Building, storing and running sandboxed agent containers is its own product, **kennel** (`~/Dev/rust/kennel`, own git repo, binary `kennel`). Clankr relates to kennel the way Kubernetes relates to a container runtime:

- **Contract:** OCI images (manifest, config, layers) plus kennel's `io.kennel.*` labels (`abi`, `skills`, `source.digest`).
- **Runtime interface:** the Clankr node runs, per agent instance:
  `kennel [--root R] run --network none --name <instance> -v <socket-dir>:/run/clankr -e CLANKR_SOCKET=/run/clankr/agent.sock -e CLANKR_AGENT=… -e CLANKR_NAMESPACE=… <image@sha256:…>`
  The node stops it by closing the session (Shutdown frame), then SIGKILL; `kennel-init`'s parent-death signal takes the container down with `kennel run`. There is no Rust crate dependency in either direction.

**Moved to kennel:** the recipe grammar (now `Kennelfile`), the builder, the OCI image store, the libcontainer launcher and the AppArmor profile. The youki/libcontainer, native-builder and AppArmor decisions (0003, 0004, 0006) now belong to kennel's own ADRs.

**Clankr keeps:** the node daemon (process runtime for dev, kennel for images), the agent ABI v1, admission of `Agent.spec.runtime.image` (must be pinned by digest).
