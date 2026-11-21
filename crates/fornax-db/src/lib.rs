// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! Fornax persistence layer.
//!
//! The only crate that talks SQL. Queries are checked at runtime (no build-time database),
//! and written in portable SQL so a Postgres migration set can be added later (D-007).
//! `impl Db` is split by area: [`users`], [`repos`], [`orgs`], [`auth`].

mod auth;
mod ci;
mod ci_tokens;
mod orgs;
mod repos;
mod users;

use std::path::Path;
use std::str::FromStr;

use fornax_core::now_unix;
use serde::Serialize;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions};

pub use auth::{IdentityLink, OAuthState, SshKey};
pub use ci::{
    Attempt, CancelOutcome, Claimed, FinishOutcome, Job, LogAppend, NewJob, NewPipeline, Outcome,
    Pipeline, Reaped, Runner,
};
pub use orgs::{Member, Team};
pub use repos::{Collaborator, NewRepo, Repo, RepoUpdate};

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations/sqlite");

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("{0} already exists")]
    Conflict(&'static str),
    #[error("{0} not found")]
    NotFound(&'static str),
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
    #[error(transparent)]
    Migrate(#[from] sqlx::migrate::MigrateError),
}

pub type Result<T> = std::result::Result<T, DbError>;

/// A user account or an organisation (`is_org`). Both share one name space.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, sqlx::FromRow)]
pub struct User {
    pub id: i64,
    pub username: String,
    pub is_admin: bool,
    pub is_org: bool,
    pub email: Option<String>,
    pub created_at: i64,
}

pub(crate) const USER_COLUMNS: &str =
    "u.id, u.username, u.is_admin, (u.kind = 'org') AS is_org, u.email, u.created_at";

/// One row of the audit log as shown to administrators.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, sqlx::FromRow)]
pub struct AuditEvent {
    pub id: i64,
    pub actor: Option<String>,
    pub action: String,
    pub target: String,
    pub detail: String,
    pub created_at: i64,
}

/// Handle to the database. Cheap to clone.
#[derive(Debug, Clone)]
pub struct Db {
    pool: SqlitePool,
    /// Serialises this process's CI write transactions (FIFO), so they queue fairly here
    /// instead of contending for SQLite's write lock (whose busy handler is unfair and can
    /// starve a waiter past `busy_timeout` under load). See `ci::WriteTx`.
    write_lock: std::sync::Arc<tokio::sync::Mutex<()>>,
}

pub(crate) fn conflict_on_unique(err: sqlx::Error, what: &'static str) -> DbError {
    if let sqlx::Error::Database(db) = &err
        && db.is_unique_violation()
    {
        return DbError::Conflict(what);
    }
    DbError::Sqlx(err)
}

impl Db {
    /// Open (creating if needed) the SQLite database at `path` and run migrations.
    pub async fn open(path: &Path) -> Result<Self> {
        let opts = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .foreign_keys(true)
            .busy_timeout(std::time::Duration::from_secs(15));
        let pool = SqlitePoolOptions::new()
            .max_connections(8)
            .connect_with(opts)
            .await?;
        Self::from_pool(pool).await
    }

    /// Private in-memory database (tests).
    pub async fn open_in_memory() -> Result<Self> {
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")?.foreign_keys(true);
        // One connection: every connection to `:memory:` is a separate database.
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await?;
        Self::from_pool(pool).await
    }

    async fn from_pool(pool: SqlitePool) -> Result<Self> {
        MIGRATOR.run(&pool).await?;
        Ok(Self {
            pool,
            write_lock: std::sync::Arc::default(),
        })
    }

    /// Write a consistent copy of the database to `dest` while it is in use
    /// (`VACUUM INTO`). `dest` must not exist.
    pub async fn backup_to(&self, dest: &Path) -> Result<()> {
        sqlx::query("VACUUM INTO ?")
            .bind(dest.to_string_lossy().into_owned())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Close all connections (flushes the WAL).
    pub async fn close(&self) {
        self.pool.close().await;
    }

    // ---- audit -------------------------------------------------------------------------

    /// Append an audit event. `detail` must be a JSON object rendered as text.
    pub async fn audit(
        &self,
        actor_id: Option<i64>,
        action: &str,
        target: &str,
        detail: &str,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO audit_events (actor_id, action, target, detail, created_at) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(actor_id)
        .bind(action)
        .bind(target)
        .bind(detail)
        .bind(now_unix())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Most recent audit events, newest first.
    pub async fn recent_audit(&self, limit: i64) -> Result<Vec<AuditEvent>> {
        Ok(sqlx::query_as::<_, AuditEvent>(
            "SELECT a.id, u.username AS actor, a.action, a.target, a.detail, a.created_at \
             FROM audit_events a LEFT JOIN users u ON u.id = a.actor_id \
             ORDER BY a.id DESC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }
}

#[cfg(test)]
pub(crate) mod testutil {
    use super::*;

    pub async fn db() -> Db {
        Db::open_in_memory().await.expect("open db")
    }

    pub fn new_repo(owner_id: i64, name: &str, is_private: bool) -> NewRepo<'_> {
        NewRepo {
            owner_id,
            name,
            description: "",
            is_private,
            default_branch: "main",
        }
    }
}
