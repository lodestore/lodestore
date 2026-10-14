CREATE TABLE settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE admin_users (
    id TEXT PRIMARY KEY,
    username TEXT NOT NULL UNIQUE,
    password_hash TEXT NOT NULL,
    created_at BIGINT NOT NULL
);

CREATE TABLE sessions (
    id_hash BYTEA PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES admin_users (id),
    csrf_token TEXT NOT NULL,
    created_at BIGINT NOT NULL,
    expires_at BIGINT NOT NULL,
    last_seen_at BIGINT NOT NULL
);

CREATE TABLE secrets (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    format BIGINT NOT NULL,
    nonce BYTEA NOT NULL,
    ciphertext BYTEA NOT NULL,
    created_at BIGINT NOT NULL
);

CREATE TABLE pki (
    id TEXT PRIMARY KEY,
    ca_cert_pem TEXT NOT NULL,
    ca_key_secret_id TEXT NOT NULL REFERENCES secrets (id),
    ca_sha256 TEXT NOT NULL,
    grpc_cert_pem TEXT NOT NULL,
    grpc_key_secret_id TEXT NOT NULL REFERENCES secrets (id),
    grpc_sans TEXT NOT NULL,
    grpc_not_after BIGINT NOT NULL
);

CREATE TABLE repositories (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    backend TEXT NOT NULL,
    location_json TEXT NOT NULL,
    experimental BIGINT NOT NULL DEFAULT 0,
    format_repo_id BYTEA NOT NULL,
    config_hash BYTEA NOT NULL,
    master_secret_id TEXT NOT NULL REFERENCES secrets (id),
    server_slot_secret_id TEXT REFERENCES secrets (id),
    server_creds_secret_id TEXT REFERENCES secrets (id),
    agent_creds_secret_id TEXT REFERENCES secrets (id),
    agent_key_state TEXT NOT NULL DEFAULT 'unchecked',
    checks_json TEXT,
    checked_at BIGINT,
    created_at BIGINT NOT NULL
);

CREATE TABLE join_tokens (
    id TEXT PRIMARY KEY,
    token_hash BYTEA NOT NULL UNIQUE,
    repository_id TEXT NOT NULL REFERENCES repositories (id),
    labels_json TEXT NOT NULL,
    created_at BIGINT NOT NULL,
    expires_at BIGINT NOT NULL,
    used_at BIGINT,
    used_by_agent_id TEXT
);

CREATE TABLE agents (
    id TEXT PRIMARY KEY,
    hostname TEXT NOT NULL,
    repository_id TEXT NOT NULL REFERENCES repositories (id),
    labels_json TEXT NOT NULL,
    os TEXT NOT NULL,
    arch TEXT NOT NULL,
    version TEXT NOT NULL,
    distro TEXT,
    kernel TEXT,
    status TEXT NOT NULL,
    enrolled_at BIGINT NOT NULL,
    revoked_at BIGINT,
    cert_serial TEXT NOT NULL,
    cert_issued_at BIGINT NOT NULL,
    cert_not_after BIGINT NOT NULL,
    last_seen_at BIGINT,
    facts_json TEXT
);

CREATE INDEX agents_repository_idx ON agents (repository_id);
CREATE INDEX sessions_expires_idx ON sessions (expires_at);
