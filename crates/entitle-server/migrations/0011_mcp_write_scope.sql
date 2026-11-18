-- The granted OAuth scope: `mcp` (read) or `mcp mcp:write`.
ALTER TABLE mcp_codes ADD COLUMN scope text NOT NULL DEFAULT 'mcp';
ALTER TABLE mcp_tokens ADD COLUMN scope text NOT NULL DEFAULT 'mcp';
