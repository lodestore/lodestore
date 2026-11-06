ALTER TABLE snapshots ADD COLUMN verified_at BIGINT;

ALTER TABLE snapshots ADD COLUMN verified_by TEXT;

ALTER TABLE repositories ADD COLUMN provenance_backfilled_at BIGINT;
