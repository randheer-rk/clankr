-- CL-22-07/08: tasks with unsafe side effects don't get blindly redelivered after a lost lease.
alter table tasks add column side_effects text not null default 'idempotent'
    check (side_effects in ('idempotent', 'unsafe'));
alter table tasks drop constraint tasks_state_check;
alter table tasks add constraint tasks_state_check
    check (state in ('ready', 'leased', 'completed', 'failed', 'cancelled', 'unknown'));
