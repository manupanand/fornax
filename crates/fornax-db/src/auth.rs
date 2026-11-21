// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! Browser sessions, OIDC identities and login state, SSH keys.

use fornax_core::now_unix;
use serde::Serialize;

use crate::{Db, Result, USER_COLUMNS, User, conflict_on_unique};

/// An in-flight OIDC authorisation request.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct OAuthState {
    pub nonce: String,
    pub pkce_verifier: String,
    pub return_to: String,
}

/// An external identity resolved to a local user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityLink {
    pub provider: String,
    pub subject: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, sqlx::FromRow)]
pub struct SshKey {
    pub id: i64,
    pub user_id: i64,
    pub name: String,
    pub fingerprint: String,
    pub public_key: String,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
}

impl Db {
    // ---- sessions ----------------------------------------------------------------------

    pub async fn create_session(&self, id_hash: &str, user_id: i64, ttl_secs: i64) -> Result<()> {
        let now = now_unix();
        sqlx::query(
            "INSERT INTO sessions (id_hash, user_id, created_at, expires_at) VALUES (?, ?, ?, ?)",
        )
        .bind(id_hash)
        .bind(user_id)
        .bind(now)
        .bind(now + ttl_secs)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The user of a live session; expired sessions resolve to `None`.
    pub async fn user_by_session(&self, id_hash: &str) -> Result<Option<User>> {
        Ok(sqlx::query_as::<_, User>(&format!(
            "SELECT {USER_COLUMNS} FROM sessions s JOIN users u ON u.id = s.user_id \
             WHERE s.id_hash = ? AND s.expires_at > ? AND u.kind = 'user'"
        ))
        .bind(id_hash)
        .bind(now_unix())
        .fetch_optional(&self.pool)
        .await?)
    }

    pub async fn delete_session(&self, id_hash: &str) -> Result<()> {
        sqlx::query("DELETE FROM sessions WHERE id_hash = ?")
            .bind(id_hash)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Remove expired sessions and login states; returns rows removed.
    pub async fn prune_expired(&self) -> Result<u64> {
        let now = now_unix();
        let a = sqlx::query("DELETE FROM sessions WHERE expires_at <= ?")
            .bind(now)
            .execute(&self.pool)
            .await?;
        let b = sqlx::query("DELETE FROM oauth_states WHERE expires_at <= ?")
            .bind(now)
            .execute(&self.pool)
            .await?;
        Ok(a.rows_affected() + b.rows_affected())
    }

    // ---- OIDC --------------------------------------------------------------------------

    pub async fn save_oauth_state(
        &self,
        state_hash: &str,
        state: &OAuthState,
        ttl_secs: i64,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO oauth_states (state_hash, nonce, pkce_verifier, return_to, expires_at) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(state_hash)
        .bind(&state.nonce)
        .bind(&state.pkce_verifier)
        .bind(&state.return_to)
        .bind(now_unix() + ttl_secs)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Fetch and delete a login state (single use). Expired states are not returned.
    pub async fn take_oauth_state(&self, state_hash: &str) -> Result<Option<OAuthState>> {
        let state = sqlx::query_as::<_, OAuthState>(
            "DELETE FROM oauth_states WHERE state_hash = ? AND expires_at > ? \
             RETURNING nonce, pkce_verifier, return_to",
        )
        .bind(state_hash)
        .bind(now_unix())
        .fetch_optional(&self.pool)
        .await?;
        Ok(state)
    }

    pub async fn user_by_identity(&self, link: &IdentityLink) -> Result<Option<User>> {
        Ok(sqlx::query_as::<_, User>(&format!(
            "SELECT {USER_COLUMNS} FROM user_identities i JOIN users u ON u.id = i.user_id \
             WHERE i.provider = ? AND i.subject = ?"
        ))
        .bind(&link.provider)
        .bind(&link.subject)
        .fetch_optional(&self.pool)
        .await?)
    }

    pub async fn link_identity(&self, link: &IdentityLink, user_id: i64) -> Result<()> {
        sqlx::query(
            "INSERT INTO user_identities (provider, subject, user_id, created_at) \
             VALUES (?, ?, ?, ?)",
        )
        .bind(&link.provider)
        .bind(&link.subject)
        .bind(user_id)
        .bind(now_unix())
        .execute(&self.pool)
        .await
        .map_err(|e| conflict_on_unique(e, "identity"))?;
        Ok(())
    }

    // ---- SSH keys ----------------------------------------------------------------------

    pub async fn add_ssh_key(
        &self,
        user_id: i64,
        name: &str,
        fingerprint: &str,
        public_key: &str,
    ) -> Result<SshKey> {
        sqlx::query_as::<_, SshKey>(
            "INSERT INTO ssh_keys (user_id, name, fingerprint, public_key, created_at) \
             VALUES (?, ?, ?, ?, ?) \
             RETURNING id, user_id, name, fingerprint, public_key, created_at, last_used_at",
        )
        .bind(user_id)
        .bind(name)
        .bind(fingerprint)
        .bind(public_key)
        .bind(now_unix())
        .fetch_one(&self.pool)
        .await
        .map_err(|e| conflict_on_unique(e, "ssh key"))
    }

    pub async fn ssh_keys(&self, user_id: i64) -> Result<Vec<SshKey>> {
        Ok(sqlx::query_as::<_, SshKey>(
            "SELECT id, user_id, name, fingerprint, public_key, created_at, last_used_at \
             FROM ssh_keys WHERE user_id = ? ORDER BY id",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn delete_ssh_key(&self, user_id: i64, key_id: i64) -> Result<bool> {
        let res = sqlx::query("DELETE FROM ssh_keys WHERE id = ? AND user_id = ?")
            .bind(key_id)
            .bind(user_id)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }

    /// Resolve an SSH key fingerprint (`SHA256:…`) to its user, recording use.
    pub async fn user_by_ssh_fingerprint(&self, fingerprint: &str) -> Result<Option<User>> {
        let user = sqlx::query_as::<_, User>(&format!(
            "SELECT {USER_COLUMNS} FROM ssh_keys k JOIN users u ON u.id = k.user_id \
             WHERE k.fingerprint = ? AND u.kind = 'user'"
        ))
        .bind(fingerprint)
        .fetch_optional(&self.pool)
        .await?;
        if user.is_some() {
            sqlx::query("UPDATE ssh_keys SET last_used_at = ? WHERE fingerprint = ?")
                .bind(now_unix())
                .bind(fingerprint)
                .execute(&self.pool)
                .await?;
        }
        Ok(user)
    }
}

#[cfg(test)]
mod tests {
    use super::{IdentityLink, OAuthState};
    use crate::DbError;
    use crate::testutil::db;

    #[tokio::test]
    async fn sessions_expire() {
        let db = db().await;
        let u = db.create_user("sam", false).await.expect("u");
        db.create_session("live", u.id, 3600).await.expect("s");
        db.create_session("dead", u.id, -1).await.expect("s");
        assert_eq!(
            db.user_by_session("live").await.expect("q").map(|x| x.id),
            Some(u.id)
        );
        assert_eq!(db.user_by_session("dead").await.expect("q"), None);
        assert_eq!(db.prune_expired().await.expect("prune"), 1);
        db.delete_session("live").await.expect("del");
        assert_eq!(db.user_by_session("live").await.expect("q"), None);
    }

    #[tokio::test]
    async fn oauth_state_is_single_use() {
        let db = db().await;
        let st = OAuthState {
            nonce: "n".into(),
            pkce_verifier: "v".into(),
            return_to: "/".into(),
        };
        db.save_oauth_state("h", &st, 600).await.expect("save");
        assert_eq!(db.take_oauth_state("h").await.expect("take"), Some(st));
        assert_eq!(db.take_oauth_state("h").await.expect("take"), None);
    }

    #[tokio::test]
    async fn identities_and_keys() {
        let db = db().await;
        let u = db.create_user("ida", false).await.expect("u");
        let link = IdentityLink {
            provider: "https://idp".into(),
            subject: "123".into(),
        };
        db.link_identity(&link, u.id).await.expect("link");
        assert!(matches!(
            db.link_identity(&link, u.id).await,
            Err(DbError::Conflict(_))
        ));
        assert_eq!(
            db.user_by_identity(&link).await.expect("q").map(|x| x.id),
            Some(u.id)
        );

        let key = db
            .add_ssh_key(u.id, "laptop", "SHA256:abc", "ssh-ed25519 AAAA")
            .await
            .expect("key");
        assert!(matches!(
            db.add_ssh_key(u.id, "dup", "SHA256:abc", "x").await,
            Err(DbError::Conflict(_))
        ));
        assert_eq!(
            db.user_by_ssh_fingerprint("SHA256:abc")
                .await
                .expect("q")
                .map(|x| x.id),
            Some(u.id)
        );
        assert_eq!(db.ssh_keys(u.id).await.expect("q").len(), 1);
        assert!(db.delete_ssh_key(u.id, key.id).await.expect("del"));
    }
}
