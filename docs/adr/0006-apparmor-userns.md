# 0006 — AppArmor profile for rootless sandboxes

Status: superseded by 0007 (2026-11-01)

**Context.** Ubuntu 24.04+ sets `kernel.apparmor_restrict_unprivileged_userns=1`. Any unprofiled binary that creates a user namespace moves into the `unprivileged_userns` profile and loses `CAP_SYS_ADMIN` inside it. libcontainer's intermediate process then fails with `EPERM` (seen in the kernel log as `apparmor="DENIED" ... profile="unprivileged_userns" capname="sys_admin"`).

**Decision.** Ship an AppArmor profile that grants `userns` to the Clankr binaries, the same shape as Ubuntu's stock `/etc/apparmor.d/crun`.
- Dev: `deploy/apparmor/kennel-dev` in the kennel repo covers its cargo target dir (sandboxing moved there, ADR 0007); `runsc-local` does the same for gVisor.
- Production: the package installs a profile for `/usr/bin/clankrd`. Node preflight checks the sysctl and the profile and fails with this explanation (CL-37-07).

We do **not** recommend turning the sysctl off host-wide.
