// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! Per-attempt job tokens (D-018, migration `0005_job_tokens.sql`).
//!
//! When a runner claims a job, the server mints a random job token (`fnj_…`) and stores
//! only its SHA-256 here, bound to the attempt and the pipeline's repository. The agent uses
//! it for the git checkout (HTTP Basic `fornax-job:<token>`); the HTTP layer grants it
//! **read access to that one repository** while [`Db::job_token_repo`] returns it, i.e. while
//! the attempt is `running` and the token has not expired. Tokens are revoked when the
//! attempt finishes ([`Db::revoke_job_token`]) and swept by [`Db::prune_job_tokens`].
//!
//! Writes queue on the per-process CI write lock (see `ci::WriteTx`) so they do not contend
//! with queue transactions inside SQLite.

use crate::{Db, Result, conflict_on_unique};

impl Db {
    /// Store the hash of a new job token for `attempt_id`, scoped to `repo_id`, valid until
    /// `expires_at` (Unix seconds). [`crate::DbError::Conflict`] if the attempt already has a
    /// token or the hash is taken; [`crate::DbError::Sqlx`] (foreign key) if the attempt or
    /// repository does not exist.
    pub async fn create_job_token(
        &self,
        attempt_id: i64,
        repo_id: i64,
        token_hash: &str,
        expires_at: i64,
    ) -> Result<()> {
        let _queue = self.write_lock.clone().lock_owned().await;
        sqlx::query(
            "INSERT INTO job_tokens (attempt_id, token_hash, repo_id, expires_at) \
             VALUES (?, ?, ?, ?)",
        )
        .bind(attempt_id)
        .bind(token_hash)
        .bind(repo_id)
        .bind(expires_at)
        .execute(&self.pool)
        .await
        .map_err(|e| conflict_on_unique(e, "job token"))?;
        Ok(())
    }

    /// `(attempt_id, repo_id)` of a valid job token: the hash is known, `expires_at >= now`
    /// and its attempt is still `running`. `None` otherwise.
    pub async fn job_token_repo(&self, token_hash: &str, now: i64) -> Result<Option<(i64, i64)>> {
        Ok(sqlx::query_as::<_, (i64, i64)>(
            "SELECT t.attempt_id, t.repo_id FROM job_tokens t \
             JOIN job_attempts a ON a.id = t.attempt_id \
             WHERE t.token_hash = ? AND t.expires_at >= ? AND a.status = 'running'",
        )
        .bind(token_hash)
        .bind(now)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Delete the job token of `attempt_id`. Returns whether one existed.
    pub async fn revoke_job_token(&self, attempt_id: i64) -> Result<bool> {
        let _queue = self.write_lock.clone().lock_owned().await;
        let res = sqlx::query("DELETE FROM job_tokens WHERE attempt_id = ?")
            .bind(attempt_id)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }

    /// Delete expired tokens and tokens whose attempt is no longer `running` (e.g. reaped as
    /// `lost`). Returns the number removed.
    pub async fn prune_job_tokens(&self, now: i64) -> Result<u64> {
        let _queue = self.write_lock.clone().lock_owned().await;
        let res = sqlx::query(
            "DELETE FROM job_tokens WHERE expires_at < ? OR attempt_id NOT IN \
             (SELECT id FROM job_attempts WHERE status = 'running')",
        )
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use crate::testutil::{db, new_repo};
    use crate::{DbError, NewJob, NewPipeline, Outcome};

    async fn setup() -> (crate::Db, i64, i64, i64) {
        let db = db().await;
        let u = db.create_user("alice", false).await.expect("user");
        let repo = db
            .create_repo(&new_repo(u.id, "demo", true))
            .await
            .expect("repo");
        let other = db
            .create_repo(&new_repo(u.id, "other", true))
            .await
            .expect("repo");
        db.create_pipeline(
            &NewPipeline {
                repo_id: repo.id,
                file: ".fornax/ci.yaml".into(),
                name: "ci".into(),
                git_ref: "refs/heads/main".into(),
                sha: "0".repeat(40),
                event: "push".into(),
                trusted: true,
                triggered_by: Some(u.id),
                jobs: vec![NewJob {
                    name: "build".into(),
                    labels: vec![],
                    payload: "{}".into(),
                    needs: vec![],
                    max_attempts: 1,
                }],
            },
            100,
        )
        .await
        .expect("pipeline");
        let runner = db
            .register_runner("r", "h", &[], true, 100)
            .await
            .expect("runner");
        let claimed = db
            .claim_job(&runner, 60, 100)
            .await
            .expect("claim")
            .expect("job");
        (db, claimed.attempt.id, repo.id, other.id)
    }

    #[tokio::test]
    async fn token_lifecycle() {
        let (db, attempt, repo, _) = setup().await;
        db.create_job_token(attempt, repo, "hash1", 200)
            .await
            .expect("create");
        assert!(matches!(
            db.create_job_token(attempt, repo, "hash2", 200).await,
            Err(DbError::Conflict(_))
        ));
        assert_eq!(
            db.job_token_repo("hash1", 150).await.expect("lookup"),
            Some((attempt, repo))
        );
        assert_eq!(db.job_token_repo("hash1", 201).await.expect("lookup"), None);
        assert_eq!(db.job_token_repo("nope", 150).await.expect("lookup"), None);
        assert!(db.revoke_job_token(attempt).await.expect("revoke"));
        assert!(!db.revoke_job_token(attempt).await.expect("revoke"));
        assert_eq!(db.job_token_repo("hash1", 150).await.expect("lookup"), None);
    }

    #[tokio::test]
    async fn token_invalid_once_attempt_finished_and_pruned() {
        let (db, attempt, repo, _) = setup().await;
        db.create_job_token(attempt, repo, "hash1", 10_000)
            .await
            .expect("create");
        let runner_id = db
            .attempt(attempt)
            .await
            .expect("attempt")
            .and_then(|a| a.runner_id)
            .expect("runner");
        db.finish_attempt(
            attempt,
            runner_id,
            &Outcome {
                success: true,
                ..Outcome::default()
            },
            110,
        )
        .await
        .expect("finish");
        assert_eq!(db.job_token_repo("hash1", 120).await.expect("lookup"), None);
        assert_eq!(db.prune_job_tokens(120).await.expect("prune"), 1);
        assert_eq!(db.prune_job_tokens(120).await.expect("prune"), 0);
    }

    #[tokio::test]
    async fn prune_removes_expired() {
        let (db, attempt, repo, _) = setup().await;
        db.create_job_token(attempt, repo, "hash1", 150)
            .await
            .expect("create");
        assert_eq!(db.prune_job_tokens(150).await.expect("prune"), 0);
        assert_eq!(db.prune_job_tokens(151).await.expect("prune"), 1);
    }
}
