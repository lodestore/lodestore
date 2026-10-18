ALTER TABLE snapshots ADD COLUMN suspicious BIGINT NOT NULL DEFAULT 0;
ALTER TABLE snapshots ADD COLUMN suspicious_by TEXT;
ALTER TABLE snapshots ADD COLUMN suspicious_reason TEXT;
ALTER TABLE snapshots ADD COLUMN anomaly_json TEXT;
ALTER TABLE snapshots ADD COLUMN suspicious_at BIGINT;
ALTER TABLE snapshots ADD COLUMN suspicious_cleared_by TEXT;
ALTER TABLE snapshots ADD COLUMN suspicious_cleared_reason TEXT;
ALTER TABLE snapshots ADD COLUMN suspicious_cleared_at BIGINT;

ALTER TABLE jobs ADD COLUMN retention_json TEXT;
