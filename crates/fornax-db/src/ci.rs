// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! CI queue, leases and the pipeline state machine (C-004, D-007, D-014).
//!
//! The database is the source of truth for every pipeline, job and attempt. Runners claim
//! work with a Gitea-style **conditional UPDATE** (`… WHERE id = ? AND status = 'queued'`);
//! each claim creates a `job_attempts` row holding a lease that the runner renews
//! ([`Db::extend_lease`]); [`Db::expire_leases`] turns expired attempts into `lost` and
//! re-queues or fails their jobs. Status values and transitions come from
//! [`fornax_core::ci`].
//!
//! **Concurrency.** Every state change runs in a write transaction opened with
//! `BEGIN IMMEDIATE`: SQLite takes the write lock up front, so a transaction never has to
//! upgrade a read snapshot to a write lock (the upgrade fails with `SQLITE_BUSY` immediately,
//! bypassing `busy_timeout`). Within one process those transactions additionally queue on a
//! FIFO async mutex ([`WriteTx`]), so they never contend inside SQLite (its busy handler is
//! unfair and could starve a waiter past `busy_timeout` under CPU load); other processes
//! (e.g. the admin CLI) are still covered by `busy_timeout`.
//! Reads that pick claim candidates run outside the transaction (WAL readers never block);
//! the conditional UPDATE inside the transaction decides who wins, so the same code stays
//! correct on Postgres (where `write_tx` becomes a plain `BEGIN`).
//!
//! **Wake-ups.** The in-process notifier lives in the server. Methods that can make a job
//! claimable report it (`queue_changed`, `requeued`); after [`Db::create_pipeline`] the
//! caller should always notify.
//!
//! All time-dependent methods take `now` (Unix seconds) so tests are deterministic.

use std::collections::{HashMap, HashSet};
use std::str::FromStr;

use fornax_core::ValidationError;
use fornax_core::ci::{
    AttemptStatus, DagError, JobStatus, LeaseState, PipelineStatus, WaitingReason, labels_match,
    pipeline_result, validate_dag,
};
use serde::Serialize;
use sqlx::{Sqlite, SqliteConnection, SqliteExecutor, Transaction};

use crate::{Db, DbError, Result, conflict_on_unique};

/// Queued jobs inspected per claim round trip.
const CLAIM_BATCH: usize = 64;
/// Log chunks returned per [`Db::read_log`] call at most.
const READ_LOG_CHUNKS: i64 = 256;
/// How often a job may be put back without consuming an attempt (claims its runner never
/// started, see [`Db::release_claim`] and [`Db::expire_leases`]). Beyond this the normal
/// lost/failed handling applies, so a misbehaving runner cannot loop a job forever.
const MAX_FREE_REQUEUES: i64 = 5;

// ---- public types ----------------------------------------------------------------------

/// A registered CI runner. The token hash is never returned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Runner {
    pub id: i64,
    pub name: String,
    /// Labels the runner offers (sorted, unique).
    pub labels: Vec<String>,
    /// Only trusted jobs may run here (host executors, D-008).
    pub trusted_only: bool,
    /// A disabled runner never gets work.
    pub disabled: bool,
    pub created_at: i64,
    pub last_seen_at: Option<i64>,
}

/// A pipeline run to create; see [`Db::create_pipeline`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewPipeline {
    pub repo_id: i64,
    /// Spec file, e.g. `.fornax/ci.yaml`.
    pub file: String,
    pub name: String,
    pub git_ref: String,
    pub sha: String,
    /// Trigger event, e.g. `push`.
    pub event: String,
    /// Trusted refs may use secrets and host runners (D-008, D-018). Copied to every job.
    pub trusted: bool,
    pub triggered_by: Option<i64>,
    pub jobs: Vec<NewJob>,
}

/// One job of a [`NewPipeline`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewJob {
    /// Unique within the pipeline.
    pub name: String,
    /// Labels a runner must offer (all of them).
    pub labels: Vec<String>,
    /// The resolved job as JSON; stored and returned verbatim.
    pub payload: String,
    /// Names of jobs in the same pipeline that must succeed first.
    pub needs: Vec<String>,
    /// Executions allowed in total (values below 1 count as 1).
    pub max_attempts: u32,
}

/// A pipeline run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Pipeline {
    pub id: i64,
    pub repo_id: i64,
    /// Per-repository sequence number starting at 1.
    pub number: i64,
    pub file: String,
    pub name: String,
    pub git_ref: String,
    pub sha: String,
    pub event: String,
    pub trusted: bool,
    pub status: PipelineStatus,
    pub cancel_requested: bool,
    pub triggered_by: Option<i64>,
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
}

/// A job of a pipeline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Job {
    pub id: i64,
    pub pipeline_id: i64,
    pub name: String,
    pub status: JobStatus,
    /// Set iff `status` is `waiting`.
    pub waiting_reason: Option<WaitingReason>,
    pub labels: Vec<String>,
    pub payload: String,
    pub trusted: bool,
    pub max_attempts: i64,
    /// Attempts started so far (= number of the latest attempt).
    pub attempts: i64,
    /// Set by [`Db::cancel_pipeline`] while the job runs.
    pub cancel_requested: bool,
    pub created_at: i64,
    /// Last time the job became `queued`.
    pub queued_at: Option<i64>,
    /// First time the job was claimed.
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
}

/// One execution of a job by a runner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Attempt {
    pub id: i64,
    pub job_id: i64,
    /// 1-based per job.
    pub number: i64,
    /// `None` once the runner has been deleted.
    pub runner_id: Option<i64>,
    pub status: AttemptStatus,
    pub lease_expires_at: i64,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub exit_code: Option<i32>,
    pub error: Option<String>,
}

/// A job claimed by a runner, with the new attempt that holds the lease.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Claimed {
    pub job: Job,
    pub attempt: Attempt,
}

/// Result a runner reports for its attempt.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Outcome {
    pub success: bool,
    pub exit_code: Option<i32>,
    pub error: Option<String>,
}

/// Effect of [`Db::finish_attempt`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "result")]
pub enum FinishOutcome {
    /// The runner no longer holds the lease (expired, lost, already finished, or another
    /// runner's attempt). Nothing changed; the runner should drop the result.
    Rejected,
    /// The result was recorded.
    Recorded {
        /// New job status: `success`, `failed`, `canceled`, or `queued` (retry).
        job_status: JobStatus,
        /// Pipeline status after the change (terminal once every job is).
        pipeline_status: PipelineStatus,
        /// A job became claimable (retry or promoted dependents): wake waiting runners.
        queue_changed: bool,
    },
}

/// Effect of [`Db::expire_leases`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct Reaped {
    /// Attempts marked `lost`.
    pub lost: u64,
    /// Jobs put back to `queued` (wake runners if > 0), including the `released` ones.
    pub requeued: u64,
    /// Expired attempts their runner never showed life for (no `Extend`/`Log`/`Update`/
    /// `Done` since the claim, e.g. the `Next` reply was lost): deleted, and their jobs
    /// re-queued **without consuming an attempt**.
    pub released: u64,
}

/// Effect of [`Db::cancel_pipeline`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CancelOutcome {
    /// Waiting/queued jobs that became `canceled`.
    pub canceled_jobs: u64,
    /// Running jobs flagged; their runners see [`LeaseState::CancelRequested`].
    pub running_jobs: u64,
    /// `canceled` unless jobs are still running (or it had already finished).
    pub pipeline_status: PipelineStatus,
}

/// Effect of [`Db::append_log`]. Every variant carries the committed log length.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "result", content = "committed")]
pub enum LogAppend {
    /// New bytes were stored.
    Appended(u64),
    /// Everything was already stored (a retried chunk); nothing changed.
    Duplicate(u64),
    /// The chunk starts after the committed end; resend from the committed length.
    Gap(u64),
    /// The attempt is no longer `running` (finished, lost, canceled): its log is closed and
    /// nothing was stored.
    Closed(u64),
    /// Storing the chunk would make the log longer than [`LogAppend::MAX_BYTES`]; nothing
    /// was stored.
    LimitExceeded(u64),
}

impl LogAppend {
    /// Most log bytes stored per attempt (64 MiB). The agent stops sending below this and
    /// appends a truncation marker; chunks beyond it are refused with
    /// [`LogAppend::LimitExceeded`].
    pub const MAX_BYTES: u64 = 64 * 1024 * 1024;

    /// Committed log length after the call.
    pub fn committed(self) -> u64 {
        match self {
            Self::Appended(n)
            | Self::Duplicate(n)
            | Self::Gap(n)
            | Self::Closed(n)
            | Self::LimitExceeded(n) => n,
        }
    }
}

// ---- rows and decoding -----------------------------------------------------------------

const RUNNER_COLUMNS: &str = "id, name, labels, trusted_only, disabled, created_at, last_seen_at";
const PIPELINE_COLUMNS: &str = "id, repo_id, number, file, name, git_ref, sha, event, trusted, \
     status, cancel_requested, triggered_by, created_at, started_at, finished_at";
const JOB_COLUMNS: &str = "id, pipeline_id, name, status, waiting_reason, labels, payload, \
     trusted, max_attempts, attempts, cancel_requested, created_at, queued_at, started_at, \
     finished_at";
const ATTEMPT_COLUMNS: &str = "id, job_id, number, runner_id, status, lease_expires_at, \
     started_at, finished_at, exit_code, error";

fn decode_err<E: std::error::Error + Send + Sync + 'static>(err: E) -> DbError {
    DbError::Sqlx(sqlx::Error::Decode(Box::new(err)))
}

fn parse<T: FromStr<Err = ValidationError>>(s: &str) -> Result<T> {
    s.parse().map_err(decode_err)
}

fn parse_labels(s: &str) -> Result<Vec<String>> {
    serde_json::from_str(s).map_err(decode_err)
}

fn encode_labels(labels: &[String]) -> Result<String> {
    let mut labels = labels.to_vec();
    labels.sort();
    labels.dedup();
    serde_json::to_string(&labels).map_err(|e| DbError::Sqlx(sqlx::Error::Encode(Box::new(e))))
}

