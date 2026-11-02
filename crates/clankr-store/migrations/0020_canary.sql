-- The exact Agent spec each instance runs, so a canary can keep the rest of a deployment on
-- the previous release and roll back to it.
alter table instances add column agent_spec jsonb;

-- One row per (deployment, Agent generation) rolled out with a canary.
create table rollouts (
    namespace         text   not null,
    deployment        text   not null,
    generation        bigint not null,
    stable_generation bigint not null,
    stable_spec       jsonb,
    phase             text   not null check (phase in ('Canary', 'Promoted', 'Failed')),
    reason            text,
    started_at        timestamptz not null default now(),
    updated_at        timestamptz not null default now(),
    primary key (namespace, deployment, generation)
);
