-- Restart budgets (CL-20-04): why a node gave up on an instance, valid only for the epoch it
-- was reported at (a reassignment starts a new epoch, and with it a fresh budget).
alter table instances add column failure text, add column failure_epoch bigint;
