-- Cluster-wide revision counter. Every resource write bumps it inside the write's transaction;
-- the row lock serializes writers so commit order == revision order and watch cursors never skip.
-- ponytail: one global write lock on resource mutations; shard per namespace if control-plane write rate ever needs it.
create table revision (
    id        boolean primary key default true check (id),
    rev       bigint not null,
    compacted bigint not null  -- changes with rev <= compacted are gone; older cursors get HistoryExpired
);
insert into revision values (true, 0, 0);

-- Current state. `object` is the full serialized Resource; key columns are copies for lookup/constraints.
create table resources (
    uid       uuid primary key,
    namespace text   not null,
    kind      text   not null,
    name      text   not null,
    rv        bigint not null,
    object    jsonb  not null,
    unique (namespace, kind, name)
);
create index resources_kind on resources (kind, namespace);

-- Change log for watch resume (CL-19-04). Compacted by `Store::compact`.
create table resource_changes (
    rev       bigint primary key,
    namespace text  not null,
    kind      text  not null,
    change    text  not null check (change in ('Added', 'Modified', 'Deleted')),
    object    jsonb not null
);
