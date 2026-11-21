// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! Users, organisations (as owners) and access tokens.

use fornax_core::now_unix;

use crate::{Db, DbError, Result, USER_COLUMNS, User, conflict_on_unique};

impl Db {
    async fn insert_owner(&self, name: &str, kind: &str, is_admin: bool) -> Result<User> {
        let (id,): (i64,) = sqlx::query_as(
            "INSERT INTO users (username, lower_name, kind, is_admin, created_at) \
             VALUES (?, ?, ?, ?, ?) RETURNING id",
        )
        .bind(name)
        .bind(name.to_ascii_lowercase())
        .bind(kind)
        .bind(is_admin)
        .bind(now_unix())
        .fetch_one(&self.pool)
        .await
        .map_err(|e| conflict_on_unique(e, "name"))?;
        self.user_by_id(id).await?.ok_or(DbError::NotFound("user"))
    }

    pub async fn create_user(&self, username: &str, is_admin: bool) -> Result<User> {
        self.insert_owner(username, "user", is_admin).await
    }

    /// Create an organisation owned by `owner_id`.
    pub async fn create_org(&self, name: &str, owner_id: i64) -> Result<User> {
        let org = self.insert_owner(name, "org", false).await?;
        self.set_org_member(org.id, owner_id, fornax_core::perm::OrgRole::Owner)
            .await?;
        Ok(org)
    }

    pub async fn user_by_id(&self, id: i64) -> Result<Option<User>> {
        Ok(sqlx::query_as::<_, User>(&format!(
            "SELECT {USER_COLUMNS} FROM users u WHERE u.id = ?"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Look up a user **or** organisation by name (case-insensitive).
    pub async fn user_by_name(&self, username: &str) -> Result<Option<User>> {
        Ok(sqlx::query_as::<_, User>(&format!(
            "SELECT {USER_COLUMNS} FROM users u WHERE u.lower_name = ?"
        ))
        .bind(username.to_ascii_lowercase())
        .fetch_optional(&self.pool)
        .await?)
    }

    pub async fn set_user_email(&self, user_id: i64, email: Option<&str>) -> Result<()> {
        sqlx::query("UPDATE users SET email = ? WHERE id = ?")
            .bind(email)
            .bind(user_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ---- access tokens -----------------------------------------------------------------

    /// Store a token. Only its hash is persisted; the caller shows the plain value once.
    pub async fn create_token(&self, user_id: i64, name: &str, token_hash: &str) -> Result<i64> {
        let (id,): (i64,) = sqlx::query_as(
            "INSERT INTO access_tokens (user_id, name, token_hash, created_at) \
             VALUES (?, ?, ?, ?) RETURNING id",
        )
        .bind(user_id)
        .bind(name)
        .bind(token_hash)
        .bind(now_unix())
        .fetch_one(&self.pool)
        .await
        .map_err(|e| conflict_on_unique(e, "token"))?;
        Ok(id)
    }

    /// Resolve a token hash to its (non-organisation) user and record `last_used_at`.
    pub async fn user_by_token_hash(&self, token_hash: &str) -> Result<Option<User>> {
        let user = sqlx::query_as::<_, User>(&format!(
            "SELECT {USER_COLUMNS} FROM access_tokens t JOIN users u ON u.id = t.user_id \
             WHERE t.token_hash = ? AND u.kind = 'user'"
        ))
        .bind(token_hash)
        .fetch_optional(&self.pool)
        .await?;
        if user.is_some() {
            sqlx::query("UPDATE access_tokens SET last_used_at = ? WHERE token_hash = ?")
                .bind(now_unix())
                .bind(token_hash)
                .execute(&self.pool)
                .await?;
        }
        Ok(user)
    }
}

#[cfg(test)]
mod tests {
    use crate::DbError;
    use crate::testutil::db;

    #[tokio::test]
    async fn users_are_case_insensitively_unique() {
        let db = db().await;
        let alice = db.create_user("Alice", true).await.expect("create");
        assert!(alice.is_admin && !alice.is_org);
        assert!(matches!(
            db.create_user("alice", false).await,
            Err(DbError::Conflict(_))
        ));
        assert_eq!(db.user_by_name("ALICE").await.expect("query"), Some(alice));
    }

    #[tokio::test]
    async fn orgs_share_the_name_space_and_cannot_use_tokens() {
        let db = db().await;
        let bob = db.create_user("bob", false).await.expect("create");
        let org = db.create_org("acme", bob.id).await.expect("org");
        assert!(org.is_org);
        assert!(matches!(
            db.create_user("ACME", false).await,
            Err(DbError::Conflict(_))
        ));
        db.create_token(org.id, "x", "orghash")
            .await
            .expect("token");
        assert_eq!(db.user_by_token_hash("orghash").await.expect("q"), None);
        db.create_token(bob.id, "cli", "hash1")
            .await
            .expect("token");
        assert_eq!(
            db.user_by_token_hash("hash1")
                .await
                .expect("q")
                .map(|u| u.id),
            Some(bob.id)
        );
    }
}
