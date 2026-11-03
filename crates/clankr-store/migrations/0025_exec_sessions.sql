-- clank exec sessions (ADR 0030), shared by every clankrd replica: the user's and the node's
-- connections may land on different replicas, and bytes cross between them by LISTEN/NOTIFY.
create table exec_sessions (
    id uuid primary key,
    namespace text not null,
    instance text not null,
    node text not null,
    actor text not null,
    request jsonb not null,
    created_at timestamptz not null default now(),
    -- the user's output listener is subscribed: the node may start
    user_listening boolean not null default false,
    -- the node's input listener is subscribed: the user's input may flow
    claimed boolean not null default false
);
create index exec_sessions_pending on exec_sessions (node) where not claimed;
