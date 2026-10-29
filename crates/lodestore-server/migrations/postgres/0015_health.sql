CREATE TABLE repo_usage (
    repository_id TEXT PRIMARY KEY REFERENCES repositories (id),
    physical_bytes BIGINT NOT NULL,
    logical_bytes BIGINT NOT NULL,
    packs BIGINT NOT NULL,
    objects BIGINT NOT NULL,
    counted_at BIGINT NOT NULL
);

CREATE TABLE alerts (
    id TEXT PRIMARY KEY,
    type TEXT NOT NULL,
    key TEXT NOT NULL,
    severity TEXT NOT NULL,
    state TEXT NOT NULL,
    host_id TEXT REFERENCES agents (id),
    repository_id TEXT REFERENCES repositories (id),
    policy_name TEXT,
    check_level TEXT,
    cert TEXT,
    title TEXT NOT NULL,
    detail TEXT NOT NULL,
    facts_json TEXT NOT NULL,
    started_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL,
    resolved_at BIGINT,
    notified_at BIGINT,
    resolved_notified_at BIGINT
);

CREATE UNIQUE INDEX alerts_firing_idx ON alerts (key) WHERE state = 'firing';
CREATE INDEX alerts_state_idx ON alerts (state, started_at);

CREATE TABLE alert_events (
    id TEXT PRIMARY KEY,
    alert_id TEXT NOT NULL REFERENCES alerts (id) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    at BIGINT NOT NULL,
    fanned_out_at BIGINT
);

CREATE INDEX alert_events_pending_idx ON alert_events (fanned_out_at, at);

CREATE TABLE alert_silences (
    host_id TEXT PRIMARY KEY REFERENCES agents (id),
    until_ms BIGINT,
    created_by TEXT NOT NULL,
    created_at BIGINT NOT NULL
);
