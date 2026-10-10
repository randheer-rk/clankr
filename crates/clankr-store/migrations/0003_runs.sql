-- Durable workflow execution (PRD §21-22, §36). One transaction per step writes the new events,
-- the run's folded state and the task/timer rows those events imply, so a task row is its own
-- outbox entry (CL-21-10, CL-36-01).

create table workflow_runs (
    run_id              uuid primary key,
    namespace           text   not null,
    workflow            text   not null,
    workflow_generation bigint not null,
    graph               jsonb  not null,  -- pinned copy; later Workflow edits don't affect this run (CL-21-08)
    graph_digest        text   not null,
    status              text   not null,
    seq                 bigint not null,  -- last committed event
    state               jsonb  not null,  -- replay(events), cached
    idempotency_key     text,
    created_at          timestamptz not null default now(),
    updated_at          timestamptz not null default now(),
    unique (namespace, idempotency_key)
);
create index workflow_runs_ns on workflow_runs (namespace, workflow, created_at);

create table workflow_events (
    run_id uuid   not null references workflow_runs,
    seq    bigint not null,
    at     timestamptz not null,
    event  jsonb  not null,
    primary key (run_id, seq)       -- unique, gap-free per run (CL-36-02)
);

create table tasks (
    run_id        uuid   not null references workflow_runs,
    task          text   not null,       -- deterministic: node#visit.attempt
    namespace     text   not null,
    node          text   not null,
    attempt       int    not null,
    queue         text   not null,
    state         text   not null check (state in ('ready', 'leased', 'completed', 'failed', 'cancelled')),
    not_before    timestamptz not null,
    lease_epoch   bigint not null default 0,  -- fencing token; bumped on every lease (CL-22-02)
    lease_owner   text,
    lease_expires timestamptz,
    deliveries    int    not null default 0,
    created_at    timestamptz not null default now(),
    finished_at   timestamptz,
    primary key (run_id, task)
);
create index tasks_dispatch on tasks (namespace, queue, not_before) where state = 'ready';
create index tasks_expiry on tasks (lease_expires) where state = 'leased';

create table timers (
    run_id uuid not null references workflow_runs,
    timer  text not null,
    due_at timestamptz not null,
    fired  boolean not null default false,
    primary key (run_id, timer)
);
create index timers_due on timers (due_at) where not fired;
