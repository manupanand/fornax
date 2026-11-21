-- Copyright 2026 Manu P Anand
-- Licensed under the Apache License, Version 2.0
-- See LICENSE for details.

-- Attempt liveness (review finding C1, C4).
-- `job_attempts.seen_at`: first time the runner showed life for the attempt (Extend, Log,
-- Update, Done). An attempt whose lease expires while `seen_at` is still NULL was never
-- started by its runner (e.g. the `Next` reply was lost), so the reaper puts the job back
-- without consuming an attempt.
-- `jobs.free_requeues`: how often a job was put back that way (bounded, so a runner that
-- keeps dropping claims cannot loop a job forever).

ALTER TABLE job_attempts ADD COLUMN seen_at INTEGER;
ALTER TABLE jobs ADD COLUMN free_requeues INTEGER NOT NULL DEFAULT 0;

-- Existing attempts keep the old behaviour (treated as having shown life).
UPDATE job_attempts SET seen_at = COALESCE(finished_at, started_at);
