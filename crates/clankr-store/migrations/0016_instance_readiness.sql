-- Readiness (rolling restarts): nodes report which instances completed the agent handshake.
-- A report only counts for the epoch it names, so a restart (epoch bump) makes the instance
-- unready until its node says otherwise.
alter table instances add column ready_epoch bigint;
alter table instances add column ready_since timestamptz;
