-- Distributed runtime (PRD §23, M3).

-- Leader leases with fencing epochs (CL-23-05, CL-36-03). The epoch bumps on every change of
-- holder; fenced writers lock this row and check holder + epoch + expiry in the same txn.
create table leader_leases (
    name       text primary key,
    holder     text   not null,
    epoch      bigint not null,
    expires_at timestamptz not null
);

-- Worker nodes (CL-23-01). Credentials are stored hashed; capacity is what the node offers.
create table nodes (
    name            text primary key,
    credential_hash text  not null,
    labels          jsonb not null default '{}',
    capacity        jsonb not null,            -- {memoryMiB, cpus, runtimes: [...]}
    ready           boolean not null default false,
    last_heartbeat  timestamptz,
    registered_at   timestamptz not null default now()
);

-- Agent instances placed by the scheduler. `epoch` bumps on every (re)assignment so a node
-- that lost an instance (e.g. after a partition) can tell and stop it.
create table instances (
    id          text primary key,
    namespace   text   not null,
    deployment  text   not null,
    agent       text   not null,
    node        text   references nodes on delete set null,
    epoch       bigint not null default 1,
    memory_mib  bigint not null,
    cpus        double precision not null,
    created_at  timestamptz not null default now(),
    updated_at  timestamptz not null default now()
);
create index instances_node on instances (node);
create index instances_deployment on instances (namespace, deployment);
