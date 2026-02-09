-- Add configurable timeout to invariants (default 5 minutes).
ALTER TABLE invariants ADD COLUMN timeout_secs INTEGER NOT NULL DEFAULT 300;
