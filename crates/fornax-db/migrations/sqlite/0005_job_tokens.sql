-- Copyright 2026 Manu P Anand
-- Licensed under the Apache License, Version 2.0
-- See LICENSE for details.

-- Per-attempt job tokens (D-018): a short-lived credential the runner uses for the git
-- checkout of exactly one repository. Only the SHA-256 of the token is stored. A token is
-- valid while its attempt is `running` and `expires_at >= now`; it is deleted when the
-- attempt finishes and pruned by the lease reaper.

CREATE TABLE job_tokens (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    attempt_id  INTEGER NOT NULL UNIQUE REFERENCES job_attempts(id) ON DELETE CASCADE,
    token_hash  TEXT    NOT NULL UNIQUE,
    repo_id     INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    expires_at  INTEGER NOT NULL
);

-- Pruning scan.
CREATE INDEX job_tokens_expiry ON job_tokens (expires_at);
