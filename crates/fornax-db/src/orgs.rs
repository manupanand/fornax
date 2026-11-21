// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! Organisation membership and teams.

use fornax_core::now_unix;
use fornax_core::perm::{OrgRole, Permission};
use serde::Serialize;

use crate::{Db, DbError, Result, conflict_on_unique};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, sqlx::FromRow)]
pub struct Member {
    pub username: String,
    pub role: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, sqlx::FromRow)]
pub struct Team {
    pub id: i64,
    pub org_id: i64,
    pub name: String,
    pub permission: String,
    pub created_at: i64,
}

impl Db {
    pub async fn set_org_member(&self, org_id: i64, user_id: i64, role: OrgRole) -> Result<()> {
        sqlx::query(
            "INSERT INTO org_members (org_id, user_id, role, created_at) VALUES (?, ?, ?, ?) \
             ON CONFLICT (org_id, user_id) DO UPDATE SET role = excluded.role",
        )
        .bind(org_id)
        .bind(user_id)
        .bind(role.as_str())
        .bind(now_unix())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Remove a member (and their team memberships in this organisation).
    pub async fn remove_org_member(&self, org_id: i64, user_id: i64) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "DELETE FROM team_members WHERE user_id = ? \
             AND team_id IN (SELECT id FROM teams WHERE org_id = ?)",
        )
        .bind(user_id)
        .bind(org_id)
        .execute(&mut *tx)
        .await?;
        let res = sqlx::query("DELETE FROM org_members WHERE org_id = ? AND user_id = ?")
            .bind(org_id)
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(res.rows_affected() > 0)
    }

    pub async fn org_role(&self, org_id: i64, user_id: i64) -> Result<Option<OrgRole>> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT role FROM org_members WHERE org_id = ? AND user_id = ?")
                .bind(org_id)
                .bind(user_id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.and_then(|(r,)| r.parse().ok()))
    }

    pub async fn org_owner_count(&self, org_id: i64) -> Result<i64> {
        let (n,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM org_members WHERE org_id = ? AND role = 'owner'")
                .bind(org_id)
                .fetch_one(&self.pool)
                .await?;
        Ok(n)
    }

    pub async fn org_members(&self, org_id: i64) -> Result<Vec<Member>> {
        Ok(sqlx::query_as::<_, Member>(
            "SELECT u.username, m.role FROM org_members m JOIN users u ON u.id = m.user_id \
             WHERE m.org_id = ? ORDER BY u.lower_name",
        )
        .bind(org_id)
        .fetch_all(&self.pool)
        .await?)
    }

    // ---- teams -------------------------------------------------------------------------

    pub async fn create_team(
        &self,
        org_id: i64,
        name: &str,
        permission: Permission,
    ) -> Result<Team> {
        let (id,): (i64,) = sqlx::query_as(
            "INSERT INTO teams (org_id, name, lower_name, permission, created_at) \
             VALUES (?, ?, ?, ?, ?) RETURNING id",
        )
        .bind(org_id)
        .bind(name)
        .bind(name.to_ascii_lowercase())
        .bind(permission.as_str())
        .bind(now_unix())
        .fetch_one(&self.pool)
        .await
        .map_err(|e| conflict_on_unique(e, "team"))?;
        self.team_by_id(id).await?.ok_or(DbError::NotFound("team"))
    }

    async fn team_by_id(&self, id: i64) -> Result<Option<Team>> {
        Ok(sqlx::query_as::<_, Team>(
            "SELECT id, org_id, name, permission, created_at FROM teams WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?)
    }

    pub async fn team_by_name(&self, org_id: i64, name: &str) -> Result<Option<Team>> {
        Ok(sqlx::query_as::<_, Team>(
            "SELECT id, org_id, name, permission, created_at FROM teams \
             WHERE org_id = ? AND lower_name = ?",
        )
        .bind(org_id)
        .bind(name.to_ascii_lowercase())
        .fetch_optional(&self.pool)
        .await?)
    }

    pub async fn teams(&self, org_id: i64) -> Result<Vec<Team>> {
        Ok(sqlx::query_as::<_, Team>(
            "SELECT id, org_id, name, permission, created_at FROM teams \
             WHERE org_id = ? ORDER BY lower_name",
        )
        .bind(org_id)
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn delete_team(&self, team_id: i64) -> Result<()> {
        sqlx::query("DELETE FROM teams WHERE id = ?")
            .bind(team_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn add_team_member(&self, team_id: i64, user_id: i64) -> Result<()> {
        sqlx::query("INSERT OR IGNORE INTO team_members (team_id, user_id) VALUES (?, ?)")
            .bind(team_id)
            .bind(user_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn remove_team_member(&self, team_id: i64, user_id: i64) -> Result<()> {
        sqlx::query("DELETE FROM team_members WHERE team_id = ? AND user_id = ?")
            .bind(team_id)
            .bind(user_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn team_members(&self, team_id: i64) -> Result<Vec<String>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT u.username FROM team_members m JOIN users u ON u.id = m.user_id \
             WHERE m.team_id = ? ORDER BY u.lower_name",
        )
        .bind(team_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(n,)| n).collect())
    }

    pub async fn add_team_repo(&self, team_id: i64, repo_id: i64) -> Result<()> {
        sqlx::query("INSERT OR IGNORE INTO team_repos (team_id, repo_id) VALUES (?, ?)")
            .bind(team_id)
            .bind(repo_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn remove_team_repo(&self, team_id: i64, repo_id: i64) -> Result<()> {
        sqlx::query("DELETE FROM team_repos WHERE team_id = ? AND repo_id = ?")
            .bind(team_id)
            .bind(repo_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use fornax_core::perm::{OrgRole, Permission};

    use crate::DbError;
    use crate::testutil::db;

    #[tokio::test]
    async fn membership_and_teams() {
        let db = db().await;
        let owner = db.create_user("owner", false).await.expect("u");
        let dev = db.create_user("dev", false).await.expect("u");
        let org = db.create_org("acme", owner.id).await.expect("org");
        assert_eq!(
            db.org_role(org.id, owner.id).await.expect("q"),
            Some(OrgRole::Owner)
        );
        assert_eq!(db.org_owner_count(org.id).await.expect("q"), 1);

        db.set_org_member(org.id, dev.id, OrgRole::Member)
            .await
            .expect("add");
        let team = db
            .create_team(org.id, "Devs", Permission::Write)
            .await
            .expect("team");
        assert!(matches!(
            db.create_team(org.id, "devs", Permission::Read).await,
            Err(DbError::Conflict(_))
        ));
        db.add_team_member(team.id, dev.id).await.expect("tm");
        db.add_team_member(team.id, dev.id)
            .await
            .expect("idempotent");
        assert_eq!(db.team_members(team.id).await.expect("q"), ["dev"]);
        assert_eq!(db.teams(org.id).await.expect("q").len(), 1);
        assert_eq!(
            db.team_by_name(org.id, "DEVS")
                .await
                .expect("q")
                .map(|t| t.id),
            Some(team.id)
        );

        assert!(db.remove_org_member(org.id, dev.id).await.expect("rm"));
        assert!(db.team_members(team.id).await.expect("q").is_empty());
        assert_eq!(db.org_members(org.id).await.expect("q").len(), 1);
    }
}
