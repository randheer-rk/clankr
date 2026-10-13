-- SEC-09: secrets encrypted at rest (AES-256-GCM, master key held outside the database).
-- Values never pass through `resources`, so they can't leak via watch, list or the change log.
create table secrets (
    namespace  text   not null,
    name       text   not null,
    version    bigint not null,
    keys       text[] not null,    -- key names only, for display
    nonce      bytea  not null,
    ciphertext bytea  not null,    -- AES-GCM over the JSON map; AAD = namespace/name/version
    updated_at timestamptz not null default now(),
    primary key (namespace, name)
);

-- Token budgets per ModelClaim (CL-27-06): reserve before the call, reconcile after.
-- scope is 'total' or a run id.
create table model_usage (
    namespace text   not null,
    claim     text   not null,
    scope     text   not null,
    used      bigint not null default 0,
    reserved  bigint not null default 0,
    primary key (namespace, claim, scope)
);

-- One row per model call, for cost attribution and debugging (CL-27-05, PRV-07). No prompt content.
create table model_calls (
    id            bigserial primary key,
    at            timestamptz not null default now(),
    namespace     text   not null,
    claim         text   not null,
    connection    text   not null,
    provider      text   not null,
    model         text   not null,
    run_id        uuid,
    task          text,
    actor         text   not null,
    input_tokens  bigint not null,
    output_tokens bigint not null,
    latency_ms    bigint not null,
    outcome       text   not null,  -- ok | error:<class>
    stop_reason   text
);
create index model_calls_claim on model_calls (namespace, claim, at);
