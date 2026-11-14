# Product

<!-- impeccable:product-schema 1 -->

## Platform

web

## Users

Hands-on engineers, of two kinds, engineers first (confirmed by the owner, 2026-11-14):

- Platform and infrastructure engineers who will self-host Clankr and run many agents in production. They judge durability, sandboxing, key handling and ops burden.
- Agent developers who write agents with the Python, TypeScript or Go SDK and want somewhere reliable to run them.

There is no executive or buyer audience.

## Product Purpose

Clankr is an open-source orchestration engine for AI agents, written in Rust. You describe agents, models, tools and workflows as resources and `clank apply` them. Clankr runs each agent in a sandbox on one of your machines, records every workflow step in Postgres, and stands between agents and everything they touch.

## Positioning

One binary on Postgres. No Kubernetes, no Temporal. `clankrd` is the whole control plane (API, scheduler, workflow engine, gateway, inbox, web UI); Postgres is the source of truth and the message bus.

## Operating Context

- Linux hosts; nodes dial in over mTLS and can sit behind NAT.
- `clankr-node` runs agents under kennel in a rootless container, gVisor, or a Firecracker microVM.
- The `clank` CLI: `apply`, `run start`, `approve`, `audit-check`.
- Runs are event logs: inspect a run after any event, fork from it.

## Capabilities and Constraints

- Durable workflows: a step commits with the work it schedules; a lost node's run resumes from the last committed event, and no model is called twice.
- Brokered credentials: agents name a claim; the gateway holds the key, reserves budget before a call and settles after.
- Human approvals: an approval-gated tool parks the run in an inbox; the decision goes in a hash-chained audit log.
- SDKs: Python, TypeScript, Go.
- Version 0.1.0, early: APIs and schemas will still change. Apache-2.0.

## Brand Commitments

- Name: Clankr.
- Mark: Lockbot, a padlock with two eyes (`site/public/clankr-mark.svg`, `clankr-mark-animated.svg`). The mark's shape and its animation (eyes look around, blink, follow the pointer) stay as they are in any redesign (owner, 2026-11-14).
- Landing-page palette and fonts come from the palletism.art packs Pixel Arcade and Pop Grain (owner, 2026-11-14).
- Voice: plain and concrete. No metaphors that need explaining: the owner rejected "agents that work shifts, not demos" as meaningless (2026-11-14).
- Landing-page copy is for people, not specialists: plain words, no jargon, no implementation detail. Do not name Postgres or other internals there. Say what problem it solves. "Kubernetes and an AI agent harness combined" is the owner's description of what the project is; it guides the copy but is not to be printed on the page (owner, 2026-11-14).

## Evidence on Hand

- Real copy, event log and CLI snippets in `site/src/App.tsx`.
- Three Remotion films in `site/src/films/` (durable work, agent isolation, human approval).
- README and ADRs in `docs/adr`.
- No customers, testimonials, benchmarks or pricing exist. Do not invent any.

## Product Principles

1. Show the log, not the claim: every promise maps to an event a reader can see.
2. Fewer moving parts beat more features.
3. Agents are never trusted with keys or irreversible actions.
4. An engineer should be running it the same afternoon.
