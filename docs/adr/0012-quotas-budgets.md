# 0012 — Quotas and budgets

Status: accepted (2026-11-01)

## ResourceQuota (namespaced)

`spec.hard` caps:
- `cpus`, `memoryMiB` and `instances`: the sum over AgentDeployments of replicas × the Agent's resources
- `volumeMiB`: the sum of VolumeClaim capacity
- `activeRuns`: running workflow runs
- `count/<plural>`: the number of objects of a kind

If several quotas exist in a namespace, the strictest limit for each key applies.

**Admission:**
- **Atomicity:** a create or update that feeds a limited key takes a per-namespace Postgres advisory lock held in a transaction. It then re-reads the quotas, recomputes usage with the new object in place, and refuses with `QuotaExceeded` (HTTP 429). The lock serializes racing writers across clankrd replicas, and dropping it on cancellation releases it.
- **Increases only:** only writes that raise usage are refused. A namespace already over a lowered quota can still shrink.
- **Dry runs** are checked too.
- **Run starts** check `activeRuns` under the same lock. A retried start whose idempotency key already has a run returns that run, even when the namespace is at the limit.

## AgentBudget (namespaced)

- **Fields:** `period` (`day`, `month` or `total`; calendar UTC) plus `maxTokens` and/or `maxCostUSD`. Every budget in a namespace applies to every model call there.
- **Reservation:** the gateway reserves the estimated tokens and cost against each budget's current window before dispatch, in the same atomic transaction as the claim's own limits. Rows are locked in a fixed order, so concurrent calls can't overshoot or deadlock. After the call, the reservation is replaced by actual usage.
- **ModelClaim limits:** `budget` gains `maxCostUSDPerRun` and `maxCostUSDTotal`.

## Pricing

- **Where:** `ProviderConnection.spec.pricing[model] = {inputUSDPerMTok, outputUSDPerMTok}`.
- **Recording:** each call records the cost at the price applied then (`model_calls.cost_micro_usd`), so a later price change doesn't rewrite history.
- **Units:** budgets are counted in integer micro-USD. A price in USD per million tokens is numerically micro-USD per token.
- **Fail closed:** a cost limit on a model with no price refuses the call.

## Reporting

- **API:** `GET /v1/namespaces/{ns}/usage` returns quota use against limits, each budget's current window (used plus reserved), and per-claim spend this month.
- **CLI:** `clank usage` renders it. The web UI slice will reuse the same endpoint for the dashboard.

**Known limit:** a provider that returns more output tokens than the requested `max_tokens` is charged what it reports, so that one call can overshoot. Later calls are then refused.

**Not yet:**
- budget alerts and thresholds (§34.9)
- CPU-time and tool budgets: there is no tool gateway yet
- per-queue tenant fairness (CL-22-09)
