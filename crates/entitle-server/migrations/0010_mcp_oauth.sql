-- OAuth 2.1 authorization server for the MCP endpoint (`ENTITLE_PUBLIC_URL`).

-- Dynamically registered clients (RFC 7591). `client_id` is public; the
-- registered `redirect_uris` (checked against an allowlist) are the boundary.
CREATE TABLE mcp_clients (
    client_id     text PRIMARY KEY,
    client_name   text,
    redirect_uris text[] NOT NULL,
    created_at    timestamptz NOT NULL DEFAULT now()
);

-- Single-use authorization codes, deleted on exchange; hashed like tokens.
CREATE TABLE mcp_codes (
    code_hash      bytea PRIMARY KEY,
    client_id      text NOT NULL REFERENCES mcp_clients ON DELETE CASCADE,
    redirect_uri   text NOT NULL,
    code_challenge text NOT NULL,
    expires_at     timestamptz NOT NULL
);

-- Access tokens. The hash is keyed with the admin token, so rotating
-- ENTITLE_ADMIN_TOKEN revokes every token.
CREATE TABLE mcp_tokens (
    token_hash bytea PRIMARY KEY,
    client_id  text NOT NULL REFERENCES mcp_clients ON DELETE CASCADE,
    created_at timestamptz NOT NULL DEFAULT now()
);
