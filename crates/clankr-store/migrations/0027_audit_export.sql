-- Off-site audit export (CL-33-06): how far each destination has been sent. The exported copy
-- is the evidence; this cursor only saves re-sending (a reset cursor just re-sends, by seq).
create table audit_exports (
    dest text primary key,
    seq  bigint not null default 0,
    hash text   not null default repeat('0', 64)
);
