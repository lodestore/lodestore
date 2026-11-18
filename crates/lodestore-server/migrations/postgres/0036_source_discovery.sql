ALTER TABLE agents ADD COLUMN sources_json TEXT;
ALTER TABLE agents ADD COLUMN sources_collected_at BIGINT;
CREATE TABLE discovered_sources (
    host_id TEXT NOT NULL,
    source_ref TEXT NOT NULL,
    PRIMARY KEY (host_id, source_ref)
);
CREATE INDEX discovered_sources_ref ON discovered_sources (source_ref);
ALTER TABLE source_instances_gone ADD COLUMN returned_at BIGINT;
