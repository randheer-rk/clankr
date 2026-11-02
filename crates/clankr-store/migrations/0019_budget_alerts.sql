-- Budget alerts (§34.9): each threshold fires once per budget window and metric.
create table budget_alerts (
    id          bigserial primary key,
    at          timestamptz not null default now(),
    namespace   text   not null,
    budget      text   not null,
    window_key  text   not null,
    metric      text   not null,      -- tokens | costUSD
    threshold   int    not null,      -- percent
    used        double precision not null,
    limit_value double precision not null,
    unique (namespace, budget, window_key, metric, threshold)
);
