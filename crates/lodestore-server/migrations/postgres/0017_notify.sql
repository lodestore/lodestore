CREATE TABLE notification_channels (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    kind TEXT NOT NULL,
    source TEXT NOT NULL,
    enabled BIGINT NOT NULL,
    types_json TEXT NOT NULL,
    config_json TEXT NOT NULL,
    secret_id TEXT REFERENCES secrets (id),
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL,
    last_ok BIGINT,
    last_at BIGINT,
    last_error TEXT
);

CREATE TABLE alert_deliveries (
    id TEXT PRIMARY KEY,
    event_id TEXT NOT NULL REFERENCES alert_events (id) ON DELETE CASCADE,
    channel_id TEXT NOT NULL REFERENCES notification_channels (id) ON DELETE CASCADE,
    state TEXT NOT NULL,
    attempts BIGINT NOT NULL DEFAULT 0,
    next_attempt_at BIGINT NOT NULL,
    last_error TEXT,
    created_at BIGINT NOT NULL,
    sent_at BIGINT,
    UNIQUE (event_id, channel_id)
);

CREATE INDEX alert_deliveries_due_idx ON alert_deliveries (state, next_attempt_at);
