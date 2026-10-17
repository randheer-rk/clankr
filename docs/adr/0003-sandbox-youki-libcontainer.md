# 0003 — Sandbox via embedded youki `libcontainer`

Status: superseded by 0007 (2026-11-01)

**Decision.** `clankr-node` embeds `libcontainer` (youki) and drives it with OCI runtime specs built through `oci-spec`. There's no external runtime binary. The builder runs `RUN` steps through the same launcher (`clankr_node::run_to_exit`).

**Why.** Pure Rust, in-process, and rootless with user namespaces, seccomp, cgroup v2, caps drop and no-new-privs. One sandbox code path for build and runtime halves the escape-testing surface.

**Risks.** libcontainer is less battle-tested than crun/runc. Mitigation: the sandbox escape suite (PRD §32) runs against it in CI from M2. Containers aren't a hostile multi-tenant boundary (PRD §13.4); the S3/VM tier is separate future work.

**Host requirement.** See ADR 0006 (AppArmor on Ubuntu 24.04+).
