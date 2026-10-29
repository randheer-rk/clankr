-- AgentMailbox: durable at-least-once messages (CL-30-02..05, CL-30-08).
create table mailbox_messages (
    id             uuid primary key,
    namespace      text   not null,
    mailbox        text   not null,
    seq            bigserial,
    sender         text   not null,   -- authenticated principal, never taken from the payload (CL-30-10)
    correlation_id text,
    dedup_key      text,
    payload        jsonb  not null,
    state          text   not null check (state in ('ready', 'leased', 'acked', 'dead')),
    not_before     timestamptz not null default now(),
    lease_epoch    bigint not null default 0,
    lease_owner    text,
    lease_expires  timestamptz,
    deliveries     int    not null default 0,
    max_deliveries int    not null,
    expires_at     timestamptz not null,
    dead_reason    text,
    created_at     timestamptz not null default now(),
    unique (namespace, mailbox, dedup_key)
);
create index mailbox_ready on mailbox_messages (namespace, mailbox, seq) where state = 'ready';
create index mailbox_leased on mailbox_messages (lease_expires) where state = 'leased';
