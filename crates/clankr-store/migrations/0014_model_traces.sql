-- Golden traces (CL-34-08): opt-in capture of model requests and responses on a
-- ProviderConnection with `record: true`, exported as a replay script for deterministic
-- evaluation. Unlike model_calls this stores prompt content, so it is off by default and
-- readable only with secret-level access.
create table model_traces (
    id             bigserial primary key,
    at             timestamptz not null default now(),
    namespace      text   not null,
    claim          text   not null,
    connection     text   not null,
    model          text   not null,
    run_id         uuid,
    task           text,
    prompt_sha256  text   not null,
    request        jsonb  not null,
    response       text   not null,
    input_tokens   bigint not null,
    output_tokens  bigint not null,
    stop_reason    text
);
create index model_traces_run on model_traces (namespace, run_id, id);