fn to_u64(n: i64) -> u64 {
    u64::try_from(n).unwrap_or(0)
}

fn to_i64(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

#[derive(sqlx::FromRow)]
struct RunnerRow {
    id: i64,
    name: String,
    labels: String,
    trusted_only: bool,
    disabled: bool,
    created_at: i64,
    last_seen_at: Option<i64>,
}

impl TryFrom<RunnerRow> for Runner {
    type Error = DbError;

    fn try_from(r: RunnerRow) -> Result<Self> {
        Ok(Self {
            id: r.id,
            name: r.name,
            labels: parse_labels(&r.labels)?,
            trusted_only: r.trusted_only,
            disabled: r.disabled,
            created_at: r.created_at,
            last_seen_at: r.last_seen_at,
        })
    }
}

#[derive(sqlx::FromRow)]
struct PipelineRow {
    id: i64,
    repo_id: i64,
    number: i64,
    file: String,
    name: String,
    git_ref: String,
    sha: String,
    event: String,
    trusted: bool,
    status: String,
    cancel_requested: bool,
    triggered_by: Option<i64>,
    created_at: i64,
    started_at: Option<i64>,
    finished_at: Option<i64>,
}

impl TryFrom<PipelineRow> for Pipeline {
    type Error = DbError;

    fn try_from(r: PipelineRow) -> Result<Self> {
        Ok(Self {
            id: r.id,
            repo_id: r.repo_id,
            number: r.number,
            file: r.file,
            name: r.name,
            git_ref: r.git_ref,
            sha: r.sha,
            event: r.event,
            trusted: r.trusted,
            status: parse(&r.status)?,
            cancel_requested: r.cancel_requested,
            triggered_by: r.triggered_by,
            created_at: r.created_at,
            started_at: r.started_at,
            finished_at: r.finished_at,
        })
    }
}

#[derive(sqlx::FromRow)]
struct JobRow {
    id: i64,
    pipeline_id: i64,
    name: String,
    status: String,
    waiting_reason: Option<String>,
    labels: String,
    payload: String,
    trusted: bool,
    max_attempts: i64,
    attempts: i64,
    cancel_requested: bool,
    created_at: i64,
    queued_at: Option<i64>,
    started_at: Option<i64>,
    finished_at: Option<i64>,
}

impl TryFrom<JobRow> for Job {
    type Error = DbError;

    fn try_from(r: JobRow) -> Result<Self> {
        Ok(Self {
            id: r.id,
            pipeline_id: r.pipeline_id,
            name: r.name,
            status: parse(&r.status)?,
            waiting_reason: r.waiting_reason.as_deref().map(parse).transpose()?,
            labels: parse_labels(&r.labels)?,
            payload: r.payload,
            trusted: r.trusted,
            max_attempts: r.max_attempts,
            attempts: r.attempts,
            cancel_requested: r.cancel_requested,
            created_at: r.created_at,
            queued_at: r.queued_at,
            started_at: r.started_at,
            finished_at: r.finished_at,
        })
    }
}

#[derive(sqlx::FromRow)]
struct AttemptRow {
    id: i64,
    job_id: i64,
    number: i64,
    runner_id: Option<i64>,
    status: String,
    lease_expires_at: i64,
    started_at: i64,
    finished_at: Option<i64>,
    exit_code: Option<i32>,
    error: Option<String>,
}

impl TryFrom<AttemptRow> for Attempt {
    type Error = DbError;

    fn try_from(r: AttemptRow) -> Result<Self> {
        Ok(Self {
            id: r.id,
            job_id: r.job_id,
            number: r.number,
            runner_id: r.runner_id,
            status: parse(&r.status)?,
            lease_expires_at: r.lease_expires_at,
            started_at: r.started_at,
            finished_at: r.finished_at,
            exit_code: r.exit_code,
            error: r.error,
        })
    }
}

fn dag_error(err: DagError) -> DbError {
    match err {
        DagError::DuplicateJob(_) => DbError::Conflict("job"),
        DagError::UnknownNeed { .. } => DbError::NotFound("needed job"),
        DagError::Cycle(_) => DbError::Conflict("job dependency cycle"),
    }
}

// ---- internal helpers (run inside a write transaction) ---------------------------------

async fn fetch_pipeline<'e>(ex: impl SqliteExecutor<'e>, id: i64) -> Result<Option<Pipeline>> {
    sqlx::query_as::<_, PipelineRow>(&format!(
        "SELECT {PIPELINE_COLUMNS} FROM pipelines WHERE id = ?"
    ))
    .bind(id)
    .fetch_optional(ex)
    .await?
    .map(Pipeline::try_from)
    .transpose()
}

async fn fetch_job<'e>(ex: impl SqliteExecutor<'e>, id: i64) -> Result<Option<Job>> {
    sqlx::query_as::<_, JobRow>(&format!("SELECT {JOB_COLUMNS} FROM jobs WHERE id = ?"))
        .bind(id)
        .fetch_optional(ex)
        .await?
        .map(Job::try_from)
        .transpose()
}

async fn fetch_attempt<'e>(ex: impl SqliteExecutor<'e>, id: i64) -> Result<Option<Attempt>> {
    sqlx::query_as::<_, AttemptRow>(&format!(
        "SELECT {ATTEMPT_COLUMNS} FROM job_attempts WHERE id = ?"
    ))
    .bind(id)
    .fetch_optional(ex)
    .await?
    .map(Attempt::try_from)
    .transpose()
}

/// Queue every `waiting` dependent of `job_id` whose needs have all succeeded.
async fn promote_dependents(conn: &mut SqliteConnection, job_id: i64, now: i64) -> Result<u64> {
    Ok(sqlx::query(
        "UPDATE jobs SET status = 'queued', waiting_reason = NULL, queued_at = ? \
         WHERE status = 'waiting' \
           AND id IN (SELECT job_id FROM job_needs WHERE needs_job_id = ?) \
           AND NOT EXISTS (SELECT 1 FROM job_needs n JOIN jobs d ON d.id = n.needs_job_id \
                           WHERE n.job_id = jobs.id AND d.status <> 'success')",
    )
    .bind(now)
    .bind(job_id)
    .execute(&mut *conn)
    .await?
    .rows_affected())
}

/// Mark every transitive `waiting` dependent of `job_id` as `skipped`.
async fn skip_dependents(conn: &mut SqliteConnection, job_id: i64, now: i64) -> Result<u64> {
    Ok(sqlx::query(
        "WITH RECURSIVE dep(id) AS ( \
             SELECT job_id FROM job_needs WHERE needs_job_id = ? \
             UNION \
             SELECT n.job_id FROM job_needs n JOIN dep ON n.needs_job_id = dep.id) \
         UPDATE jobs SET status = 'skipped', waiting_reason = NULL, finished_at = ? \
         WHERE status = 'waiting' AND id IN (SELECT id FROM dep)",
    )
    .bind(job_id)
    .bind(now)
    .execute(&mut *conn)
    .await?
    .rows_affected())
}

