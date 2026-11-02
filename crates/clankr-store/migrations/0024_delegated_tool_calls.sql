-- Container tools run on the caller's node (ADR 0028): the gateway records who took the call,
-- when, and how long it may take, so a node that never reports back is settled by a sweep.
alter table tool_calls
    add column delegated_to text,
    add column delegated_at timestamptz,
    add column timeout_s int,
    add column unsafe_effects boolean;
