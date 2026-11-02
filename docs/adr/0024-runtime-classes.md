# 0024 — Runtime classes (gVisor, Firecracker)

Status: accepted (2026-11-02)

**Decision.** An `oci` Agent runtime takes `class: container | gvisor | microvm` (default `container`). This maps onto `kennel run --runtime`:

- `container`: libcontainer namespaces, seccomp and cgroups on the node's kernel (S1).
- `gvisor`: `runsc --rootless`, so syscalls go to a user-space kernel (S2).
- `microvm`: a Firecracker VM per instance, with its own guest kernel and no network device (S3). kennel builds a cached read-only ext4 root disk per image, and `kennel-init --vm` is PID 1. See kennel's README.

**Scheduling.** The class becomes the scheduling kind (`oci`, `oci/gvisor`, `oci/microvm`). Nodes advertise what they can run (`clankr-node --runtimes oci,oci/microvm`), and the scheduler only places an agent where its class is offered.

**Policy.** `ImagePolicy.runtimeClasses` lists the classes a namespace allows. For example, `[microvm]` for tenants running untrusted code. It's enforced when an Agent is saved, at node launch, and for sandboxed tools (`container.runtime` on `sandbox`/`mcpStdio` tools).

**Status of verification on the dev host.**
- `container` is covered end to end.
- `gvisor` is implemented, but `runsc` isn't installed, so its test skips.
- `microvm`'s disk build and init injection were checked by hand, and its error paths are tested. The boot test runs once `KENNEL_VM_KERNEL` points at a kernel. The host's `/boot/vmlinuz` is root-only, so it needs one `sudo install` (kennel README).

**Limits.**
- `gvisor` refuses resource limits, because rootless runsc can't enforce them.
- `microvm` has no volumes, stdin or host networking yet, so agents (which need the node socket) can't use it until a vsock bridge exists. Today it serves sandboxed tools and batch work.
