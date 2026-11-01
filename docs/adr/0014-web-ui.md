# 0014 — Web UI

Status: accepted (2026-11-01)

## Design

The direction is "Vision": a macOS-style app with sidebar, list and detail panes. It was chosen from four rounds of mockups, which are kept locally and not in the repo.

Colour means state:

| Colour | Meaning |
|---|---|
| blue | action or running |
| orange | needs a person |
| green | done |
| red | failed |

Fonts are Inter (self-hosted). Icons are Lucide.

## Stack

- `ui/` is Vite + React + TypeScript, with no component library and hand-written CSS from the signed-off mockup.
- `npm run build` writes `ui/dist`.
- clankrd embeds that folder with rust-embed (release builds; debug builds read from disk) and serves it for every path outside `/v1`. Unknown `/v1` paths stay JSON 404s.
- Static files need no authentication. Every API call carries the user's bearer token.

## Screens

- **Inbox:** approvals waiting for you, waiting for others, and already decided. The detail shows the run data being approved and where the run is. Decisions are digest-bound.
- **Runs:** grouped as Needs you / Running / Today / Earlier. The detail has the live flow graph (from the run's pinned graph), activity, budget, history, data and cost. Runs can be cancelled.
- **Agents and Workflows:** the spec, and starting a run with JSON input.
- **Budgets:** month-to-date spend with forecast and budget line, budget cards, quota limits, and spend per model claim.

Data is polled every 3–10 s and paused while the tab is hidden.

## API additions

- `RunView.graph` and `RunView.startedBy`
- `GET runs/{id}/spend`
- `usage.spendByDay`

## Sign-in

You paste a token: a ServiceAccount token, the ID token from `clank login`, or the owner token. It is kept in localStorage.

**Not yet:**
- browser OIDC (authorization code + PKCE)
- people & access and audit pages
- live push instead of polling
- small-screen layout
