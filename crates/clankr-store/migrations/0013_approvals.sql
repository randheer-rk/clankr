-- Approval inbox (HUM-01/02, SEC-17). A row per approval node visit, written in the same
-- transaction as the WaitStarted event; deciding it and resuming the run is one transaction too.
alter table workflow_runs add column started_by text;

create table approvals (
    id             uuid primary key,
    namespace      text  not null,
    run_id         uuid  not null references workflow_runs,
    node           text  not null,
    visit          int   not null,
    approver_group text  not null,
    requested_by   text,                -- who started the run; may not approve it
    subject        jsonb not null,      -- run data when the approval was requested
    digest         text  not null,      -- sha256 over run, node, visit, subject: what the approver signs off on
    state          text  not null default 'pending',  -- pending | approved | rejected | expired | cancelled
    deadline       timestamptz,
    decided_by     text,
    decided_at     timestamptz,
    comment        text,
    created_at     timestamptz not null default now(),
    unique (run_id, node, visit)
);
create index approvals_ns_state on approvals (namespace, state, created_at);
create index approvals_deadline on approvals (deadline) where state = 'pending';
