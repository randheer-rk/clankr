-- Local persistent volumes and node maintenance (PRD §29, §23).

-- A claim binds to the node that first runs an instance using it; local-path data never moves,
-- so every later instance using the claim is pinned to that node (CL-23-10, CL-29-06).
create table volume_bindings (
    namespace  text not null,
    claim      text not null,
    node       text not null references nodes,
    bound_at   timestamptz not null default now(),
    primary key (namespace, claim)
);

-- cordoned: no new placements. draining: also move instances off (except volume-pinned ones).
alter table nodes add column cordoned boolean not null default false;
alter table nodes add column draining boolean not null default false;

-- Why an instance is where it is (e.g. "drain blocked: local volume ns/claim"), for operators.
alter table instances add column note text;
