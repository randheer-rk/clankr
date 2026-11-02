-- Autoscaling state per deployment (owned by the scheduler leader): the replica count it
-- currently runs and since when demand has been below it (for scale-down stabilization).
create table autoscale_state (
    namespace  text   not null,
    deployment text   not null,
    replicas   bigint not null,
    low_since  timestamptz,
    updated_at timestamptz not null default now(),
    primary key (namespace, deployment)
);
