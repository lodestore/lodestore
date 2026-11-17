UPDATE snapshots SET policy_name = NULL, stdin_name = NULL
WHERE job_id IS NOT NULL AND EXISTS (
    SELECT 1 FROM jobs j WHERE j.id = snapshots.job_id AND (
        (j.policy_name IS NULL) <> (snapshots.policy_name IS NULL)
        OR j.policy_name <> snapshots.policy_name
        OR (j.stdin_name IS NULL) <> (snapshots.stdin_name IS NULL)
        OR j.stdin_name <> snapshots.stdin_name
        OR snapshots.agent_id_claim IS NULL OR snapshots.agent_id_claim <> snapshots.host_id
    )
);

UPDATE snapshots SET origin = 'stdin'
WHERE job_id IS NOT NULL AND (origin IS NULL OR origin <> 'stdin') AND EXISTS (
    SELECT 1 FROM jobs j WHERE j.id = snapshots.job_id AND j.source = 'cli'
);
