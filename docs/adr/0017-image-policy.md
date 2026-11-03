# 0017 — ImagePolicy and signature enforcement

Status: accepted (2026-11-02)

## The policy

`kind: ImagePolicy` (namespaced; every policy in a namespace applies) has these fields:
- `allowedRepositories`: image repository prefixes. Empty means any.
- `requireSignature` and `keys`: cosign-format signature from a trusted ECDSA P-256 public key (kennel ADR 0005): the classic `.sig` tag, or cosign v3's default sigstore bundle attached as an OCI referrer.
- `allowProcessRuntime`: the unsandboxed S0 runtime is refused wherever any policy exists, unless every policy allows it.

## Enforcement (two layers)

1. **API, when an Agent is saved:** wrong repository or process runtime returns 403 with the policy's name. The API can't check signatures, because the control plane may not reach the registry.
2. **Node, at every launch:** the node fetches its namespace's policies (nodes may `GET imagepolicies`, since they hold only public keys) and re-applies them.
   - Signed images launch as `kennel run --verify-key <trusted.pub>…`, which pulls the signature from the registry and refuses on mismatch.
   - Because it's checked at launch, an Agent saved before a stricter policy (or edited directly in the database) still doesn't run. Tested in `crates/clankr-node/tests/image_policy_e2e.rs`.
   - A refused launch counts as "not ready", so a rolling restart onto an unsigned image stalls after one instance instead of replacing the fleet.

**Not yet:**
- SBOM and vulnerability gates
- keyless signing
- a promotion gate that requires a signature before a stage accepts a digest (the node enforces it at launch either way)
- a cached verification result per digest (each launch re-verifies)
