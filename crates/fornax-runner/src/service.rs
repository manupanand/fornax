// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! The `Runner` trait implementation.

use std::time::Duration;

use fornax_core::ci::LeaseState;
use fornax_core::now_unix;
use fornax_db::{Claimed, DbError, FinishOutcome, LogAppend, Outcome, Runner as DbRunner};
use fornax_proto::runner::v1::runner_server::Runner;
use fornax_proto::runner::v1::{
    DoneRequest, DoneResponse, ExtendRequest, ExtendResponse, ExtendState, Job, JobResult, LogAck,
    LogChunk, NextRequest, NextResponse, RegisterRequest, RegisterResponse, UpdateRequest,
    UpdateResponse,
};
use fornax_proto::{AUTHORIZATION_HEADER, is_runner_token_format};
use sha2::{Digest, Sha256};
use tokio::time::Instant;
use tonic::metadata::MetadataMap;
use tonic::{Request, Response, Status, Streaming};

use crate::payload::JobPayload;
use crate::{
    MAX_TRACKED_ATTEMPTS, MAX_TRACKED_STEPS, RunnerService, StepRecord, create_runner,
    generate_job_token, hash_token,
};

/// Skip `touch_runner` when the runner was seen this recently (saves a write per call).
const TOUCH_INTERVAL_SECS: i64 = 5;
/// Largest accepted log chunk (the agent sends ≤ 64 KiB).
const MAX_LOG_CHUNK: usize = 1024 * 1024;
/// Longest stored error message from `Done`.
const MAX_ERROR_LEN: usize = 4096;

/// Why a claimed job could not be turned into a `Job` message.
#[derive(Debug)]
pub(crate) enum DispatchError {
    /// The stored payload does not decode: permanent, the job is failed.
    Payload(String),
    /// Anything else (DB error, random source, …): the claim is released.
    Transient(String),
}

fn unauthenticated() -> Status {
    Status::unauthenticated("missing or invalid runner token")
}

/// Map a database error to a gRPC status without leaking details to the agent.
fn db_status(err: DbError) -> Status {
    match err {
        DbError::NotFound(what) => Status::not_found(format!("{what} not found")),
        DbError::Conflict(what) => Status::already_exists(format!("{what} already exists")),
        other => {
            tracing::error!(error = %other, "database error in runner service");
            Status::internal("internal error")
        }
    }
}

fn parse_attempt(id: &str) -> Option<i64> {
    id.parse::<i64>().ok().filter(|n| *n > 0)
}

/// The client's deadline from the `grpc-timeout` header (`<1-8 digits><H|M|S|m|u|n>`).
pub(crate) fn client_timeout(md: &MetadataMap) -> Option<Duration> {
    let v = md.get("grpc-timeout")?.to_str().ok()?;
    if !(2..=9).contains(&v.len()) {
        return None;
    }
    let (num, unit) = v.split_at(v.len() - 1);
    let n: u64 = num.parse().ok()?;
    Some(match unit {
        "H" => Duration::from_secs(n.saturating_mul(3600)),
        "M" => Duration::from_secs(n.saturating_mul(60)),
        "S" => Duration::from_secs(n),
        "m" => Duration::from_millis(n),
        "u" => Duration::from_micros(n),
        "n" => Duration::from_nanos(n),
        _ => return None,
    })
}

