-- Spec rollout: the Agent generation each instance was started from. When the Agent changes
-- (e.g. a promotion moves its image), the scheduler bumps the instance's epoch so its node
-- restarts it on the new spec; the old epoch is fenced like any other move.
alter table instances add column agent_generation bigint not null default 0;

-- Image promotion history (BLD-17): which digest each stage of an AgentImage runs, who moved
-- it there, why, and on what evidence. The current digest of a stage is its newest row.
create table image_promotions (
    id              bigserial primary key,
    namespace       text not null,
    image           text not null,
    stage           text not null,
    digest          text not null,
    previous_digest text,
    promoted_by     text not null,
    at              timestamptz not null default now(),
    reason          text not null default '',
    evidence        jsonb,
    rollback        boolean not null default false
);
create index image_promotions_stage on image_promotions (namespace, image, stage, id desc);
