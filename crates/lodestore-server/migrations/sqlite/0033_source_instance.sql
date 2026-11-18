ALTER TABLE jobs ADD COLUMN source_ref TEXT NOT NULL DEFAULT '';
ALTER TABLE jobs ADD COLUMN source_meta_json TEXT;
ALTER TABLE snapshots ADD COLUMN source_type TEXT NOT NULL DEFAULT 'files';
ALTER TABLE snapshots ADD COLUMN source_ref TEXT NOT NULL DEFAULT '';
ALTER TABLE snapshots ADD COLUMN source_label TEXT;
