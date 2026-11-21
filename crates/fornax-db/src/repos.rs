// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! Repositories, collaborators and effective-permission resolution.

use fornax_core::now_unix;
use fornax_core::perm::{OrgRole, Permission};
use serde::Serialize;

use crate::{Db, DbError, Result, User, conflict_on_unique};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, sqlx::FromRow)]
pub struct Repo {
    pub id: i64,
    pub owner_id: i64,
    pub owner: String,
    pub owner_is_org: bool,
    pub name: String,
    pub description: String,
    pub is_private: bool,
    pub default_branch: String,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Repo {
    /// Lowercase `(owner, name)` used for the on-disk path.
    pub fn storage_key(&self) -> (String, String) {
        (
            self.owner.to_ascii_lowercase(),
            self.name.to_ascii_lowercase(),
        )
    }

    pub fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}

#[derive(Debug, Clone)]
pub struct NewRepo<'a> {
    pub owner_id: i64,
    pub name: &'a str,
    pub description: &'a str,
    pub is_private: bool,
    pub default_branch: &'a str,
}

/// Fields of a repository that can be changed; `None` leaves a field as it is.
#[derive(Debug, Clone, Default)]
pub struct RepoUpdate<'a> {
    pub name: Option<&'a str>,
    pub description: Option<&'a str>,
    pub is_private: Option<bool>,
    pub default_branch: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, sqlx::FromRow)]
pub struct Collaborator {
    pub username: String,
    pub permission: String,
}

const REPO_COLUMNS: &str = "r.id, r.owner_id, u.username AS owner, (u.kind = 'org') AS owner_is_org, \
     r.name, r.description, r.is_private, r.default_branch, r.created_at, r.updated_at";