/// Set the pipeline's final status once every job is terminal; returns the current status.
async fn finalize_pipeline(
    conn: &mut SqliteConnection,
    pipeline_id: i64,
    now: i64,
) -> Result<PipelineStatus> {
    let (status, cancel_requested): (String, bool) =
        sqlx::query_as("SELECT status, cancel_requested FROM pipelines WHERE id = ?")
            .bind(pipeline_id)
            .fetch_optional(&mut *conn)
            .await?
            .ok_or(DbError::NotFound("pipeline"))?;
    let status: PipelineStatus = parse(&status)?;
    if status.is_terminal() {
        return Ok(status);
    }
    let jobs: Vec<(String,)> = sqlx::query_as("SELECT status FROM jobs WHERE pipeline_id = ?")
        .bind(pipeline_id)
        .fetch_all(&mut *conn)
        .await?;
    let jobs = jobs
        .iter()
        .map(|(s,)| parse::<JobStatus>(s))
        .collect::<Result<Vec<_>>>()?;
    let Some(result) = pipeline_result(jobs, cancel_requested) else {
        return Ok(status);
    };
    sqlx::query("UPDATE pipelines SET status = ?, finished_at = ? WHERE id = ?")
        .bind(result.as_str())
        .bind(now)
        .bind(pipeline_id)
        .execute(&mut *conn)
        .await?;
    Ok(result)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttemptEnd {
    Success,
    Failed,
    Lost,
}

struct Settled {
    job_status: JobStatus,
    pipeline_status: PipelineStatus,
    queue_changed: bool,
}

/// Close a `running` attempt and move its job (and dependents, and the pipeline) on.
/// Returns `None` if the attempt was no longer running.
async fn settle_attempt(
    conn: &mut SqliteConnection,
    attempt: &Attempt,
    end: AttemptEnd,
    exit_code: Option<i32>,
    error: Option<&str>,
    now: i64,
) -> Result<Option<Settled>> {
    let job = fetch_job(&mut *conn, attempt.job_id)
        .await?
        .ok_or(DbError::NotFound("job"))?;
    let retry_or_fail = if job.attempts < job.max_attempts {
        JobStatus::Queued
    } else {
        JobStatus::Failed
    };
    let (attempt_status, job_status) = match end {
        AttemptEnd::Success => (AttemptStatus::Success, JobStatus::Success),
        _ if job.cancel_requested => (AttemptStatus::Canceled, JobStatus::Canceled),
        AttemptEnd::Failed => (AttemptStatus::Failed, retry_or_fail),
        AttemptEnd::Lost => (AttemptStatus::Lost, retry_or_fail),
    };
    debug_assert!(JobStatus::Running.can_transition_to(job_status));

    let closed = sqlx::query(
        "UPDATE job_attempts SET status = ?, finished_at = ?, exit_code = ?, error = ? \
         WHERE id = ? AND status = 'running'",
    )
    .bind(attempt_status.as_str())
    .bind(now)
    .bind(exit_code)
    .bind(error)
    .bind(attempt.id)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if closed == 0 {
        return Ok(None);
    }

    if job_status == JobStatus::Queued {
        sqlx::query(
            "UPDATE jobs SET status = 'queued', queued_at = ? WHERE id = ? AND status = 'running'",
        )
        .bind(now)
        .bind(job.id)
        .execute(&mut *conn)
        .await?;
    } else {
        sqlx::query(
            "UPDATE jobs SET status = ?, finished_at = ? WHERE id = ? AND status = 'running'",
        )
        .bind(job_status.as_str())
        .bind(now)
        .bind(job.id)
        .execute(&mut *conn)
        .await?;
    }

    let mut queue_changed = job_status == JobStatus::Queued;
    match job_status {
        JobStatus::Success => queue_changed |= promote_dependents(conn, job.id, now).await? > 0,
        JobStatus::Failed => {
            skip_dependents(conn, job.id, now).await?;
        }
        _ => {}
    }
    let pipeline_status = finalize_pipeline(conn, job.pipeline_id, now).await?;
    Ok(Some(Settled {
        job_status,
        pipeline_status,
        queue_changed,
    }))
}

/// Put the job of a `running` attempt back to `queued` **without consuming an attempt**: the
/// attempt row is deleted (its job token and logs cascade) and `jobs.attempts` decremented,
/// so the next claim reuses the attempt number. Only for attempts the runner never started.
/// Returns `false` and changes nothing if the attempt is not the job's latest, the job is
/// not `running`, cancel was requested, or the job already used [`MAX_FREE_REQUEUES`].
async fn release_attempt(conn: &mut SqliteConnection, attempt: &Attempt, now: i64) -> Result<bool> {
    let row: Option<(String, i64, bool, i64)> = sqlx::query_as(
        "SELECT status, attempts, cancel_requested, free_requeues FROM jobs WHERE id = ?",
    )
    .bind(attempt.job_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((status, attempts, cancel_requested, free_requeues)) = row else {
        return Ok(false);
    };
    if status != JobStatus::Running.as_str()
        || cancel_requested
        || free_requeues >= MAX_FREE_REQUEUES
        || attempts != attempt.number
    {
        return Ok(false);
    }
    let deleted = sqlx::query("DELETE FROM job_attempts WHERE id = ? AND status = 'running'")
        .bind(attempt.id)
        .execute(&mut *conn)
        .await?
        .rows_affected();
    if deleted == 0 {
        return Ok(false);
    }
    // SET expressions see the old row, so `attempts = 1` means "this was the first attempt".
    sqlx::query(
        "UPDATE jobs SET status = 'queued', queued_at = ?, attempts = attempts - 1, \
         free_requeues = free_requeues + 1, \
         started_at = CASE WHEN attempts = 1 THEN NULL ELSE started_at END \
         WHERE id = ? AND status = 'running'",
    )
    .bind(now)
    .bind(attempt.job_id)
    .execute(&mut *conn)
    .await?;
    Ok(true)
}

fn holds_lease(attempt: &Attempt, runner_id: i64, now: i64) -> bool {
    attempt.status == AttemptStatus::Running
        && attempt.runner_id == Some(runner_id)
        && attempt.lease_expires_at >= now
}

async fn log_length(conn: &mut SqliteConnection, attempt_id: i64) -> Result<u64> {
    let end: Option<(i64,)> = sqlx::query_as(
        "SELECT byte_offset + length(data) FROM job_logs WHERE attempt_id = ? \
         ORDER BY byte_offset DESC LIMIT 1",
    )
    .bind(attempt_id)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(end.map_or(0, |(n,)| to_u64(n)))
}

/// A write transaction plus this process's write-queue slot. Derefs to the connection so
/// `&mut *tx` works as an executor; dropping it rolls back and frees the slot.
pub(crate) struct WriteTx {
    tx: Transaction<'static, Sqlite>,
    _guard: tokio::sync::OwnedMutexGuard<()>,
}

impl WriteTx {
    async fn commit(self) -> Result<()> {
        self.tx.commit().await?;
        Ok(())
    }
}

impl std::ops::Deref for WriteTx {
    type Target = SqliteConnection;
    fn deref(&self) -> &SqliteConnection {
        &self.tx
    }
}

impl std::ops::DerefMut for WriteTx {
    fn deref_mut(&mut self) -> &mut SqliteConnection {
        &mut self.tx
    }
}

// ---- Db API ----------------------------------------------------------------------------

impl Db {
    /// A write transaction that holds SQLite's write lock from the start, taken only after
    /// this process's write queue lets it through (see module docs).
    async fn write_tx(&self) -> Result<WriteTx> {
        let guard = self.write_lock.clone().lock_owned().await;
        let tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        Ok(WriteTx { tx, _guard: guard })
    }

    // ---- runners -----------------------------------------------------------------------

    /// Register a runner. `token_hash` is the hash of its secret token (never the token).
    /// Fails with [`DbError::Conflict`] if the hash is already registered.
    pub async fn register_runner(
        &self,
        name: &str,
        token_hash: &str,
        labels: &[String],
        trusted_only: bool,
        now: i64,
    ) -> Result<Runner> {
        let row = sqlx::query_as::<_, RunnerRow>(&format!(
            "INSERT INTO runners (name, token_hash, labels, trusted_only, created_at) \
             VALUES (?, ?, ?, ?, ?) RETURNING {RUNNER_COLUMNS}"
        ))
        .bind(name)
        .bind(token_hash)
        .bind(encode_labels(labels)?)
        .bind(trusted_only)
        .bind(now)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| conflict_on_unique(e, "runner"))?;
        row.try_into()
    }

    /// The runner owning `token_hash` (disabled runners included; check `disabled`).
    pub async fn runner_by_token_hash(&self, token_hash: &str) -> Result<Option<Runner>> {
        sqlx::query_as::<_, RunnerRow>(&format!(
            "SELECT {RUNNER_COLUMNS} FROM runners WHERE token_hash = ?"
        ))
        .bind(token_hash)
        .fetch_optional(&self.pool)
        .await?
        .map(Runner::try_from)
        .transpose()
    }

    /// A runner by id.
    pub async fn runner(&self, id: i64) -> Result<Option<Runner>> {
        sqlx::query_as::<_, RunnerRow>(&format!(
            "SELECT {RUNNER_COLUMNS} FROM runners WHERE id = ?"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?
        .map(Runner::try_from)
        .transpose()
    }

    /// Record that a runner contacted the server. Queues on the CI write lock.
    pub async fn touch_runner(&self, id: i64, now: i64) -> Result<()> {
        let _queue = self.write_lock.clone().lock_owned().await;
        sqlx::query("UPDATE runners SET last_seen_at = ? WHERE id = ?")
            .bind(now)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Enable or disable a runner. Disabled runners get no new work.
    pub async fn set_runner_disabled(&self, id: i64, disabled: bool) -> Result<()> {
        let res = sqlx::query("UPDATE runners SET disabled = ? WHERE id = ?")
            .bind(disabled)
            .bind(id)
            .execute(&self.pool)
            .await?;
        if res.rows_affected() == 0 {
            return Err(DbError::NotFound("runner"));
        }
        Ok(())
    }

    /// All runners, by id.
    pub async fn list_runners(&self) -> Result<Vec<Runner>> {
        sqlx::query_as::<_, RunnerRow>(&format!("SELECT {RUNNER_COLUMNS} FROM runners ORDER BY id"))
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(Runner::try_from)
            .collect()
    }

    // ---- pipelines ---------------------------------------------------------------------

    /// Create a pipeline and its jobs in one transaction.
    ///
    /// Allocates the next per-repository `number`. Jobs without `needs` start `queued`,
    /// the others `waiting(dependency)`. A pipeline without jobs is created `success`.
    /// Errors: [`DbError::Conflict`] for duplicate job names or a dependency cycle,
    /// [`DbError::NotFound`] for an unknown `needs` name or repository.
    /// The caller should wake waiting runners afterwards.
    pub async fn create_pipeline(&self, new: &NewPipeline, now: i64) -> Result<Pipeline> {
        validate_dag(
            new.jobs
                .iter()
                .map(|j| (j.name.as_str(), j.needs.iter().map(String::as_str))),
        )
        .map_err(dag_error)?;

        let mut tx = self.write_tx().await?;
        let (number,): (i64,) =
            sqlx::query_as("SELECT COALESCE(MAX(number), 0) + 1 FROM pipelines WHERE repo_id = ?")
                .bind(new.repo_id)
                .fetch_one(&mut *tx)
                .await?;
        let (pipeline_id,): (i64,) = sqlx::query_as(
            "INSERT INTO pipelines (repo_id, number, file, name, git_ref, sha, event, trusted, \
             status, triggered_by, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             RETURNING id",
        )
        .bind(new.repo_id)
        .bind(number)
        .bind(&new.file)
        .bind(&new.name)
        .bind(&new.git_ref)
        .bind(&new.sha)
        .bind(&new.event)
        .bind(new.trusted)
        .bind(PipelineStatus::Pending.as_str())
        .bind(new.triggered_by)
        .bind(now)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| match &e {
            sqlx::Error::Database(d) if d.is_foreign_key_violation() => {
                DbError::NotFound("repository")
            }
            _ => conflict_on_unique(e, "pipeline"),
        })?;

        let mut ids: HashMap<&str, i64> = HashMap::new();
        for job in &new.jobs {
            let (status, reason, queued_at) = if job.needs.is_empty() {
                (JobStatus::Queued, None, Some(now))
            } else {
                (
                    JobStatus::Waiting,
                    Some(WaitingReason::Dependency.as_str()),
                    None,
                )
            };
            let (id,): (i64,) = sqlx::query_as(
                "INSERT INTO jobs (pipeline_id, name, status, waiting_reason, labels, payload, \
                 trusted, max_attempts, created_at, queued_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?) RETURNING id",
            )
            .bind(pipeline_id)
            .bind(&job.name)
            .bind(status.as_str())
            .bind(reason)
            .bind(encode_labels(&job.labels)?)
            .bind(&job.payload)
            .bind(new.trusted)
            .bind(i64::from(job.max_attempts.max(1)))
            .bind(now)
            .bind(queued_at)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| conflict_on_unique(e, "job"))?;
            ids.insert(job.name.as_str(), id);
        }
        for job in &new.jobs {
            let job_id = ids.get(job.name.as_str()).copied();
            for need in &job.needs {
                let need_id = ids.get(need.as_str()).copied();
                let (Some(job_id), Some(need_id)) = (job_id, need_id) else {
                    return Err(DbError::NotFound("needed job"));
                };
                sqlx::query(
                    "INSERT INTO job_needs (job_id, needs_job_id) VALUES (?, ?) \
                     ON CONFLICT DO NOTHING",
                )
                .bind(job_id)
                .bind(need_id)
                .execute(&mut *tx)
                .await?;
            }
        }
        finalize_pipeline(&mut tx, pipeline_id, now).await?;
        let pipeline = fetch_pipeline(&mut *tx, pipeline_id)
            .await?
            .ok_or(DbError::NotFound("pipeline"))?;
        tx.commit().await?;
        Ok(pipeline)
    }

    /// A pipeline by id.
    pub async fn pipeline(&self, id: i64) -> Result<Option<Pipeline>> {
        fetch_pipeline(&self.pool, id).await
    }

    /// A pipeline by repository and per-repository number (as shown in URLs).
    pub async fn pipeline_by_number(&self, repo_id: i64, number: i64) -> Result<Option<Pipeline>> {
        sqlx::query_as::<_, PipelineRow>(&format!(
            "SELECT {PIPELINE_COLUMNS} FROM pipelines WHERE repo_id = ? AND number = ?"
        ))
        .bind(repo_id)
        .bind(number)
        .fetch_optional(&self.pool)
        .await?
        .map(Pipeline::try_from)
        .transpose()
    }

    /// The latest `limit` pipelines of a repository, newest first.
    pub async fn pipelines_for_repo(&self, repo_id: i64, limit: i64) -> Result<Vec<Pipeline>> {
        sqlx::query_as::<_, PipelineRow>(&format!(
            "SELECT {PIPELINE_COLUMNS} FROM pipelines WHERE repo_id = ? \
             ORDER BY number DESC LIMIT ?"
        ))
        .bind(repo_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(Pipeline::try_from)
        .collect()
    }

    /// Jobs of a pipeline in creation order.
    pub async fn jobs_for_pipeline(&self, pipeline_id: i64) -> Result<Vec<Job>> {
        sqlx::query_as::<_, JobRow>(&format!(
            "SELECT {JOB_COLUMNS} FROM jobs WHERE pipeline_id = ? ORDER BY id"
        ))
        .bind(pipeline_id)
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(Job::try_from)
        .collect()
    }

    /// `(job_id, needs_job_id)` edges of a pipeline's DAG.
    pub async fn job_needs(&self, pipeline_id: i64) -> Result<Vec<(i64, i64)>> {
        Ok(sqlx::query_as(
            "SELECT n.job_id, n.needs_job_id FROM job_needs n JOIN jobs j ON j.id = n.job_id \
             WHERE j.pipeline_id = ? ORDER BY n.job_id, n.needs_job_id",
        )
        .bind(pipeline_id)
        .fetch_all(&self.pool)
        .await?)
    }

    /// A job by id.
    pub async fn job(&self, id: i64) -> Result<Option<Job>> {
        fetch_job(&self.pool, id).await
    }

    /// An attempt by id.
    pub async fn attempt(&self, id: i64) -> Result<Option<Attempt>> {
        fetch_attempt(&self.pool, id).await
    }

    /// Attempts of a job, oldest first.
    pub async fn attempts_for_job(&self, job_id: i64) -> Result<Vec<Attempt>> {
        sqlx::query_as::<_, AttemptRow>(&format!(
            "SELECT {ATTEMPT_COLUMNS} FROM job_attempts WHERE job_id = ? ORDER BY number"
        ))
        .bind(job_id)
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(Attempt::try_from)
        .collect()
    }

    /// Number of `queued` jobs (for a queue-depth metric).
    pub async fn queued_count(&self) -> Result<i64> {
        let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM jobs WHERE status = 'queued'")
            .fetch_one(&self.pool)
            .await?;
        Ok(n)
    }

    // ---- queue -------------------------------------------------------------------------

    /// Claim the oldest `queued` job `runner` may run: job labels ⊆ runner labels, and
    /// trusted jobs only if the runner is `trusted_only`. Disabled runners get `None`.
    ///
    /// The job becomes `running`, gets a new attempt (`number = attempts + 1`) with
    /// `lease_expires_at = now + lease_secs`, and a `pending` pipeline becomes `running`.
    /// Safe under concurrency: each candidate is taken with a conditional UPDATE; if another
    /// runner won, the next candidate is tried. `None` means nothing is claimable now.
    pub async fn claim_job(
        &self,
        runner: &Runner,
        lease_secs: i64,
        now: i64,
    ) -> Result<Option<Claimed>> {
        if runner.disabled {
            return Ok(None);
        }
        let mut cursor = (i64::MIN, i64::MIN);
        loop {
            let batch: Vec<(i64, i64, String)> = sqlx::query_as(
                "SELECT id, queued_at, labels FROM jobs \
                 WHERE status = 'queued' AND (trusted = 1 OR ? = 0) \
                   AND (queued_at > ? OR (queued_at = ? AND id > ?)) \
                 ORDER BY queued_at, id LIMIT ?",
            )
            .bind(runner.trusted_only)
            .bind(cursor.0)
            .bind(cursor.0)
            .bind(cursor.1)
            .bind(to_i64(CLAIM_BATCH as u64))
            .fetch_all(&self.pool)
            .await?;
            for (id, _, labels) in &batch {
                if !labels_match(&parse_labels(labels)?, &runner.labels) {
                    continue;
                }
                if let Some(claimed) = self.try_claim(*id, runner.id, lease_secs, now).await? {
                    return Ok(Some(claimed));
                }
            }
            match batch.last() {
                Some((id, queued_at, _)) if batch.len() == CLAIM_BATCH => {
                    cursor = (*queued_at, *id);
                }
                _ => return Ok(None),
            }
        }
    }

    async fn try_claim(
        &self,
        job_id: i64,
        runner_id: i64,
        lease_secs: i64,
        now: i64,
    ) -> Result<Option<Claimed>> {
        let mut tx = self.write_tx().await?;
        let won: Option<(i64, i64)> = sqlx::query_as(
            "UPDATE jobs SET status = 'running', attempts = attempts + 1, \
             started_at = COALESCE(started_at, ?) \
             WHERE id = ? AND status = 'queued' RETURNING attempts, pipeline_id",
        )
        .bind(now)
        .bind(job_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((number, pipeline_id)) = won else {
            return Ok(None); // lost the race; dropping `tx` rolls back
        };
        let (attempt_id,): (i64,) = sqlx::query_as(
            "INSERT INTO job_attempts (job_id, number, runner_id, status, lease_expires_at, \
             started_at) VALUES (?, ?, ?, 'running', ?, ?) RETURNING id",
        )
        .bind(job_id)
        .bind(number)
        .bind(runner_id)
        .bind(now.saturating_add(lease_secs))
        .bind(now)
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE pipelines SET status = 'running', started_at = COALESCE(started_at, ?) \
             WHERE id = ? AND status = 'pending'",
        )
        .bind(now)
        .bind(pipeline_id)
        .execute(&mut *tx)
        .await?;
        let job = fetch_job(&mut *tx, job_id)
            .await?
            .ok_or(DbError::NotFound("job"))?;
        let attempt = fetch_attempt(&mut *tx, attempt_id)
            .await?
            .ok_or(DbError::NotFound("job attempt"))?;
        tx.commit().await?;
        Ok(Some(Claimed { job, attempt }))
    }

    /// Renew the lease of `attempt_id` to `now + lease_secs`.
    ///
    /// [`LeaseState::Lost`] if the attempt is not `running`, belongs to another runner, or
    /// its lease already expired (`lease_expires_at < now`, even if not reaped yet);
    /// [`LeaseState::CancelRequested`] if the pipeline was canceled (lease still renewed so
    /// the runner can stop cleanly and report); otherwise [`LeaseState::Ok`].
    /// A renewal also marks the attempt as started by its runner (see [`Reaped::released`]).
    /// The write queues on the CI write lock.
    pub async fn extend_lease(
        &self,
        attempt_id: i64,
        runner_id: i64,
        lease_secs: i64,
        now: i64,
    ) -> Result<LeaseState> {
        let queue = self.write_lock.clone().lock_owned().await;
        let job: Option<(i64,)> = sqlx::query_as(
            "UPDATE job_attempts SET lease_expires_at = ?, seen_at = COALESCE(seen_at, ?) \
             WHERE id = ? AND runner_id = ? AND status = 'running' AND lease_expires_at >= ? \
             RETURNING job_id",
        )
        .bind(now.saturating_add(lease_secs))
        .bind(now)
        .bind(attempt_id)
        .bind(runner_id)
        .bind(now)
        .fetch_optional(&self.pool)
        .await?;
        drop(queue);
        let Some((job_id,)) = job else {
            return Ok(LeaseState::Lost);
        };
        let cancel: Option<(bool,)> =
            sqlx::query_as("SELECT cancel_requested FROM jobs WHERE id = ?")
                .bind(job_id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(match cancel {
            Some((true,)) => LeaseState::CancelRequested,
            Some((false,)) => LeaseState::Ok,
            None => LeaseState::Lost,
        })
    }

    /// Record the result of an attempt. Only the runner holding a live lease
    /// (`running`, same runner, `lease_expires_at >= now`) is heard; anything else is
    /// [`FinishOutcome::Rejected`] and changes nothing.
    ///
    /// Transitions: success ⇒ attempt + job `success`, dependents whose needs all
    /// succeeded become `queued`. Failure ⇒ attempt `failed`; the job is re-`queued` while
    /// `attempts < max_attempts`, else `failed` and every transitive waiting dependent
    /// becomes `skipped`. If the job was cancel-requested, a failure becomes `canceled`
    /// (no retry). When every job is terminal the pipeline gets its final status
    /// ([`fornax_core::ci::pipeline_result`]) and `finished_at`.
    /// [`DbError::NotFound`] if the attempt does not exist.
    pub async fn finish_attempt(
        &self,
        attempt_id: i64,
        runner_id: i64,
        outcome: &Outcome,
        now: i64,
    ) -> Result<FinishOutcome> {
        let mut tx = self.write_tx().await?;
        let attempt = fetch_attempt(&mut *tx, attempt_id)
            .await?
            .ok_or(DbError::NotFound("job attempt"))?;
        if !holds_lease(&attempt, runner_id, now) {
            return Ok(FinishOutcome::Rejected);
        }
        let end = if outcome.success {
            AttemptEnd::Success
        } else {
            AttemptEnd::Failed
        };
        let settled = settle_attempt(
            &mut tx,
            &attempt,
            end,
            outcome.exit_code,
            outcome.error.as_deref(),
            now,
        )
        .await?;
        let Some(s) = settled else {
            return Ok(FinishOutcome::Rejected);
        };
        tx.commit().await?;
        Ok(FinishOutcome::Recorded {
            job_status: s.job_status,
            pipeline_status: s.pipeline_status,
            queue_changed: s.queue_changed,
        })
    }

    /// Reap expired leases: every `running` attempt with `lease_expires_at < now` becomes
    /// `lost` (error "lease expired"); its job is re-`queued` while attempts remain, else
    /// `failed` with dependents `skipped` (or `canceled` if cancel was requested).
    ///
    /// Exception: an expired attempt its runner never showed life for (no `Extend`, `Log`,
    /// `Update` or `Done` since the claim — typically a lost `Next` reply) is deleted and its
    /// job re-queued without consuming an attempt ([`Reaped::released`]), at most
    /// `MAX_FREE_REQUEUES` (5) times per job.
    /// Call periodically (e.g. every lease interval).
    pub async fn expire_leases(&self, now: i64) -> Result<Reaped> {
        let mut tx = self.write_tx().await?;
        let expired = sqlx::query_as::<_, AttemptRow>(&format!(
            "SELECT {ATTEMPT_COLUMNS} FROM job_attempts \
             WHERE status = 'running' AND lease_expires_at < ? ORDER BY id"
        ))
        .bind(now)
        .fetch_all(&mut *tx)
        .await?;
        let unseen: HashSet<i64> = sqlx::query_as::<_, (i64,)>(
            "SELECT id FROM job_attempts \
             WHERE status = 'running' AND lease_expires_at < ? AND seen_at IS NULL",
        )
        .bind(now)
        .fetch_all(&mut *tx)
        .await?
        .into_iter()
        .map(|(id,)| id)
        .collect();
        let mut reaped = Reaped::default();
        for row in expired {
            let attempt = Attempt::try_from(row)?;
            if unseen.contains(&attempt.id) && release_attempt(&mut tx, &attempt, now).await? {
                reaped.released += 1;
                reaped.requeued += 1;
                continue;
            }
            let settled = settle_attempt(
                &mut tx,
                &attempt,
                AttemptEnd::Lost,
                None,
                Some("lease expired"),
                now,
            )
            .await?;
            if let Some(s) = settled {
                reaped.lost += 1;
                if s.job_status == JobStatus::Queued {
                    reaped.requeued += 1;
                }
            }
        }
        tx.commit().await?;
        Ok(reaped)
    }

    /// Give a claim back: the runner holding the live lease of `attempt_id` could not start
    /// it (e.g. the server failed to build the job for a transient reason). The attempt is
    /// deleted and the job re-`queued` **without consuming an attempt**.
    ///
    /// Returns `false` (nothing changed) if `runner_id` does not hold the lease, cancel was
    /// requested, or the job was already given back `MAX_FREE_REQUEUES` (5) times — the
    /// caller should then fail the attempt with [`Db::finish_attempt`].
    /// [`DbError::NotFound`] if the attempt does not exist.
    pub async fn release_claim(&self, attempt_id: i64, runner_id: i64, now: i64) -> Result<bool> {
        let mut tx = self.write_tx().await?;
        let attempt = fetch_attempt(&mut *tx, attempt_id)
            .await?
            .ok_or(DbError::NotFound("job attempt"))?;
        if !holds_lease(&attempt, runner_id, now)
            || !release_attempt(&mut tx, &attempt, now).await?
        {
            return Ok(false);
        }
        tx.commit().await?;
        Ok(true)
    }

    /// Mark `attempt_id` as started by its runner (first sign of life, e.g. a step `Update`),
    /// so an expiring lease is treated as `lost` rather than released. Idempotent.
    pub async fn mark_attempt_seen(&self, attempt_id: i64, now: i64) -> Result<()> {
        let _queue = self.write_lock.clone().lock_owned().await;
        sqlx::query("UPDATE job_attempts SET seen_at = ? WHERE id = ? AND seen_at IS NULL")
            .bind(now)
            .bind(attempt_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Cancel a pipeline: `waiting`/`queued` jobs become `canceled`; running jobs are
    /// flagged so [`Db::extend_lease`] returns [`LeaseState::CancelRequested`]. The pipeline
    /// becomes `canceled` once nothing runs (immediately if nothing did). A finished
    /// pipeline is left unchanged. [`DbError::NotFound`] if it does not exist.
    pub async fn cancel_pipeline(&self, pipeline_id: i64, now: i64) -> Result<CancelOutcome> {
        let mut tx = self.write_tx().await?;
        let pipeline = fetch_pipeline(&mut *tx, pipeline_id)
            .await?
            .ok_or(DbError::NotFound("pipeline"))?;
        if pipeline.status.is_terminal() {
            return Ok(CancelOutcome {
                canceled_jobs: 0,
                running_jobs: 0,
                pipeline_status: pipeline.status,
            });
        }
        sqlx::query("UPDATE pipelines SET cancel_requested = 1 WHERE id = ?")
            .bind(pipeline_id)
            .execute(&mut *tx)
            .await?;
        let canceled_jobs = sqlx::query(
            "UPDATE jobs SET status = 'canceled', waiting_reason = NULL, finished_at = ? \
             WHERE pipeline_id = ? AND status IN ('waiting', 'queued')",
        )
        .bind(now)
        .bind(pipeline_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        let running_jobs = sqlx::query(
            "UPDATE jobs SET cancel_requested = 1 WHERE pipeline_id = ? AND status = 'running'",
        )
        .bind(pipeline_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        let pipeline_status = finalize_pipeline(&mut tx, pipeline_id, now).await?;
        tx.commit().await?;
        Ok(CancelOutcome {
            canceled_jobs,
            running_jobs,
            pipeline_status,
        })
    }

    // ---- logs --------------------------------------------------------------------------

    /// Append `data` at byte `offset` of an attempt's log.
    ///
    /// Idempotent for retries: bytes before the committed end are ignored (a chunk that is
    /// entirely stored is [`LogAppend::Duplicate`]; one that overlaps the end stores only its
    /// new tail). A chunk starting after the committed end is [`LogAppend::Gap`] and is not
    /// stored. [`DbError::NotFound`] if the attempt does not exist.
    ///
    /// Logs are accepted only while the attempt is `running` ([`LogAppend::Closed`]
    /// otherwise — the agent sends `Done` only after every chunk was acknowledged, so no
    /// grace period is needed) and up to [`LogAppend::MAX_BYTES`] per attempt
    /// ([`LogAppend::LimitExceeded`]). Stored bytes mark the attempt as started by its runner.
    pub async fn append_log(&self, attempt_id: i64, offset: u64, data: &[u8]) -> Result<LogAppend> {
        let mut tx = self.write_tx().await?;
        let status: Option<(String,)> =
            sqlx::query_as("SELECT status FROM job_attempts WHERE id = ?")
                .bind(attempt_id)
                .fetch_optional(&mut *tx)
                .await?;
        let Some((status,)) = status else {
            return Err(DbError::NotFound("job attempt"));
        };
        let committed = log_length(&mut tx, attempt_id).await?;
        if status != AttemptStatus::Running.as_str() {
            return Ok(LogAppend::Closed(committed));
        }
        let end = offset.saturating_add(data.len() as u64);
        if offset > committed {
            return Ok(LogAppend::Gap(committed));
        }
        if end <= committed {
            return Ok(LogAppend::Duplicate(committed));
        }
        let skip = usize::try_from(committed - offset).unwrap_or(usize::MAX);
        let tail = data.get(skip..).unwrap_or_default();
        if committed.saturating_add(tail.len() as u64) > LogAppend::MAX_BYTES {
            return Ok(LogAppend::LimitExceeded(committed));
        }
        sqlx::query("INSERT INTO job_logs (attempt_id, byte_offset, data) VALUES (?, ?, ?)")
            .bind(attempt_id)
            .bind(to_i64(committed))
            .bind(tail)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE job_attempts SET seen_at = ? WHERE id = ? AND seen_at IS NULL")
            .bind(fornax_core::now_unix())
            .bind(attempt_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(LogAppend::Appended(committed + tail.len() as u64))
    }

    /// Up to `max_bytes` of an attempt's log starting at `from_offset`, and the offset to
    /// continue from. An empty result means no more data yet.
    pub async fn read_log(
        &self,
        attempt_id: i64,
        from_offset: u64,
        max_bytes: usize,
    ) -> Result<(Vec<u8>, u64)> {
        let chunks: Vec<(i64, Vec<u8>)> = sqlx::query_as(
            "SELECT byte_offset, data FROM job_logs \
             WHERE attempt_id = ? AND byte_offset + length(data) > ? \
             ORDER BY byte_offset LIMIT ?",
        )
        .bind(attempt_id)
        .bind(to_i64(from_offset))
        .bind(READ_LOG_CHUNKS)
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::new();
        let mut pos = from_offset;
        for (offset, data) in chunks {
            let offset = to_u64(offset);
            if offset > pos || out.len() >= max_bytes {
                break;
            }
            let start = usize::try_from(pos - offset).unwrap_or(usize::MAX);
            let chunk = data.get(start..).unwrap_or_default();
            let take = chunk.len().min(max_bytes - out.len());
            out.extend_from_slice(chunk.get(..take).unwrap_or_default());
            pos += take as u64;
        }
        Ok((out, pos))
    }

    /// Committed length of an attempt's log.
    pub async fn log_len(&self, attempt_id: i64) -> Result<u64> {
        let mut conn = self.pool.acquire().await?;
        log_length(&mut conn, attempt_id).await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use fornax_core::ci::{AttemptStatus, JobStatus, LeaseState, PipelineStatus, WaitingReason};

    use super::*;
    use crate::testutil::{db, new_repo};

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| (*x).to_owned()).collect()
    }

    fn job(name: &str, needs: &[&str]) -> NewJob {
        NewJob {
            name: name.to_owned(),
            labels: s(&["linux"]),
            payload: format!(r#"{{"script":"echo {name}"}}"#),
            needs: s(needs),
            max_attempts: 1,
        }
    }

    fn pipeline(repo_id: i64, trusted: bool, jobs: Vec<NewJob>) -> NewPipeline {
        NewPipeline {
            repo_id,
            file: ".fornax/ci.yaml".into(),
            name: "ci".into(),
            git_ref: "refs/heads/main".into(),
            sha: "0123456789abcdef0123456789abcdef01234567".into(),
            event: "push".into(),
            trusted,
            triggered_by: None,
            jobs,
        }
    }

    async fn repo(db: &Db, owner: &str, name: &str) -> i64 {
        let u = match db.create_user(owner, false).await {
            Ok(u) => u,
            Err(_) => db.user_by_name(owner).await.expect("user").expect("exists"),
        };
        db.create_repo(&new_repo(u.id, name, false))
            .await
            .expect("repo")
            .id
    }

    async fn runner(db: &Db, name: &str, labels: &[&str], trusted_only: bool) -> Runner {
        db.register_runner(name, &format!("hash-{name}"), &s(labels), trusted_only, 1)
            .await
            .expect("runner")
    }

    async fn status_of(db: &Db, pipeline_id: i64) -> HashMap<String, JobStatus> {
        db.jobs_for_pipeline(pipeline_id)
            .await
            .expect("jobs")
            .into_iter()
            .map(|j| (j.name, j.status))
            .collect()
    }

    fn ok() -> Outcome {
        Outcome {
            success: true,
            exit_code: Some(0),
            error: None,
        }
    }

    fn fail() -> Outcome {
        Outcome {
            success: false,
            exit_code: Some(1),
            error: Some("exit status 1".into()),
        }
    }

    async fn claim(db: &Db, r: &Runner, now: i64) -> Claimed {
        db.claim_job(r, 60, now)
            .await
            .expect("claim")
            .expect("a job")
    }

    async fn finish(db: &Db, c: &Claimed, r: &Runner, o: Outcome, now: i64) -> FinishOutcome {
        db.finish_attempt(c.attempt.id, r.id, &o, now)
            .await
            .expect("finish")
    }

    fn diamond() -> Vec<NewJob> {
        vec![
            job("a", &[]),
            job("b", &["a"]),
            job("c", &["a"]),
            job("d", &["b", "c"]),
        ]
    }

    #[tokio::test]
    async fn dag_success_path() {
        let db = db().await;
        let repo = repo(&db, "alice", "app").await;
        let r = runner(&db, "r1", &["linux"], false).await;
        let p = db
            .create_pipeline(&pipeline(repo, true, diamond()), 10)
            .await
            .expect("create");
        assert_eq!((p.number, p.status), (1, PipelineStatus::Pending));
        let jobs = db.jobs_for_pipeline(p.id).await.expect("jobs");
        assert_eq!(jobs[0].status, JobStatus::Queued);
        assert!(jobs[1..].iter().all(|j| j.status == JobStatus::Waiting
            && j.waiting_reason == Some(WaitingReason::Dependency)));
        assert_eq!(db.job_needs(p.id).await.expect("needs").len(), 4);
        assert_eq!(db.queued_count().await.expect("count"), 1);

        let a = claim(&db, &r, 11).await;
        assert_eq!((a.job.name.as_str(), a.attempt.number), ("a", 1));
        assert_eq!(a.attempt.lease_expires_at, 71);
        assert_eq!(
            db.pipeline(p.id).await.expect("q").expect("p").status,
            PipelineStatus::Running
        );
        assert!(db.claim_job(&r, 60, 11).await.expect("claim").is_none());
        assert_eq!(
            finish(&db, &a, &r, ok(), 12).await,
            FinishOutcome::Recorded {
                job_status: JobStatus::Success,
                pipeline_status: PipelineStatus::Running,
                queue_changed: true
            }
        );
        let b = claim(&db, &r, 13).await;
        let c = claim(&db, &r, 13).await;
        assert_eq!((b.job.name.as_str(), c.job.name.as_str()), ("b", "c"));
        let FinishOutcome::Recorded { queue_changed, .. } = finish(&db, &b, &r, ok(), 14).await
        else {
            panic!("rejected")
        };
        assert!(!queue_changed, "d still needs c");
        assert_eq!(status_of(&db, p.id).await["d"], JobStatus::Waiting);
        finish(&db, &c, &r, ok(), 15).await;
        let d = claim(&db, &r, 16).await;
        assert_eq!(d.job.name, "d");
        assert_eq!(
            finish(&db, &d, &r, ok(), 17).await,
            FinishOutcome::Recorded {
                job_status: JobStatus::Success,
                pipeline_status: PipelineStatus::Success,
                queue_changed: false
            }
        );
        let p = db.pipeline(p.id).await.expect("q").expect("p");
        assert_eq!(
            (p.status, p.started_at, p.finished_at),
            (PipelineStatus::Success, Some(11), Some(17))
        );
    }

    #[tokio::test]
    async fn dag_failure_skips_dependents() {
        let db = db().await;
        let repo = repo(&db, "alice", "app").await;
        let r = runner(&db, "r1", &["linux"], false).await;
        let p = db
            .create_pipeline(&pipeline(repo, true, diamond()), 10)
            .await
            .expect("create");
        let a = claim(&db, &r, 11).await;
        finish(&db, &a, &r, ok(), 12).await;
        let b = claim(&db, &r, 13).await;
        let c = claim(&db, &r, 13).await;
        assert_eq!(
            finish(&db, &b, &r, fail(), 14).await,
            FinishOutcome::Recorded {
                job_status: JobStatus::Failed,
                pipeline_status: PipelineStatus::Running,
                queue_changed: false
            }
        );
        let st = status_of(&db, p.id).await;
        assert_eq!((st["d"], st["c"]), (JobStatus::Skipped, JobStatus::Running));
        assert_eq!(
            finish(&db, &c, &r, ok(), 15).await,
            FinishOutcome::Recorded {
                job_status: JobStatus::Success,
                pipeline_status: PipelineStatus::Failed,
                queue_changed: false
            }
        );
        let attempt = db.attempt(b.attempt.id).await.expect("q").expect("a");
        assert_eq!(
            (attempt.status, attempt.exit_code, attempt.error.as_deref()),
            (AttemptStatus::Failed, Some(1), Some("exit status 1"))
        );
        assert!(db.claim_job(&r, 60, 16).await.expect("claim").is_none());
    }

    #[tokio::test]
    async fn retries_until_success() {
        let db = db().await;
        let repo = repo(&db, "alice", "app").await;
        let r = runner(&db, "r1", &["linux"], false).await;
        let mut j = job("flaky", &[]);
        j.max_attempts = 2;
        let p = db
            .create_pipeline(&pipeline(repo, true, vec![j]), 10)
            .await
            .expect("create");
        let first = claim(&db, &r, 11).await;
        assert_eq!(
            finish(&db, &first, &r, fail(), 12).await,
            FinishOutcome::Recorded {
                job_status: JobStatus::Queued,
                pipeline_status: PipelineStatus::Running,
                queue_changed: true
            }
        );
        let second = claim(&db, &r, 13).await;
        assert_eq!((second.job.id, second.attempt.number), (first.job.id, 2));
        finish(&db, &second, &r, ok(), 14).await;
        let attempts = db.attempts_for_job(first.job.id).await.expect("q");
        assert_eq!(
            attempts.iter().map(|a| a.status).collect::<Vec<_>>(),
            [AttemptStatus::Failed, AttemptStatus::Success]
        );
        let job = db.job(first.job.id).await.expect("q").expect("job");
        assert_eq!((job.status, job.attempts), (JobStatus::Success, 2));
        assert_eq!(
            db.pipeline(p.id).await.expect("q").expect("p").status,
            PipelineStatus::Success
        );
    }

    #[tokio::test]
    async fn lease_expiry_requeues_and_rejects_late_finish() {
        let db = db().await;
        let repo = repo(&db, "alice", "app").await;
        let r1 = runner(&db, "r1", &["linux"], false).await;
        let r2 = runner(&db, "r2", &["linux"], false).await;
        let mut j = job("build", &[]);
        j.max_attempts = 2;
        let p = db
            .create_pipeline(&pipeline(repo, true, vec![j]), 100)
            .await
            .expect("create");
        let c1 = db
            .claim_job(&r1, 30, 100)
            .await
            .expect("claim")
            .expect("job");
        assert_eq!(c1.attempt.lease_expires_at, 130);
        assert_eq!(
            db.extend_lease(c1.attempt.id, r2.id, 30, 120)
                .await
                .expect("extend"),
            LeaseState::Lost,
            "another runner's attempt"
        );
        assert_eq!(
            db.extend_lease(c1.attempt.id, r1.id, 30, 120)
                .await
                .expect("extend"),
            LeaseState::Ok
        );
        assert_eq!(
            db.expire_leases(150).await.expect("reap"),
            Reaped::default()
        );
        assert_eq!(
            db.expire_leases(151).await.expect("reap"),
            Reaped {
                lost: 1,
                requeued: 1,
                released: 0
            }
        );
        let lost = db.attempt(c1.attempt.id).await.expect("q").expect("a");
        assert_eq!(
            (lost.status, lost.error.as_deref()),
            (AttemptStatus::Lost, Some("lease expired"))
        );
        assert_eq!(
            db.extend_lease(c1.attempt.id, r1.id, 30, 152)
                .await
                .expect("extend"),
            LeaseState::Lost
        );
        assert_eq!(
            finish(&db, &c1, &r1, ok(), 152).await,
            FinishOutcome::Rejected
        );

        let c2 = db
            .claim_job(&r2, 30, 153)
            .await
            .expect("claim")
            .expect("job");
        assert_eq!((c2.job.id, c2.attempt.number), (c1.job.id, 2));
        // A stored log line shows the runner started the attempt (no free requeue below).
        db.append_log(c2.attempt.id, 0, b"started\n")
            .await
            .expect("log");
        // An expired but not yet reaped lease is not honoured either.
        assert_eq!(
            finish(&db, &c2, &r2, ok(), 184).await,
            FinishOutcome::Rejected
        );
        // Out of attempts: the second loss fails the job and the pipeline.
        assert_eq!(
            db.expire_leases(184).await.expect("reap"),
            Reaped {
                lost: 1,
                requeued: 0,
                released: 0
            }
        );
        assert_eq!(status_of(&db, p.id).await["build"], JobStatus::Failed);
        assert_eq!(
            db.pipeline(p.id).await.expect("q").expect("p").status,
            PipelineStatus::Failed
        );
    }

    #[tokio::test]
    async fn label_and_trust_matching() {
        let db = db().await;
        let repo = repo(&db, "alice", "app").await;
        let linux = runner(&db, "linux", &["linux"], false).await;
        let full = runner(&db, "full", &["docker", "linux", "gpu"], false).await;
        let host = runner(&db, "host", &["docker", "linux"], true).await;
        let mut j = job("img", &[]);
        j.labels = s(&["linux", "docker"]);

        // Untrusted pipeline: not for the trusted-only host runner.
        let untrusted = db
            .create_pipeline(&pipeline(repo, false, vec![j.clone()]), 10)
            .await
            .expect("create");
        assert!(db.claim_job(&linux, 60, 11).await.expect("q").is_none());
        assert!(db.claim_job(&host, 60, 11).await.expect("q").is_none());
        let c = claim(&db, &full, 11).await;
        assert_eq!(c.job.pipeline_id, untrusted.id);
        assert!(!c.job.trusted);

        // Trusted pipeline: host runner may take it.
        let trusted = db
            .create_pipeline(&pipeline(repo, true, vec![j]), 12)
            .await
            .expect("create");
        let c = claim(&db, &host, 13).await;
        assert_eq!(c.job.pipeline_id, trusted.id);
        assert_eq!(c.job.labels, s(&["docker", "linux"]));

        // Disabled runners get nothing.
        db.create_pipeline(&pipeline(repo, true, vec![job("x", &[])]), 14)
            .await
            .expect("create");
        db.set_runner_disabled(linux.id, true).await.expect("dis");
        let linux = db.runner(linux.id).await.expect("q").expect("r");
        assert!(db.claim_job(&linux, 60, 15).await.expect("q").is_none());
    }

    #[tokio::test]
    async fn cancel_running_pipeline() {
        let db = db().await;
        let repo = repo(&db, "alice", "app").await;
        let r = runner(&db, "r1", &["linux"], false).await;
        let p = db
            .create_pipeline(
                &pipeline(
                    repo,
                    true,
                    vec![job("a", &[]), job("b", &["a"]), job("c", &[])],
                ),
                10,
            )
            .await
            .expect("create");
        let a = claim(&db, &r, 11).await;
        assert_eq!(a.job.name, "a");
        assert_eq!(
            db.cancel_pipeline(p.id, 12).await.expect("cancel"),
            CancelOutcome {
                canceled_jobs: 2,
                running_jobs: 1,
                pipeline_status: PipelineStatus::Running
            }
        );
        assert!(db.claim_job(&r, 60, 12).await.expect("q").is_none());
        assert_eq!(
            db.extend_lease(a.attempt.id, r.id, 60, 13)
                .await
                .expect("extend"),
            LeaseState::CancelRequested
        );
        assert_eq!(
            finish(&db, &a, &r, fail(), 14).await,
            FinishOutcome::Recorded {
                job_status: JobStatus::Canceled,
                pipeline_status: PipelineStatus::Canceled,
                queue_changed: false
            }
        );
        let attempt = db.attempt(a.attempt.id).await.expect("q").expect("a");
        assert_eq!(attempt.status, AttemptStatus::Canceled);
        let p = db.pipeline(p.id).await.expect("q").expect("p");
        assert_eq!(
            (p.status, p.finished_at),
            (PipelineStatus::Canceled, Some(14))
        );
        // Cancelling a finished pipeline changes nothing.
        assert_eq!(
            db.cancel_pipeline(p.id, 15).await.expect("cancel"),
            CancelOutcome {
                canceled_jobs: 0,
                running_jobs: 0,
                pipeline_status: PipelineStatus::Canceled
            }
        );
        // Nothing running: canceled at once.
        let q = db
            .create_pipeline(&pipeline(repo, true, vec![job("a", &[])]), 16)
            .await
            .expect("create");
        assert_eq!(
            db.cancel_pipeline(q.id, 17)
                .await
                .expect("cancel")
                .pipeline_status,
            PipelineStatus::Canceled
        );
    }

    #[tokio::test]
    async fn logs_are_chunked_and_idempotent() {
        let db = db().await;
        let repo = repo(&db, "alice", "app").await;
        let r = runner(&db, "r1", &["linux"], false).await;
        db.create_pipeline(&pipeline(repo, true, vec![job("a", &[])]), 10)
            .await
            .expect("create");
        let id = claim(&db, &r, 11).await.attempt.id;

        assert_eq!(
            db.append_log(id, 0, b"hello ").await.expect("log"),
            LogAppend::Appended(6)
        );
        assert_eq!(
            db.append_log(id, 0, b"hello ").await.expect("log"),
            LogAppend::Duplicate(6)
        );
        assert_eq!(
            db.append_log(id, 10, b"late").await.expect("log"),
            LogAppend::Gap(6)
        );
        assert_eq!(
            db.append_log(id, 6, b"world")
                .await
                .expect("log")
                .committed(),
            11
        );
        // Overlapping retry: only the new tail is stored.
        assert_eq!(
            db.append_log(id, 6, b"world!!").await.expect("log"),
            LogAppend::Appended(13)
        );
        assert_eq!(db.log_len(id).await.expect("len"), 13);
        assert_eq!(
            db.read_log(id, 0, 1024).await.expect("read"),
            (b"hello world!!".to_vec(), 13)
        );
        assert_eq!(
            db.read_log(id, 3, 4).await.expect("read"),
            (b"lo w".to_vec(), 7)
        );
        assert_eq!(
            db.read_log(id, 13, 10).await.expect("read"),
            (Vec::new(), 13)
        );
        assert!(matches!(
            db.append_log(9999, 0, b"x").await,
            Err(DbError::NotFound(_))
        ));
    }

    /// S5: logs are closed once the attempt finished.
    #[tokio::test]
    async fn logs_rejected_after_attempt_finished() {
        let db = db().await;
        let repo = repo(&db, "alice", "app").await;
        let r = runner(&db, "r1", &["linux"], false).await;
        db.create_pipeline(&pipeline(repo, true, vec![job("a", &[])]), 10)
            .await
            .expect("create");
        let c = claim(&db, &r, 11).await;
        assert_eq!(
            db.append_log(c.attempt.id, 0, b"ok\n").await.expect("log"),
            LogAppend::Appended(3)
        );
        finish(&db, &c, &r, ok(), 12).await;
        assert_eq!(
            db.append_log(c.attempt.id, 3, b"late\n")
                .await
                .expect("log"),
            LogAppend::Closed(3)
        );
        assert_eq!(db.log_len(c.attempt.id).await.expect("len"), 3);
    }

    /// S5: a per-attempt cap bounds stored log bytes.
    #[tokio::test]
    async fn log_size_is_capped_per_attempt() {
        let db = db().await;
        let repo = repo(&db, "alice", "app").await;
        let r = runner(&db, "r1", &["linux"], false).await;
        db.create_pipeline(&pipeline(repo, true, vec![job("a", &[])]), 10)
            .await
            .expect("create");
        let id = claim(&db, &r, 11).await.attempt.id;
        let chunk = vec![b'x'; 1024 * 1024];
        let mut offset = 0u64;
        while offset < LogAppend::MAX_BYTES {
            assert_eq!(
                db.append_log(id, offset, &chunk).await.expect("log"),
                LogAppend::Appended(offset + chunk.len() as u64)
            );
            offset += chunk.len() as u64;
        }
        assert_eq!(offset, LogAppend::MAX_BYTES);
        assert_eq!(
            db.append_log(id, offset, b"one more").await.expect("log"),
            LogAppend::LimitExceeded(LogAppend::MAX_BYTES)
        );
        assert_eq!(db.log_len(id).await.expect("len"), LogAppend::MAX_BYTES);
    }

    /// C1: an attempt whose runner never showed life is requeued without consuming one.
    #[tokio::test]
    async fn unseen_expired_attempt_is_released_without_consuming_an_attempt() {
        let db = db().await;
        let repo = repo(&db, "alice", "app").await;
        let r = runner(&db, "r1", &["linux"], false).await;
        let p = db
            .create_pipeline(&pipeline(repo, true, vec![job("a", &[])]), 10)
            .await
            .expect("create");
        let c1 = db.claim_job(&r, 30, 11).await.expect("claim").expect("job");
        assert_eq!(
            db.expire_leases(42).await.expect("reap"),
            Reaped {
                lost: 0,
                requeued: 1,
                released: 1
            }
        );
        assert!(db.attempt(c1.attempt.id).await.expect("q").is_none());
        let j = db.job(c1.job.id).await.expect("q").expect("job");
        assert_eq!(
            (j.status, j.attempts, j.started_at),
            (JobStatus::Queued, 0, None)
        );
        assert!(matches!(
            db.finish_attempt(c1.attempt.id, r.id, &ok(), 43).await,
            Err(DbError::NotFound(_))
        ));
        // max_attempts = 1 and the job can still run and succeed.
        let c2 = claim(&db, &r, 50).await;
        assert_eq!((c2.job.id, c2.attempt.number), (c1.job.id, 1));
        finish(&db, &c2, &r, ok(), 51).await;
        assert_eq!(
            db.pipeline(p.id).await.expect("q").expect("p").status,
            PipelineStatus::Success
        );
    }

    /// C1: free requeues are bounded; afterwards the normal lost handling applies.
    #[tokio::test]
    async fn free_requeues_are_bounded() {
        let db = db().await;
        let repo = repo(&db, "alice", "app").await;
        let r = runner(&db, "r1", &["linux"], false).await;
        let p = db
            .create_pipeline(&pipeline(repo, true, vec![job("a", &[])]), 10)
            .await
            .expect("create");
        let mut now = 10;
        for _ in 0..MAX_FREE_REQUEUES {
            db.claim_job(&r, 30, now)
                .await
                .expect("claim")
                .expect("job");
            now += 31;
            assert_eq!(db.expire_leases(now).await.expect("reap").released, 1);
        }
        db.claim_job(&r, 30, now)
            .await
            .expect("claim")
            .expect("job");
        now += 31;
        let reaped = db.expire_leases(now).await.expect("reap");
        assert_eq!((reaped.lost, reaped.released, reaped.requeued), (1, 0, 0));
        assert_eq!(
            db.pipeline(p.id).await.expect("q").expect("p").status,
            PipelineStatus::Failed
        );
    }

    /// C1: an Extend counts as a sign of life.
    #[tokio::test]
    async fn seen_attempt_is_lost_not_released() {
        let db = db().await;
        let repo = repo(&db, "alice", "app").await;
        let r = runner(&db, "r1", &["linux"], false).await;
        db.create_pipeline(&pipeline(repo, true, vec![job("a", &[])]), 10)
            .await
            .expect("create");
        let c = db.claim_job(&r, 30, 11).await.expect("claim").expect("job");
        db.mark_attempt_seen(c.attempt.id, 12).await.expect("seen");
        let reaped = db.expire_leases(42).await.expect("reap");
        assert_eq!((reaped.lost, reaped.released), (1, 0));
        assert_eq!(
            status_of(&db, c.job.pipeline_id).await["a"],
            JobStatus::Failed
        );
    }

    /// C4: a claim can be given back without consuming an attempt.
    #[tokio::test]
    async fn release_claim_requeues_without_consuming() {
        let db = db().await;
        let repo = repo(&db, "alice", "app").await;
        let r1 = runner(&db, "r1", &["linux"], false).await;
        let r2 = runner(&db, "r2", &["linux"], false).await;
        db.create_pipeline(&pipeline(repo, true, vec![job("a", &[])]), 10)
            .await
            .expect("create");
        let c = claim(&db, &r1, 11).await;
        assert!(
            !db.release_claim(c.attempt.id, r2.id, 12)
                .await
                .expect("release"),
            "only the lease holder can release"
        );
        assert!(
            db.release_claim(c.attempt.id, r1.id, 12)
                .await
                .expect("release")
        );
        assert!(matches!(
            db.release_claim(c.attempt.id, r1.id, 12).await,
            Err(DbError::NotFound(_))
        ));
        let j = db.job(c.job.id).await.expect("q").expect("job");
        assert_eq!((j.status, j.attempts), (JobStatus::Queued, 0));
        assert_eq!(claim(&db, &r2, 13).await.attempt.number, 1);
    }

    /// C3: lease renewals and runner touches queue on the CI write lock.
    #[tokio::test]
    async fn extend_and_touch_take_the_write_lock() {
        let db = db().await;
        let repo = repo(&db, "alice", "app").await;
        let r = runner(&db, "r1", &["linux"], false).await;
        db.create_pipeline(&pipeline(repo, true, vec![job("a", &[])]), 10)
            .await
            .expect("create");
        let c = claim(&db, &r, 11).await;
        let guard = db.write_lock.clone().lock_owned().await;
        let short = std::time::Duration::from_millis(200);
        assert!(
            tokio::time::timeout(short, db.extend_lease(c.attempt.id, r.id, 60, 12))
                .await
                .is_err(),
            "extend_lease must wait for the write lock"
        );
        assert!(
            tokio::time::timeout(short, db.touch_runner(r.id, 12))
                .await
                .is_err(),
            "touch_runner must wait for the write lock"
        );
        drop(guard);
        assert_eq!(
            db.extend_lease(c.attempt.id, r.id, 60, 12)
                .await
                .expect("extend"),
            LeaseState::Ok
        );
        db.touch_runner(r.id, 12).await.expect("touch");
    }

    #[tokio::test]
    async fn pipeline_numbers_per_repo_and_validation() {
        let db = db().await;
        let one = repo(&db, "alice", "one").await;
        let two = repo(&db, "alice", "two").await;
        let mk = |repo| pipeline(repo, true, vec![job("a", &[])]);
        assert_eq!(db.create_pipeline(&mk(one), 1).await.expect("p").number, 1);
        assert_eq!(db.create_pipeline(&mk(one), 2).await.expect("p").number, 2);
        assert_eq!(db.create_pipeline(&mk(two), 3).await.expect("p").number, 1);
        assert_eq!(db.create_pipeline(&mk(one), 4).await.expect("p").number, 3);
        let listed = db.pipelines_for_repo(one, 2).await.expect("list");
        assert_eq!(listed.iter().map(|p| p.number).collect::<Vec<_>>(), [3, 2]);

        assert!(matches!(
            db.create_pipeline(&pipeline(one, true, vec![job("a", &["nope"])]), 5)
                .await,
            Err(DbError::NotFound(_))
        ));
        assert!(matches!(
            db.create_pipeline(
                &pipeline(one, true, vec![job("a", &["b"]), job("b", &["a"])]),
                5
            )
            .await,
            Err(DbError::Conflict(_))
        ));
        assert!(matches!(
            db.create_pipeline(&pipeline(one, true, vec![job("a", &[]), job("a", &[])]), 5)
                .await,
            Err(DbError::Conflict(_))
        ));
        assert!(matches!(
            db.create_pipeline(&mk(9999), 5).await,
            Err(DbError::NotFound("repository"))
        ));
        // Rejected creations did not consume numbers.
        assert_eq!(db.create_pipeline(&mk(one), 6).await.expect("p").number, 4);
        let empty = db
            .create_pipeline(&pipeline(two, true, vec![]), 7)
            .await
            .expect("empty");
        assert_eq!(
            (empty.status, empty.finished_at),
            (PipelineStatus::Success, Some(7))
        );
    }

    #[tokio::test]
    async fn runner_registry() {
        let db = db().await;
        let r = db
            .register_runner("r1", "h1", &s(&["linux", "docker", "linux"]), true, 5)
            .await
            .expect("reg");
        assert_eq!(r.labels, s(&["docker", "linux"]));
        assert!(matches!(
            db.register_runner("r2", "h1", &[], false, 5).await,
            Err(DbError::Conflict("runner"))
        ));
        db.touch_runner(r.id, 9).await.expect("touch");
        let found = db
            .runner_by_token_hash("h1")
            .await
            .expect("q")
            .expect("runner");
        assert_eq!((found.id, found.last_seen_at), (r.id, Some(9)));
        assert!(db.runner_by_token_hash("nope").await.expect("q").is_none());
        assert_eq!(db.list_runners().await.expect("list").len(), 1);
    }

    /// File-backed WAL database, 50 jobs, 8 runners claiming concurrently: every job is
    /// claimed exactly once and nothing errors (SQLITE_BUSY is absorbed by BEGIN IMMEDIATE
    /// + busy_timeout).
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_claims_are_exclusive() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Db::open(&dir.path().join("ci.db")).await.expect("open");
        let repo = repo(&db, "alice", "app").await;
        let jobs = (0..50).map(|i| job(&format!("j{i}"), &[])).collect();
        let p = db
            .create_pipeline(&pipeline(repo, true, jobs), 1)
            .await
            .expect("create");

        let mut tasks = Vec::new();
        for i in 0..8 {
            let db = db.clone();
            let r = runner(&db, &format!("r{i}"), &["linux"], false).await;
            tasks.push(tokio::spawn(async move {
                let mut mine = Vec::new();
                while let Some(c) = db.claim_job(&r, 60, 2).await.expect("claim") {
                    db.append_log(c.attempt.id, 0, b"ok\n").await.expect("log");
                    let done = db
                        .finish_attempt(c.attempt.id, r.id, &ok(), 3)
                        .await
                        .expect("finish");
                    assert!(matches!(done, FinishOutcome::Recorded { .. }));
                    mine.push(c.job.id);
                    tokio::task::yield_now().await;
                }
                mine
            }));
        }
        let mut all = Vec::new();
        for t in tasks {
            all.extend(t.await.expect("task"));
        }
        let unique: HashSet<_> = all.iter().copied().collect();
        assert_eq!((all.len(), unique.len()), (50, 50));
        for j in db.jobs_for_pipeline(p.id).await.expect("jobs") {
            assert_eq!((j.status, j.attempts), (JobStatus::Success, 1));
            assert_eq!(db.attempts_for_job(j.id).await.expect("a").len(), 1);
        }
        assert_eq!(
            db.pipeline(p.id).await.expect("q").expect("p").status,
            PipelineStatus::Success
        );
        db.close().await;
    }
}
