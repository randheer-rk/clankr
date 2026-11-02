-- Runs this cluster started on another (multi-cluster placement); status mirrored on read.
create table remote_runs (
    remote_run_id   uuid primary key,
    namespace       text not null,
    workflow        text not null,
    cluster         text not null,
    region          text not null,
    idempotency_key text not null,
    started_by      text not null,
    status          text not null,
    created_at      timestamptz not null default now(),
    updated_at      timestamptz not null default now(),
    unique (namespace, idempotency_key)
);
