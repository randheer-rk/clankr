-- Certificate rotation overlap: after a renewal the previous certificate stays valid briefly so
-- requests in flight on pooled connections don't fail. Re-join clears it (immediate revocation).
alter table nodes add column prev_credential_hash text;
alter table nodes add column prev_valid_until timestamptz;
