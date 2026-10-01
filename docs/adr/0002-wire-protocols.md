# 0002 — Wire protocols

Status: accepted (2026-11-01)

- **Public management API:** REST/JSON over HTTP (axum), with SSE for watch and streams. The event id is the cluster revision, and an expired cursor returns an `error` event with `HistoryExpired`.
- **Agent ↔ node:** length-prefixed JSON frames over a per-instance Unix socket (`/run/clankr/agent.sock`) carrying ABI major/minor. Defined in M1.
- **Node ↔ control plane:** HTTP long-poll / SSE at M3.
- gRPC/protobuf only once a measured need exists (frame size, streaming fan-out). The frame schema lives in `clankr-types`, so switching the encoding later doesn't change the contract.
