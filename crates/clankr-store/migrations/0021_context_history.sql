-- Every context write, append-only (value null = deleted), so context can be read as of any
-- moment: named snapshots, and what a run saw at a point in its history.
create table context_history (
    seq       bigserial primary key,
    namespace text   not null,
    claim     text   not null,
    key       text   not null,
    version   bigint not null,
    value     jsonb,
    writer    text   not null,
    at        timestamptz not null default clock_timestamp()
);
create index context_history_key on context_history (namespace, claim, key, seq desc);
create index context_history_at on context_history (namespace, claim, at);

-- a trigger covers every write path, present and future
create function context_history_record() returns trigger language plpgsql as $$
begin
    if tg_op = 'DELETE' then
        insert into context_history (namespace, claim, key, version, value, writer)
        values (old.namespace, old.claim, old.key, old.version + 1, null, 'deleted');
        return old;
    end if;
    insert into context_history (namespace, claim, key, version, value, writer)
    values (new.namespace, new.claim, new.key, new.version, new.value, new.writer);
    return new;
end $$;
create trigger context_history after insert or update or delete on context_objects
    for each row execute function context_history_record();

-- what's there now is the starting point
insert into context_history (namespace, claim, key, version, value, writer, at)
select namespace, claim, key, version, value, writer, updated_at from context_objects order by updated_at;

create table context_snapshots (
    namespace  text   not null,
    claim      text   not null,
    name       text   not null,
    seq        bigint not null,   -- history position the snapshot covers (inclusive)
    created_by text   not null,
    created_at timestamptz not null default now(),
    primary key (namespace, claim, name)
);

-- forks: where a run's history was copied from
alter table workflow_runs add column forked_from uuid, add column forked_at bigint;
