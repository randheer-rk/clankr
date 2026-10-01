# 0005 — Declarative YAML graphs first

Status: accepted (2026-11-01)

**Decision.** M1 workflows are declarative graphs (PRD §7.2, CL-21-01). Run state = `fold(events)` over the persisted history, so replay is a pure function and doesn't need a Temporal-style deterministic-code sandbox. A code-first DSL (DEV-017) comes later and compiles to the same graph.

**Consequence.** Model and tool calls are always activities whose results are recorded. Replay never calls them (CL-21-07) by construction.
