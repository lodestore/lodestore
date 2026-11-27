DROP INDEX jobs_coalesce_idx;

CREATE UNIQUE INDEX jobs_coalesce_idx ON jobs (host_id, policy_id, source_ref)
    WHERE state IN ('queued', 'assigned', 'running') AND policy_id IS NOT NULL;
