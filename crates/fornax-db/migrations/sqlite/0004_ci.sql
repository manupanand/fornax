-- Copyright 2026 Manu P Anand
-- Licensed under the Apache License, Version 2.0
-- See LICENSE for details.

-- CI queue (C-004, D-007, D-014): runners, pipelines, jobs with a `needs` DAG, one row per
-- execution attempt with a lease, and chunked attempt logs.
-- Portable SQL (SQLite now, Postgres later). Times are Unix seconds. Status values are the
-- `as_str` forms of fornax_core::ci. `ref` and `offset` are SQL keywords, hence `git_ref`
-- and `byte_offset`.

CREATE TABLE runners (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    name         TEXT    NOT NULL,
    token_hash   TEXT    NOT NULL UNIQUE,
    labels       TEXT    NOT NULL DEFAULT '[]',          -- JSON array of strings
    trusted_only INTEGER NOT NULL DEFAULT 0,             -- host runners: trusted jobs only (D-008)
    disabled     INTEGER NOT NULL DEFAULT 0,
    created_at   INTEGER NOT NULL,
    last_seen_at INTEGER
);

CREATE TABLE pipelines (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    repo_id          INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    number           INTEGER NOT NULL,                   -- per-repository sequence from 1
    file             TEXT    NOT NULL,                   -- e.g. .fornax/ci.yaml
    name             TEXT    NOT NULL,
    git_ref          TEXT    NOT NULL,
    sha              TEXT    NOT NULL,
    event            TEXT    NOT NULL,
    trusted          INTEGER NOT NULL DEFAULT 0,
    status           TEXT    NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'running', 'success', 'failed', 'canceled')),
    cancel_requested INTEGER NOT NULL DEFAULT 0,
    triggered_by     INTEGER REFERENCES users(id) ON DELETE SET NULL,
    created_at       INTEGER NOT NULL,
    started_at       INTEGER,
    finished_at      INTEGER,
    UNIQUE (repo_id, number)
);

CREATE TABLE jobs (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    pipeline_id      INTEGER NOT NULL REFERENCES pipelines(id) ON DELETE CASCADE,
    name             TEXT    NOT NULL,
    status           TEXT    NOT NULL
        CHECK (status IN ('waiting', 'queued', 'running', 'success', 'failed', 'canceled', 'skipped')),
    waiting_reason   TEXT
        CHECK (waiting_reason IN ('dependency', 'approval', 'resource', 'no_runner')),
    labels           TEXT    NOT NULL DEFAULT '[]',      -- JSON array of required labels
    payload          TEXT    NOT NULL DEFAULT '{}',      -- resolved job (JSON), opaque here
    trusted          INTEGER NOT NULL DEFAULT 0,         -- copied from the pipeline
    max_attempts     INTEGER NOT NULL DEFAULT 1 CHECK (max_attempts >= 1),
    attempts         INTEGER NOT NULL DEFAULT 0,
    cancel_requested INTEGER NOT NULL DEFAULT 0,
    created_at       INTEGER NOT NULL,
    queued_at        INTEGER,
    started_at       INTEGER,
    finished_at      INTEGER,
    UNIQUE (pipeline_id, name)
);

-- Claim scan: oldest queued first.
CREATE INDEX jobs_queue ON jobs (status, queued_at, id);

CREATE TABLE job_needs (
    job_id       INTEGER NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
    needs_job_id INTEGER NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
    PRIMARY KEY (job_id, needs_job_id)
);

CREATE INDEX job_needs_reverse ON job_needs (needs_job_id);

CREATE TABLE job_attempts (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    job_id           INTEGER NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
    number           INTEGER NOT NULL,                   -- 1-based per job
    runner_id        INTEGER REFERENCES runners(id) ON DELETE SET NULL,
    status           TEXT    NOT NULL
        CHECK (status IN ('running', 'success', 'failed', 'lost', 'canceled')),
    lease_expires_at INTEGER NOT NULL,
    started_at       INTEGER NOT NULL,
    finished_at      INTEGER,
    exit_code        INTEGER,
    error            TEXT,
    UNIQUE (job_id, number)
);

-- Lease reaper scan.
CREATE INDEX job_attempts_lease ON job_attempts (status, lease_expires_at);

CREATE TABLE job_logs (
    attempt_id  INTEGER NOT NULL REFERENCES job_attempts(id) ON DELETE CASCADE,
    byte_offset INTEGER NOT NULL,
    data        BLOB    NOT NULL,
    PRIMARY KEY (attempt_id, byte_offset)
);
