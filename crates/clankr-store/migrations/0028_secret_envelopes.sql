-- Envelope encryption: each secret version is encrypted with its own data key, stored wrapped
-- by a key-encryption key (`kek`: "local:<fingerprint>" or "vault:<key>"). Rows from before this
-- have no data key and stay encrypted directly with the master key until rewrapped.
alter table secrets add column dek bytea, add column kek text;
