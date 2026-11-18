-- New projects grant from production purchases only: sandbox purchases (TestFlight,
-- license testers, Creem test mode) are free. Existing projects keep their setting.
ALTER TABLE projects ALTER COLUMN sandbox_grants_entitlements SET DEFAULT false;
