# 0004 — Native image builder

Status: superseded by 0007 (2026-11-01)

**Decision.** `clankr-build` implements Clankfile builds itself, with no buildah and no Docker. The pipeline: pull the base via `oci-client` into a content-addressed blob store → unpack layers → per step, a cache key over (parent chain, instruction, input digests, platform) → `COPY`/`SKILL` written directly into the upper dir, `RUN` executed in the libcontainer sandbox over a rootless overlay → deterministic tar (sorted, `SOURCE_DATE_EPOCH` mtimes, normalized ownership) → OCI image layout + provenance.

**Scope of v1 (M1).** Single stage, host architecture only. Supports `FROM@digest`, `COPY`, `RUN`, `WORKDIR`, `ENV`, `USER`, `LABEL`, `SKILL`, `AGENT_ENTRYPOINT`, `REQUIRE_RUNTIME_ABI`. Multi-stage builds come in M2, cross-arch in M4. SBOM is our own package-db reader (M2).

**Why.** Full control over secret handling, per-tenant cache isolation, provenance and the ABI descriptor (BLD-01/09/11, CL-24-*). It reuses the ADR 0003 sandbox.
