-- Model cost accounting (CL-27-06, MOD-013): the price applied at call time is recorded with the
-- call, so later price changes don't rewrite history. Cost budgets live in model_usage in
-- micro-USD (claim = 'cost:<claim>' or 'budget-cost:<AgentBudget>').
alter table model_calls add column cost_micro_usd bigint;
create index model_calls_ns_at on model_calls (namespace, at);
