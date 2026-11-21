ALTER TABLE repositories ADD COLUMN guarantee_tick_at BIGINT;
ALTER TABLE repositories ADD COLUMN guarantee_unchecked_runs BIGINT NOT NULL DEFAULT 0;
ALTER TABLE repositories ADD COLUMN guarantee_failed_runs BIGINT NOT NULL DEFAULT 0;
ALTER TABLE repositories ADD COLUMN access_generation BIGINT NOT NULL DEFAULT 0;
