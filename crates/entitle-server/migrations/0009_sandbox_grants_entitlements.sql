-- Off for production projects: sandbox (TestFlight, license tester) purchases are
-- free, so they are still recorded but grant nothing.
ALTER TABLE projects ADD COLUMN sandbox_grants_entitlements boolean NOT NULL DEFAULT true;
