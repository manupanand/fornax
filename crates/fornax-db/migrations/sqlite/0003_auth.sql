-- Copyright 2026 Manu P Anand
-- Licensed under the Apache License, Version 2.0
-- See LICENSE for details.

-- Browser sessions and OIDC login (H-013), SSH keys (H-014).

ALTER TABLE users ADD COLUMN email TEXT;

-- External identities: (OIDC issuer, subject) -> local user.
CREATE TABLE user_identities (
    provider   TEXT    NOT NULL,
    subject    TEXT    NOT NULL,
    user_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (provider, subject)
);

-- Only the SHA-256 of the session cookie value is stored.
CREATE TABLE sessions (
    id_hash    TEXT    PRIMARY KEY,
    user_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL
);
CREATE INDEX sessions_expires_at ON sessions (expires_at);

-- In-flight OIDC authorisation requests (state, nonce, PKCE verifier), single use.
CREATE TABLE oauth_states (
    state_hash    TEXT    PRIMARY KEY,
    nonce         TEXT    NOT NULL,
    pkce_verifier TEXT    NOT NULL,
    return_to     TEXT    NOT NULL,
    expires_at    INTEGER NOT NULL
);

CREATE TABLE ssh_keys (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id      INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name         TEXT    NOT NULL,
    fingerprint  TEXT    NOT NULL UNIQUE,
    public_key   TEXT    NOT NULL,
    created_at   INTEGER NOT NULL,
    last_used_at INTEGER
);
CREATE INDEX ssh_keys_user ON ssh_keys (user_id);
