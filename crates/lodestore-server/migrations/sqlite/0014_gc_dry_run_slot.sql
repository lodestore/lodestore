DROP INDEX gc_cycles_active_idx;

CREATE UNIQUE INDEX gc_cycles_active_idx ON gc_cycles (repository_id, dry_run)
    WHERE state NOT IN ('done', 'failed', 'aborted');
