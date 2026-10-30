CREATE TABLE agent_rollouts (
    id TEXT PRIMARY KEY,
    target_version TEXT NOT NULL,
    state TEXT NOT NULL,
    canary BIGINT NOT NULL,
    wave_size BIGINT NOT NULL,
    pause_after BIGINT NOT NULL,
    pause_baseline BIGINT NOT NULL DEFAULT 0,
    created_by TEXT NOT NULL,
    created_at BIGINT NOT NULL,
    finished_at BIGINT,
    dismissed_at BIGINT,
    pause_reason TEXT,
    canary_waived BIGINT NOT NULL DEFAULT 0,
    resumed_at BIGINT
);

CREATE UNIQUE INDEX agent_rollouts_active_idx ON agent_rollouts ((1))
    WHERE state IN ('running', 'paused');

CREATE TABLE agent_updates (
    id TEXT PRIMARY KEY,
    rollout_id TEXT NOT NULL REFERENCES agent_rollouts (id) ON DELETE CASCADE,
    host_id TEXT NOT NULL REFERENCES agents (id) ON DELETE CASCADE,
    arch TEXT NOT NULL,
    from_version TEXT NOT NULL,
    target_version TEXT NOT NULL,
    wave BIGINT NOT NULL,
    state TEXT NOT NULL,
    reason TEXT,
    queued_at BIGINT NOT NULL,
    assigned_at BIGINT,
    switched_at BIGINT,
    progress_at BIGINT,
    finished_at BIGINT
);

CREATE UNIQUE INDEX agent_updates_active_idx ON agent_updates (host_id)
    WHERE state IN ('queued', 'assigned');

CREATE INDEX agent_updates_rollout_idx ON agent_updates (rollout_id, wave);

CREATE INDEX agent_updates_host_idx ON agent_updates (host_id, queued_at);

UPDATE notification_channels
SET types_json = REPLACE(types_json, ']', ',"agent_update_failed"]')
WHERE source = 'ui'
  AND types_json LIKE '%backup_failed%'
  AND types_json LIKE '%no_recent_backup%'
  AND types_json LIKE '%agent_offline%'
  AND types_json LIKE '%check_failed%'
  AND types_json LIKE '%certificate_expiring%';
