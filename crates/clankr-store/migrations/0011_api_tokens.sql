-- ServiceAccount API tokens (SEC-02). Only a SHA-256 of the token is stored; the plaintext is
-- returned once at creation. Deleting the ServiceAccount disables its tokens (checked at auth).
create table api_tokens (
    id text primary key,
    namespace text not null,
    service_account text not null,
    token_hash text not null unique,
    description text not null default '',
    created_by text not null,
    created_at timestamptz not null default now(),
    expires_at timestamptz,
    revoked_at timestamptz
);
create index api_tokens_owner on api_tokens (namespace, service_account);
