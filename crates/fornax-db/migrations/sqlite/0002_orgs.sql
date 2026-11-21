-- Copyright 2026 Manu P Anand
-- Licensed under the Apache License, Version 2.0
-- See LICENSE for details.

-- Organisations, teams and repository collaborators (H-016).
-- Organisations are rows in `users` with kind = 'org' (one namespace for owners), so a
-- repository's owner_id can point at a user or an organisation.

ALTER TABLE users ADD COLUMN kind TEXT NOT NULL DEFAULT 'user' CHECK (kind IN ('user', 'org'));

CREATE TABLE org_members (
    org_id     INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    user_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    role       TEXT    NOT NULL CHECK (role IN ('owner', 'member')),
    created_at INTEGER NOT NULL,
    PRIMARY KEY (org_id, user_id)
);

CREATE TABLE teams (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    org_id     INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name       TEXT    NOT NULL,
    lower_name TEXT    NOT NULL,
    permission TEXT    NOT NULL CHECK (permission IN ('read', 'write', 'admin')),
    created_at INTEGER NOT NULL,
    UNIQUE (org_id, lower_name)
);

CREATE TABLE team_members (
    team_id INTEGER NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    PRIMARY KEY (team_id, user_id)
);

CREATE TABLE team_repos (
    team_id INTEGER NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    PRIMARY KEY (team_id, repo_id)
);

CREATE TABLE repo_collaborators (
    repo_id    INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    user_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    permission TEXT    NOT NULL CHECK (permission IN ('read', 'write', 'admin')),
    created_at INTEGER NOT NULL,
    PRIMARY KEY (repo_id, user_id)
);

CREATE INDEX team_members_user ON team_members (user_id);
CREATE INDEX repo_collaborators_user ON repo_collaborators (user_id);
