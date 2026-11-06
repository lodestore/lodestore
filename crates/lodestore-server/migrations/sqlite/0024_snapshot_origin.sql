ALTER TABLE snapshots ADD COLUMN origin TEXT;

UPDATE snapshots SET origin = (
    SELECT CASE WHEN j.stdin_name IS NOT NULL THEN 'stdin' ELSE j.origin END
    FROM jobs j WHERE j.id = snapshots.job_id
) WHERE job_id IS NOT NULL;