/// How long `Next` may wait: `max`, but answer a margin (≥ 1 s or 10 %) before the client's
/// deadline so the empty response arrives instead of `DEADLINE_EXCEEDED`.
pub(crate) fn poll_wait(md: &MetadataMap, max: Duration) -> Duration {
    match client_timeout(md) {
        Some(t) => {
            let margin = (t / 10).max(Duration::from_secs(1));
            max.min(t.saturating_sub(margin))
        }
        None => max,
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_owned()
}

/// Constant-time equality of two secrets (compares their SHA-256 digests).
fn secret_eq(a: &str, b: &str) -> bool {
    let (da, db) = (Sha256::digest(a.as_bytes()), Sha256::digest(b.as_bytes()));
    da.iter()
        .zip(db.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

impl RunnerService {
    /// Authenticate the runner from `authorization: Bearer fnr_…`.
    async fn authenticate(&self, md: &MetadataMap) -> Result<DbRunner, Status> {
        let value = md
            .get(AUTHORIZATION_HEADER)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(unauthenticated)?;
        let token = value
            .strip_prefix("Bearer ")
            .map(str::trim)
            .filter(|t| is_runner_token_format(t))
            .ok_or_else(unauthenticated)?;
        let runner = self
            .inner
            .db
            .runner_by_token_hash(&hash_token(token))
            .await
            .map_err(db_status)?
            .filter(|r| !r.disabled)
            .ok_or_else(unauthenticated)?;
        let now = now_unix();
        if runner
            .last_seen_at
            .is_none_or(|t| now.saturating_sub(t) >= TOUCH_INTERVAL_SECS)
            && let Err(e) = self.inner.db.touch_runner(runner.id, now).await
        {
            tracing::warn!(runner = runner.id, error = %e, "touch_runner failed");
        }
        Ok(runner)
    }

    /// The attempt must exist (`NOT_FOUND`) and belong to `runner` (`PERMISSION_DENIED`).
    async fn owned_attempt(&self, runner: &DbRunner, attempt_id: &str) -> Result<i64, Status> {
        let id = parse_attempt(attempt_id).ok_or_else(|| Status::not_found("unknown attempt"))?;
        let attempt = self
            .inner
            .db
            .attempt(id)
            .await
            .map_err(db_status)?
            .ok_or_else(|| Status::not_found("unknown attempt"))?;
        if attempt.runner_id != Some(runner.id) {
            return Err(Status::permission_denied(
                "attempt belongs to another runner",
            ));
        }
        Ok(id)
    }

    /// Turn a claimed DB job into the protobuf `Job` and mint its job token.
    async fn build_job(&self, claimed: &Claimed, now: i64) -> Result<Job, DispatchError> {
        let db = &self.inner.db;
        let cfg = &self.inner.config;
        let transient = |e: &dyn std::fmt::Display| DispatchError::Transient(e.to_string());
        let pipeline = db
            .pipeline(claimed.job.pipeline_id)
            .await
            .map_err(|e| transient(&e))?
            .ok_or_else(|| transient(&"pipeline not found"))?;
        let repo = db
            .repo_by_id(pipeline.repo_id)
            .await
            .map_err(|e| transient(&e))?
            .ok_or_else(|| transient(&"repository not found"))?;
        let payload = JobPayload::parse(&claimed.job.payload)
            .map_err(|e| DispatchError::Payload(format!("invalid job payload: {e}")))?;
        let (token, hash) = generate_job_token().map_err(|e| transient(&e))?;
        let budget = if payload.timeout_secs > 0 {
            payload.timeout_secs
        } else {
            cfg.default_job_timeout_secs
        };
        let expires_at = now
            .saturating_add(cfg.lease_secs)
            .saturating_add(i64::try_from(budget).unwrap_or(i64::MAX));
        db.create_job_token(claimed.attempt.id, repo.id, &hash, expires_at)
            .await
            .map_err(|e| transient(&e))?;
        let clone_url = if cfg.checkout {
            format!(
                "{}/{}/{}.git",
                cfg.public_url.trim_end_matches('/'),
                repo.owner,
                repo.name
            )
        } else {
            String::new()
        };
        let name = if payload.name.is_empty() {
            claimed.job.name.clone()
        } else {
            payload.name.clone()
        };
        Ok(Job {
            attempt_id: claimed.attempt.id.to_string(),
            job_id: claimed.job.id.to_string(),
            pipeline_id: pipeline.id.to_string(),
            name,
            repo_full_name: format!("{}/{}", repo.owner, repo.name),
            clone_url,
            sha: pipeline.sha,
            r#ref: pipeline.git_ref,
            job_token: token.clone(),
            env: payload.proto_env(),
            steps: payload.proto_steps(),
            timeout_secs: payload.timeout_secs,
            mask: vec![token],
            secrets: Default::default(),
        })
    }

    /// A claimed job that could not be built. A bad payload fails it right away (retrying
    /// cannot help). Anything else is treated as transient: the claim is given back without
    /// consuming an attempt (bounded by the DB layer; past the bound the job is failed) and
    /// the caller answers `UNAVAILABLE` so the agent backs off.
    pub(crate) async fn handle_dispatch_error(
        &self,
        runner: &DbRunner,
        claimed: &Claimed,
        err: &DispatchError,
        now: i64,
    ) {
        match err {
            DispatchError::Payload(msg) => self.fail_claim(runner, claimed, msg, now).await,
            DispatchError::Transient(msg) => {
                let attempt = claimed.attempt.id;
                match self.inner.db.release_claim(attempt, runner.id, now).await {
                    Ok(true) => {
                        tracing::warn!(attempt, job = claimed.job.id, error = %msg,
                            "cannot dispatch job right now; claim released");
                        self.inner.notifier.notify();
                    }
                    Ok(false) => self.fail_claim(runner, claimed, msg, now).await,
                    Err(e) => {
                        // The lease expires and the reaper releases the unseen attempt.
                        tracing::error!(attempt, error = %e, "releasing claim failed");
                    }
                }
            }
        }
    }

    /// A claimed job that cannot be handed out is failed right away.
    async fn fail_claim(&self, runner: &DbRunner, claimed: &Claimed, error: &str, now: i64) {
        let db = &self.inner.db;
        let attempt = claimed.attempt.id;
        tracing::error!(attempt, job = claimed.job.id, %error, "cannot dispatch job");
        let _ = db.revoke_job_token(attempt).await;
        let outcome = Outcome {
            success: false,
            exit_code: None,
            error: Some(truncate(error, MAX_ERROR_LEN)),
        };
        match db.finish_attempt(attempt, runner.id, &outcome, now).await {
            Ok(FinishOutcome::Recorded {
                queue_changed: true,
                ..
            }) => self.inner.notifier.notify(),
            Ok(_) => {}
            Err(e) => tracing::error!(attempt, error = %e, "failing undispatchable job"),
        }
    }
}

#[tonic::async_trait]
impl Runner for RunnerService {
    async fn register(
        &self,
        req: Request<RegisterRequest>,
    ) -> Result<Response<RegisterResponse>, Status> {
        let Some(expected) = self.inner.config.registration_token.as_deref() else {
            return Err(Status::permission_denied(
                "runner self-registration is disabled; use `fornax admin create-runner`",
            ));
        };
        let r = req.into_inner();
        if !secret_eq(&r.registration_token, expected) {
            return Err(Status::permission_denied("invalid registration token"));
        }
        // Self-registered runners are always restricted to trusted jobs (D-008).
        let (runner, token) = create_runner(&self.inner.db, &r.name, &r.labels, true, now_unix())
            .await
            .map_err(|e| match e {
                crate::RunnerError::InvalidName(_) | crate::RunnerError::InvalidLabel(_) => {
                    Status::invalid_argument(e.to_string())
                }
                crate::RunnerError::Db(e) => db_status(e),
                crate::RunnerError::Random(_) => Status::internal("internal error"),
            })?;
        tracing::info!(
            runner = runner.id,
            name = %runner.name,
            version = %r.version,
            os = %r.os,
            arch = %r.arch,
            "runner registered"
        );
        let detail = serde_json::json!({ "source": "register", "runner_id": runner.id });
        if let Err(e) = self
            .inner
            .db
            .audit(None, "runner.register", &runner.name, &detail.to_string())
            .await
        {
            tracing::warn!(error = %e, "audit failed");
        }
        Ok(Response::new(RegisterResponse {
            runner_id: runner.id.to_string(),
            runner_token: token,
        }))
    }

    async fn next(&self, req: Request<NextRequest>) -> Result<Response<NextResponse>, Status> {
        let runner = self.authenticate(req.metadata()).await?;
        let wait = poll_wait(req.metadata(), self.inner.config.max_poll_wait);
        let deadline = Instant::now() + wait;
        if req.get_ref().capacity_free == 0 {
            return Ok(Response::new(NextResponse { job: None }));
        }
        let notify = self.inner.notifier.as_notify();
        loop {
            // Register for wake-ups *before* looking at the queue, so a job created between
            // the claim attempt and the wait is not missed.
            let notified = notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.inner.stopping.is_cancelled() {
                return Ok(Response::new(NextResponse { job: None }));
            }
            let now = now_unix();
            let claimed = self
                .inner
                .db
                .claim_job(&runner, self.inner.config.lease_secs, now)
                .await
                .map_err(db_status)?;
            if let Some(claimed) = claimed {
                match self.build_job(&claimed, now).await {
                    Ok(job) => {
                        tracing::info!(
                            runner = runner.id,
                            attempt = claimed.attempt.id,
                            job = claimed.job.id,
                            "job dispatched"
                        );
                        return Ok(Response::new(NextResponse { job: Some(job) }));
                    }
                    Err(e @ DispatchError::Payload(_)) => {
                        self.handle_dispatch_error(&runner, &claimed, &e, now).await;
                        continue;
                    }
                    Err(e) => {
                        self.handle_dispatch_error(&runner, &claimed, &e, now).await;
                        return Err(Status::unavailable("cannot dispatch a job right now"));
                    }
                }
            }
            tokio::select! {
                () = &mut notified => {}
                () = self.inner.stopping.cancelled() => {
                    return Ok(Response::new(NextResponse { job: None }));
                }
                () = tokio::time::sleep_until(deadline) => {
                    return Ok(Response::new(NextResponse { job: None }));
                }
            }
        }
    }

    async fn extend(
        &self,
        req: Request<ExtendRequest>,
    ) -> Result<Response<ExtendResponse>, Status> {
        let runner = self.authenticate(req.metadata()).await?;
        let state = match parse_attempt(&req.get_ref().attempt_id) {
            None => ExtendState::Lost,
            Some(id) => {
                let lease = self
                    .inner
                    .db
                    .extend_lease(id, runner.id, self.inner.config.lease_secs, now_unix())
                    .await
                    .map_err(db_status)?;
                match lease {
                    LeaseState::Ok => ExtendState::Ok,
                    LeaseState::CancelRequested => ExtendState::Cancel,
                    LeaseState::Lost => ExtendState::Lost,
                }
            }
        };
        Ok(Response::new(ExtendResponse {
            state: state as i32,
        }))
    }

    async fn update(
        &self,
        req: Request<UpdateRequest>,
    ) -> Result<Response<UpdateResponse>, Status> {
        let runner = self.authenticate(req.metadata()).await?;
        let u = req.into_inner();
        let id = self.owned_attempt(&runner, &u.attempt_id).await?;
        // Step updates start after the checkout (or replace it when it failed), so the job
        // token has served its purpose: revoke it before any step can use it (S1).
        let first_update = !self
            .inner
            .steps
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(&id);
        if first_update {
            self.inner
                .db
                .mark_attempt_seen(id, now_unix())
                .await
                .map_err(db_status)?;
            self.inner
                .db
                .revoke_job_token(id)
                .await
                .map_err(db_status)?;
        }
        let record = StepRecord {
            state: u.step_state(),
            started_at: u.started_at,
            finished_at: u.finished_at,
            exit_code: u.exit_code,
        };
        tracing::debug!(attempt = id, step = u.step_index, state = ?record.state, "step update");
        let mut steps = self
            .inner
            .steps
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if steps.len() < MAX_TRACKED_ATTEMPTS || steps.contains_key(&id) {
            let per = steps.entry(id).or_default();
            if per.len() < MAX_TRACKED_STEPS || per.contains_key(&u.step_index) {
                per.insert(u.step_index, record);
            }
        }
        Ok(Response::new(UpdateResponse {}))
    }

    async fn log(&self, req: Request<Streaming<LogChunk>>) -> Result<Response<LogAck>, Status> {
        let runner = self.authenticate(req.metadata()).await?;
        let mut stream = req.into_inner();
        let mut verified: Option<(String, i64)> = None;
        let mut committed = 0u64;
        while let Some(chunk) = stream.message().await? {
            let id = match &verified {
                Some((raw, id)) if *raw == chunk.attempt_id => *id,
                _ => {
                    let id = self.owned_attempt(&runner, &chunk.attempt_id).await?;
                    verified = Some((chunk.attempt_id.clone(), id));
                    id
                }
            };
            if chunk.data.len() > MAX_LOG_CHUNK {
                return Err(Status::invalid_argument("log chunk too large"));
            }
            match self
                .inner
                .db
                .append_log(id, chunk.offset, &chunk.data)
                .await
                .map_err(db_status)?
            {
                LogAppend::Appended(n) | LogAppend::Duplicate(n) => committed = n,
                // Resend from the committed end; later chunks of this stream would gap too.
                LogAppend::Gap(n) => {
                    return Ok(Response::new(LogAck {
                        committed_offset: n,
                    }));
                }
                LogAppend::Closed(_) => {
                    return Err(Status::failed_precondition(
                        "attempt is not running; its log is closed",
                    ));
                }
                LogAppend::LimitExceeded(_) => {
                    return Err(Status::resource_exhausted(format!(
                        "log limit of {} bytes per attempt reached",
                        LogAppend::MAX_BYTES
                    )));
                }
            }
        }
        Ok(Response::new(LogAck {
            committed_offset: committed,
        }))
    }

    async fn done(&self, req: Request<DoneRequest>) -> Result<Response<DoneResponse>, Status> {
        let runner = self.authenticate(req.metadata()).await?;
        let d = req.into_inner();
        let id =
            parse_attempt(&d.attempt_id).ok_or_else(|| Status::not_found("unknown attempt"))?;
        let result = d.result();
        let mut error = truncate(&d.error, MAX_ERROR_LEN);
        if result == JobResult::Unspecified && error.is_empty() {
            error = "agent reported no result".into();
        }
        let outcome = Outcome {
            success: result == JobResult::Success,
            exit_code: Some(d.exit_code),
            error: (!error.is_empty()).then_some(error),
        };
        let finished = self
            .inner
            .db
            .finish_attempt(id, runner.id, &outcome, now_unix())
            .await
            .map_err(db_status)?;
        match finished {
            FinishOutcome::Rejected => Err(Status::failed_precondition(
                "attempt is not running on this runner (lease lost or already finished)",
            )),
            FinishOutcome::Recorded {
                job_status,
                pipeline_status,
                queue_changed,
            } => {
                if let Err(e) = self.inner.db.revoke_job_token(id).await {
                    tracing::warn!(attempt = id, error = %e, "revoking job token failed");
                }
                self.forget_steps(id);
                if queue_changed {
                    self.inner.notifier.notify();
                }
                tracing::info!(
                    attempt = id,
                    job = %job_status.as_str(),
                    pipeline = %pipeline_status.as_str(),
                    "attempt finished"
                );
                Ok(Response::new(DoneResponse {}))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn md(timeout: &str) -> MetadataMap {
        let mut m = MetadataMap::new();
        m.insert("grpc-timeout", timeout.parse().expect("ascii"));
        m
    }

    #[test]
    fn parses_grpc_timeout() {
        assert_eq!(client_timeout(&md("35S")), Some(Duration::from_secs(35)));
        assert_eq!(client_timeout(&md("5000m")), Some(Duration::from_secs(5)));
        assert_eq!(client_timeout(&md("1M")), Some(Duration::from_secs(60)));
        assert_eq!(client_timeout(&md("7")), None);
        assert_eq!(client_timeout(&md("5x")), None);
        assert_eq!(client_timeout(&md("123456789S")), None);
        assert_eq!(client_timeout(&MetadataMap::new()), None);
    }

    #[test]
    fn poll_wait_respects_deadline() {
        let max = Duration::from_secs(25);
        assert_eq!(poll_wait(&md("35S"), max), max);
        assert_eq!(poll_wait(&md("5S"), max), Duration::from_secs(4));
        assert_eq!(poll_wait(&md("20S"), max), Duration::from_secs(18));
        assert_eq!(poll_wait(&md("500m"), max), Duration::ZERO);
        assert_eq!(poll_wait(&MetadataMap::new(), max), max);
    }

    async fn claimed_env() -> (RunnerService, DbRunner, Claimed) {
        use fornax_db::{Db, NewJob, NewPipeline, NewRepo};
        let db = Db::open_in_memory().await.expect("db");
        let user = db.create_user("alice", false).await.expect("user");
        let repo = db
            .create_repo(&NewRepo {
                owner_id: user.id,
                name: "demo",
                description: "",
                is_private: true,
                default_branch: "main",
            })
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
                triggered_by: None,
                jobs: vec![NewJob {
                    name: "build".into(),
                    labels: vec![],
                    payload: "{}".into(),
                    needs: vec![],
                    max_attempts: 1,
                }],
            },
            now_unix(),
        )
        .await
        .expect("pipeline");
        let (runner, _) = crate::create_runner(&db, "r1", &["linux"], true, now_unix())
            .await
            .expect("runner");
        let svc = RunnerService::new(
            db.clone(),
            crate::RunnerConfig::new("http://x"),
            crate::QueueNotifier::new(),
        );
        let claimed = db
            .claim_job(&runner, 60, now_unix())
            .await
            .expect("claim")
            .expect("job");
        (svc, runner, claimed)
    }

    /// C4: a transient build error gives the claim back without consuming an attempt.
    #[tokio::test]
    async fn transient_dispatch_error_releases_claim() {
        let (svc, runner, claimed) = claimed_env().await;
        let err = DispatchError::Transient("database is locked".into());
        svc.handle_dispatch_error(&runner, &claimed, &err, now_unix())
            .await;
        let db = &svc.inner.db;
        let job = db.job(claimed.job.id).await.expect("q").expect("job");
        assert_eq!(
            (job.status, job.attempts),
            (fornax_core::ci::JobStatus::Queued, 0)
        );
        assert!(db.attempt(claimed.attempt.id).await.expect("q").is_none());
        let again = db
            .claim_job(&runner, 60, now_unix())
            .await
            .expect("claim")
            .expect("requeued job");
        assert_eq!(again.attempt.number, 1);
    }

    /// C4: a payload error is permanent and fails the job.
    #[tokio::test]
    async fn payload_dispatch_error_fails_job() {
        let (svc, runner, claimed) = claimed_env().await;
        let err = DispatchError::Payload("invalid job payload: x".into());
        svc.handle_dispatch_error(&runner, &claimed, &err, now_unix())
            .await;
        let job = svc
            .inner
            .db
            .job(claimed.job.id)
            .await
            .expect("q")
            .expect("job");
        assert_eq!(job.status, fornax_core::ci::JobStatus::Failed);
    }

    #[test]
    fn helpers() {
        assert_eq!(truncate("héllo", 2), "h");
        assert_eq!(truncate("abc", 10), "abc");
        assert!(secret_eq("a", "a"));
        assert!(!secret_eq("a", "b"));
        assert_eq!(parse_attempt("12"), Some(12));
        assert_eq!(parse_attempt("-1"), None);
        assert_eq!(parse_attempt("x"), None);
    }
}
