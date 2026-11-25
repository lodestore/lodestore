ALTER TABLE repositories ADD COLUMN agent_sas_vending BIGINT NOT NULL DEFAULT 0;
ALTER TABLE repositories ADD COLUMN agent_access_changed_at BIGINT;
ALTER TABLE agents ADD COLUMN sas_expires_at BIGINT;