impl Db {
    pub async fn create_repo(&self, new: &NewRepo<'_>) -> Result<Repo> {
        let now = now_unix();
        let (id,): (i64,) = sqlx::query_as(
            "INSERT INTO repositories (owner_id, name, lower_name, description, is_private, \
             default_branch, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?) RETURNING id",
        )
        .bind(new.owner_id)
        .bind(new.name)
        .bind(new.name.to_ascii_lowercase())
        .bind(new.description)
        .bind(new.is_private)
        .bind(new.default_branch)
        .bind(now)
        .bind(now)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| conflict_on_unique(e, "repository"))?;
        self.repo_by_id(id)
            .await?
            .ok_or(DbError::NotFound("repository"))
    }

    pub async fn repo_by_id(&self, id: i64) -> Result<Option<Repo>> {
        Ok(sqlx::query_as::<_, Repo>(&format!(
            "SELECT {REPO_COLUMNS} FROM repositories r JOIN users u ON u.id = r.owner_id \
             WHERE r.id = ?"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?)
    }

    pub async fn repo_by_name(&self, owner: &str, name: &str) -> Result<Option<Repo>> {
        Ok(sqlx::query_as::<_, Repo>(&format!(
            "SELECT {REPO_COLUMNS} FROM repositories r JOIN users u ON u.id = r.owner_id \
             WHERE u.lower_name = ? AND r.lower_name = ?"
        ))
        .bind(owner.to_ascii_lowercase())
        .bind(name.to_ascii_lowercase())
        .fetch_optional(&self.pool)
        .await?)
    }

    /// All repositories of an owner, sorted by name (no visibility filtering).
    pub async fn repos_by_owner(&self, owner_id: i64) -> Result<Vec<Repo>> {
        Ok(sqlx::query_as::<_, Repo>(&format!(
            "SELECT {REPO_COLUMNS} FROM repositories r JOIN users u ON u.id = r.owner_id \
             WHERE r.owner_id = ? ORDER BY r.lower_name"
        ))
        .bind(owner_id)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Every repository, sorted by owner and name (backups, admin).
    pub async fn all_repos(&self) -> Result<Vec<Repo>> {
        Ok(sqlx::query_as::<_, Repo>(&format!(
            "SELECT {REPO_COLUMNS} FROM repositories r JOIN users u ON u.id = r.owner_id \
             ORDER BY u.lower_name, r.lower_name"
        ))
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn update_repo(&self, id: i64, update: &RepoUpdate<'_>) -> Result<Repo> {
        let current = self
            .repo_by_id(id)
            .await?
            .ok_or(DbError::NotFound("repository"))?;
        let name = update.name.unwrap_or(&current.name);
        sqlx::query(
            "UPDATE repositories SET name = ?, lower_name = ?, description = ?, is_private = ?, \
             default_branch = ?, updated_at = ? WHERE id = ?",
        )
        .bind(name)
        .bind(name.to_ascii_lowercase())
        .bind(update.description.unwrap_or(&current.description))
        .bind(update.is_private.unwrap_or(current.is_private))
        .bind(update.default_branch.unwrap_or(&current.default_branch))
        .bind(now_unix())
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(|e| conflict_on_unique(e, "repository"))?;
        self.repo_by_id(id)
            .await?
            .ok_or(DbError::NotFound("repository"))
    }

    pub async fn delete_repo(&self, id: i64) -> Result<()> {
        sqlx::query("DELETE FROM repositories WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn touch_repo(&self, id: i64) -> Result<()> {
        sqlx::query("UPDATE repositories SET updated_at = ? WHERE id = ?")
            .bind(now_unix())
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ---- collaborators -----------------------------------------------------------------

    pub async fn set_collaborator(
        &self,
        repo_id: i64,
        user_id: i64,
        permission: Permission,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO repo_collaborators (repo_id, user_id, permission, created_at) \
             VALUES (?, ?, ?, ?) \
             ON CONFLICT (repo_id, user_id) DO UPDATE SET permission = excluded.permission",
        )
        .bind(repo_id)
        .bind(user_id)
        .bind(permission.as_str())
        .bind(now_unix())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn remove_collaborator(&self, repo_id: i64, user_id: i64) -> Result<bool> {
        let res = sqlx::query("DELETE FROM repo_collaborators WHERE repo_id = ? AND user_id = ?")
            .bind(repo_id)
            .bind(user_id)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }

    pub async fn collaborators(&self, repo_id: i64) -> Result<Vec<Collaborator>> {
        Ok(sqlx::query_as::<_, Collaborator>(
            "SELECT u.username, c.permission FROM repo_collaborators c \
             JOIN users u ON u.id = c.user_id WHERE c.repo_id = ? ORDER BY u.lower_name",
        )
        .bind(repo_id)
        .fetch_all(&self.pool)
        .await?)
    }

    // ---- permissions -------------------------------------------------------------------

    /// Effective permission of `user` (or an anonymous caller) on `repo`: the highest of
    /// site admin, owner, organisation role, team grants, collaborator grant, public read.
    pub async fn permission(&self, user: Option<&User>, repo: &Repo) -> Result<Permission> {
        let public = if repo.is_private {
            Permission::None
        } else {
            Permission::Read
        };
        let Some(user) = user else {
            return Ok(public);
        };
        if user.is_admin || user.id == repo.owner_id {
            return Ok(Permission::Admin);
        }
        let grants: Vec<(String, String)> = sqlx::query_as(
            "SELECT 'org', role FROM org_members WHERE org_id = ?1 AND user_id = ?2 \
             UNION ALL \
             SELECT 'grant', permission FROM repo_collaborators WHERE repo_id = ?3 AND user_id = ?2 \
             UNION ALL \
             SELECT 'grant', t.permission FROM teams t \
               JOIN team_members m ON m.team_id = t.id \
               JOIN team_repos tr ON tr.team_id = t.id \
             WHERE m.user_id = ?2 AND tr.repo_id = ?3",
        )
        .bind(repo.owner_id)
        .bind(user.id)
        .bind(repo.id)
        .fetch_all(&self.pool)
        .await?;
        Ok(grants
            .iter()
            .filter_map(|(kind, value)| match kind.as_str() {
                "org" => value.parse::<OrgRole>().ok().map(OrgRole::base_permission),
                _ => value.parse::<Permission>().ok(),
            })
            .fold(public, Permission::max))
    }
}

#[cfg(test)]
mod tests {
    use fornax_core::perm::{OrgRole, Permission};

    use super::RepoUpdate;
    use crate::DbError;
    use crate::testutil::{db, new_repo};

    #[tokio::test]
    async fn create_update_delete() {
        let db = db().await;
        let u = db.create_user("carol", false).await.expect("create");
        let web = db
            .create_repo(&new_repo(u.id, "Web", false))
            .await
            .expect("repo");
        db.create_repo(&new_repo(u.id, "secret", true))
            .await
            .expect("repo");
        assert_eq!((web.owner.as_str(), web.owner_is_org), ("carol", false));
        assert!(matches!(
            db.create_repo(&new_repo(u.id, "web", false)).await,
            Err(DbError::Conflict(_))
        ));
        assert_eq!(db.repos_by_owner(u.id).await.expect("q").len(), 2);

        let renamed = db
            .update_repo(
                web.id,
                &RepoUpdate {
                    name: Some("site"),
                    description: Some("docs"),
                    ..RepoUpdate::default()
                },
            )
            .await
            .expect("update");
        assert_eq!(
            (renamed.name.as_str(), renamed.description.as_str()),
            ("site", "docs")
        );
        assert!(!renamed.is_private);
        assert!(matches!(
            db.update_repo(
                web.id,
                &RepoUpdate {
                    name: Some("SECRET"),
                    ..RepoUpdate::default()
                }
            )
            .await,
            Err(DbError::Conflict(_))
        ));
        db.delete_repo(web.id).await.expect("delete");
        assert_eq!(db.repo_by_name("carol", "site").await.expect("q"), None);

        db.audit(Some(u.id), "repo.create", "carol/Web", "{}")
            .await
            .expect("audit");
        assert_eq!(
            db.recent_audit(10).await.expect("q")[0].actor.as_deref(),
            Some("carol")
        );
    }

    #[tokio::test]
    async fn effective_permissions() {
        let db = db().await;
        let owner = db.create_user("owner", false).await.expect("u");
        let member = db.create_user("member", false).await.expect("u");
        let dev = db.create_user("dev", false).await.expect("u");
        let outsider = db.create_user("outsider", false).await.expect("u");
        let admin = db.create_user("root", true).await.expect("u");
        let org = db.create_org("acme", owner.id).await.expect("org");
        db.set_org_member(org.id, member.id, OrgRole::Member)
            .await
            .expect("member");
        db.set_org_member(org.id, dev.id, OrgRole::Member)
            .await
            .expect("member");
        let private = db
            .create_repo(&new_repo(org.id, "core", true))
            .await
            .expect("repo");
        let public = db
            .create_repo(&new_repo(org.id, "site", false))
            .await
            .expect("repo");

        let team = db
            .create_team(org.id, "devs", Permission::Write)
            .await
            .expect("team");
        db.add_team_member(team.id, dev.id).await.expect("tm");
        db.add_team_repo(team.id, private.id).await.expect("tr");

        let perm = |u, r| {
            let db = db.clone();
            async move { db.permission(u, r).await.expect("perm") }
        };
        assert_eq!(perm(Some(&owner), &private).await, Permission::Admin);
        assert_eq!(perm(Some(&admin), &private).await, Permission::Admin);
        assert_eq!(perm(Some(&member), &private).await, Permission::Read);
        assert_eq!(perm(Some(&dev), &private).await, Permission::Write);
        assert_eq!(perm(Some(&outsider), &private).await, Permission::None);
        assert_eq!(perm(None, &private).await, Permission::None);
        assert_eq!(perm(None, &public).await, Permission::Read);

        db.set_collaborator(private.id, outsider.id, Permission::Admin)
            .await
            .expect("collab");
        assert_eq!(perm(Some(&outsider), &private).await, Permission::Admin);
        assert_eq!(db.collaborators(private.id).await.expect("q").len(), 1);
        assert!(
            db.remove_collaborator(private.id, outsider.id)
                .await
                .expect("rm")
        );
        assert_eq!(perm(Some(&outsider), &private).await, Permission::None);
    }
}
