-- Signed identity: a public-key caller proves an identified app user id with a
-- JWT minted by the developer's backend with this (sealed) per-project secret.
ALTER TABLE projects
    ADD COLUMN sealed_identity_secret bytea,
    ADD COLUMN require_signed_identity boolean NOT NULL DEFAULT false;
