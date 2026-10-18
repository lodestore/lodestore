CREATE TABLE api_tokens (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    prefix TEXT NOT NULL,
    token_hash BYTEA NOT NULL UNIQUE,
    scopes_json TEXT NOT NULL,
    created_by TEXT NOT NULL,
    created_at BIGINT NOT NULL,
    expires_at BIGINT,
    last_used_at BIGINT,
    revoked_at BIGINT
);

CREATE INDEX api_tokens_created_idx ON api_tokens (created_at);
