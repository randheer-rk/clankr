-- Tool gateway (CL-28-06/09/10): one row per tool call. The idempotency key is claimed before
-- execution, so a retry gets the recorded outcome (or "unknown") instead of a second effect;
-- an approval can be spent by one call only. Outputs are kept for replay.
create table tool_calls (
    id              bigserial primary key,
    at              timestamptz not null default now(),
    namespace       text not null,
    claim           text not null,
    tool            text not null,
    tool_version    text not null,
    method          text not null,
    idempotency_key text not null,
    actor           text not null,
    run_id          uuid,
    task            text,
    approval_id     uuid,
    input_sha256    text not null,
    state           text not null,          -- started | done | unknown (failed calls are removed so they can be retried)
    output          jsonb,
    output_sha256   text,
    truncated       boolean not null default false,
    duration_ms     bigint,
    error           text,
    unique (namespace, claim, idempotency_key)
);
create unique index tool_calls_approval on tool_calls (approval_id) where approval_id is not null;
create index tool_calls_run on tool_calls (namespace, run_id);
