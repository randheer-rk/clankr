-- Hibernation: an instance keeps a stable slot in its deployment; hibernating asks it for a
-- checkpoint, stored per slot, handed back to the slot's next instance on resume.
alter table instances
    add column slot int not null default 0,
    add column hibernate boolean not null default false,
    add column hibernate_since timestamptz,
    add column hibernated_at timestamptz,
    add column checkpoint jsonb,
    add column checkpoint_version bigint;

create table agent_checkpoints (
    namespace  text   not null,
    deployment text   not null,
    slot       int    not null,
    version    bigint not null,
    state      jsonb,
    supported  boolean not null,   -- false: the agent gave none (no false success on resume)
    instance   text   not null,
    at         timestamptz not null default now(),
    consumed   boolean not null default false,
    primary key (namespace, deployment, slot)
);

-- existing instances get distinct slots within their deployment
update instances i set slot = r.n - 1
from (select id, row_number() over (partition by namespace, deployment order by id) as n from instances) r
where i.id = r.id;
