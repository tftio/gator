-- Phase 5: per-task harness preference and plan-level isolation mode.

ALTER TABLE plans ADD COLUMN default_harness TEXT NOT NULL DEFAULT 'claude-code';
ALTER TABLE plans ADD COLUMN isolation TEXT NOT NULL DEFAULT 'worktree'
    CHECK (isolation IN ('worktree', 'container'));
ALTER TABLE tasks ADD COLUMN requested_harness TEXT;
