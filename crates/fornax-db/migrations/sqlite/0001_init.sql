-- Copyright 2026 Manu P Anand
-- Licensed under the Apache License, Version 2.0
-- See LICENSE for details.

-- Fornax M1 schema: users, access tokens, repositories, audit log.
-- Names keep their display case in `*name` and a lowercase copy in `lower_*` for
-- case-insensitive uniqueness that also works on Postgres later.

CREATE TABLE users (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    username    TEXT    NOT NULL,
    lower_name  TEXT    NOT NULL UNIQUE,
    is_admin    INTEGER NOT NULL DEFAULT 0,
    created_at  INTEGER NOT NULL
);

CREATE TABLE access_tokens (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id      INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name         TEXT    NOT NULL,
    token_hash   TEXT    NOT NULL UNIQUE,
    created_at   INTEGER NOT NULL,
    last_used_at INTEGER
);

CREATE TABLE repositories (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    owner_id       INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name           TEXT    NOT NULL,
    lower_name     TEXT    NOT NULL,
    description    TEXT    NOT NULL DEFAULT '',
    is_private     INTEGER NOT NULL DEFAULT 0,
    default_branch TEXT    NOT NULL DEFAULT 'main',
    created_at     INTEGER NOT NULL,
    updated_at     INTEGER NOT NULL,
    UNIQUE (owner_id, lower_name)
);

-- Append-only: the application never updates or deletes rows here.
CREATE TABLE audit_events (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    actor_id   INTEGER REFERENCES users(id) ON DELETE SET NULL,
    action     TEXT    NOT NULL,
    target     TEXT    NOT NULL,
    detail     TEXT    NOT NULL DEFAULT '{}',
    created_at INTEGER NOT NULL
);

CREATE INDEX audit_events_created_at ON audit_events (created_at);
