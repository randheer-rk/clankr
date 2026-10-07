-- Append-only, hash-chained audit trail (SEC-08, CL-31-10, CL-33-06).
-- Each row's hash = sha256(prev hash || canonical row JSON); `audit_head` serializes appends.
create table audit_head (
    id   boolean primary key default true check (id),
    seq  bigint not null,
    hash text   not null
);
insert into audit_head values (true, 0, repeat('0', 64));

create table audit_events (
    seq        bigint primary key,
    at         timestamptz not null,
    actor      text not null,
    request_id text not null,
    action     text not null,           -- create | update | update_status | delete | authenticate | ...
    kind       text,
    namespace  text,
    name       text,
    uid        text,
    decision   text not null check (decision in ('allow', 'deny')),
    reason     text,
    rev        bigint,                  -- resource revision written, for allows
    prev_hash  text not null,
    hash       text not null
);

-- ponytail: DB-level append-only guard; a DB superuser can still drop it. External export (CL-33-06) closes that gap later.
create function audit_events_immutable() returns trigger language plpgsql as $$
begin
    raise exception 'audit_events is append-only';
end $$;
create trigger audit_events_no_update before update or delete on audit_events
    for each statement execute function audit_events_immutable();
