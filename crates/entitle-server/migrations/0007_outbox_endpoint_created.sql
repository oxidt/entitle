-- Admin delivery log: newest deliveries of a project's endpoints.
CREATE INDEX outbox_endpoint_created ON outbox (endpoint_id, created_at DESC);
