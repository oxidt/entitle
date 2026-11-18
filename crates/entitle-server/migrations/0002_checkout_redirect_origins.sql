-- Origins (scheme://host[:port]) a checkout `success_url` may redirect to.
-- Empty = checkouts with a `success_url` are rejected.
ALTER TABLE projects ADD COLUMN checkout_redirect_origins text[] NOT NULL DEFAULT '{}';
