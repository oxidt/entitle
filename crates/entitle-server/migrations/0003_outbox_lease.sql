-- The worker claims due rows by setting a lease and commits before sending, so no
-- row locks are held during HTTP calls. A crashed worker's lease simply expires.
ALTER TABLE outbox ADD COLUMN locked_until timestamptz;
