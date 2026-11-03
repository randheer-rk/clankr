# 0032 — Envelope encryption and KMS

Status: accepted (2026-11-03)

**Context.** Until now one master key encrypted every secret value directly. That meant two things: rotating the key rewrote every row, and the key had to sit on the control plane's disk, where anyone who could read the disk could read every secret.

**Decision.** Secrets use envelope encryption.
- **Data keys:** each secret version is encrypted (AES-256-GCM, bound to `namespace/name/version`) with its own random 256-bit data key.
- **Wrapping:** the data key is stored wrapped by a key-encryption key, recorded per row in `kek`:
  - `local:<fingerprint>`: the master key, as before.
  - `vault:<key>`: HashiCorp Vault's Transit engine. Start clankrd with `--kms vault+https://vault:8200/<mount>/<key>`, with `$VAULT_TOKEN` set. The key never leaves Vault; clankrd only sends data keys to be wrapped or unwrapped.
- **Rows from before this change** have no data key. They still decrypt with the master key, and are upgraded to envelopes the next time keys are rewrapped.
- **Rewrapping:** `clankrd rewrap-secrets` rewraps every data key with the current key-encryption key, in one transaction. That's how secrets move onto a KMS, or follow a Vault key-version rotation. `clankrd rotate-master-key` now rewraps data keys too, instead of re-encrypting values.

**Consequences.**
- Rotation touches only the small wrapped keys, and with Vault, Vault's own key rotation plus `rewrap-secrets` is enough.
- With a KMS, reading the control plane's disk no longer reveals any secret. Secrets become unreadable whenever Vault is unreachable, and those reads fail rather than fall back to something weaker.
- The master key still exists with a KMS: it derives the workload-identity signing key.
- Only Vault Transit is supported for now. AWS KMS or GCP KMS would implement the same wrap/unwrap pair.
