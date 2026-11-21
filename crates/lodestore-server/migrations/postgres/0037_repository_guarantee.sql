ALTER TABLE repositories ADD COLUMN guarantee_level TEXT NOT NULL DEFAULT 'unchecked';
ALTER TABLE repositories ADD COLUMN guarantee_accepted TEXT NOT NULL DEFAULT 'unchecked';
ALTER TABLE repositories ADD COLUMN guarantee_notes_json TEXT NOT NULL DEFAULT '[]';
ALTER TABLE repositories ADD COLUMN guarantee_checked_at BIGINT;

UPDATE repositories SET guarantee_accepted = CASE
    WHEN backend = 'local' THEN 'host_can_overwrite_or_delete'
    WHEN agent_key_state = 'verified' THEN 'create_only_enforced'
    WHEN agent_key_state IN ('can_delete', 'can_overwrite') THEN 'host_can_overwrite_or_delete'
    ELSE 'unchecked'
END;
