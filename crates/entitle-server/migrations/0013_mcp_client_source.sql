-- Where an MCP client registered from (client IP, IPv6 by /64), so unauthenticated
-- registrations without a token yet can be capped per source. NULL for older rows.
ALTER TABLE mcp_clients ADD COLUMN source text;
