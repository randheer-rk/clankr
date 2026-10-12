-- ContextClaim key/value memory with optimistic concurrency (CL-29-07/08, CL-30-07).
-- Independent of workflow history: context writes never participate in run transactions (CL-36-04).
create table context_objects (
    namespace  text   not null,
    claim      text   not null,
    key        text   not null,
    version    bigint not null,
    value      jsonb  not null,
    writer     text   not null,   -- provenance: who wrote this version
    updated_at timestamptz not null default now(),
    primary key (namespace, claim, key)
);
