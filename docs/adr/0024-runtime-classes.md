# 0024 — Runtime classes (gVisor, Firecracker)

Status: accepted (2026-11-02)

**Decision.** An `oci` Agent runtime takes `class: container | gvisor | microvm` (default `container`). This maps onto `kennel run --runtime`:

- `container`: libcontainer namespaces, seccomp and cgroups on the node's kernel (S1).
- `gvisor`: `runsc --rootless`, so syscalls go to a user-space kernel (S2).
- `microvm`: a Firecracker VM per instance, with its own guest kernel and no network device (S3). kennel builds a cached read-only ext4 root disk per image, and `kennel-init --vm` is PID 1. See kennel's README.

**Scheduling.** The class becomes the scheduling kind (`oci`, `oci/gvisor`, `oci/microvm`). Nodes advertise what they can run (`clankr-node --runtimes oci,oci/microvm`), and the scheduler only places an agent where its class is offered.

**Policy.** `ImagePolicy.runtimeClasses` lists the classes a namespace allows. For example, `[microvm]` for tenants running untrusted code. It's enforced when an Agent is saved, at node launch, and for sandboxed tools (`container.runtime` on `sandbox`/`mcpStdio` tools).

**Verified on the dev host (2026-11-03).** The same TypeScript agent runs end to end under all three classes (`oci_node_e2e`): model calls through the node, read-only root, no network, running as nobody, and egress only to listed hosts.

- **gVisor:** `runsc` runs with its own user namespace and `--host-uds=open` for the node socket. `--memory/--cpus` cap the whole sandbox through a `systemd-run --user` scope. On Ubuntu it needs kennel's `runsc-local` AppArmor profile.
- **microVM:** Firecracker boots the host's kernel in about 1.2 s. The node socket and the egress socket reach the guest over vsock (`kennel run --socket`); kennel puts the host's vsock modules on the disk, and init loads them. stdin reaches the workload over vsock, so container tools run in VMs too. Each volume is a persistent ext4 disk, the claim directory's `.kennel-volume.ext4`. For direct kennel use, VMs also support an interactive terminal (a pty relayed over vsock) and `--network host` (rootless NAT through pasta). Clankr agents use neither: they get egress through the node's allowlisting proxy.
