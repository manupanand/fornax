// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! Server side of the Fornax runner protocol (C-005/C-006 integration, LOG D-014, D-015).
//!
//! [`RunnerService`] implements the generated `fornax.runner.v1.Runner` gRPC service on top
//! of the CI queue in `fornax-db`:
//!
//! | RPC        | Server behaviour |
//! |------------|------------------|
//! | `Register` | exchanges the server-configured registration token for a runner token (`fnr_…`); disabled (`PERMISSION_DENIED`) when none is configured |
//! | `Next`     | long-poll: claims a job (`claim_job`), else waits on the [`QueueNotifier`] up to ≈ min(25 s, client deadline − margin) and answers with no job |
//! | `Extend`   | renews the attempt lease (`OK` / `CANCEL` / `LOST`) |
//! | `Update`   | step state, kept in memory per attempt ([`RunnerService::step_states`]); the first one revokes the job token (checkout is over) |
//! | `Log`      | idempotent, offset-based append after checking the attempt belongs to the caller; only while the attempt is `running` (`FAILED_PRECONDITION` after), at most [`fornax_db::LogAppend::MAX_BYTES`] per attempt (`RESOURCE_EXHAUSTED`) |
//! | `Done`     | records the result, revokes the job token, wakes runners if jobs became claimable |
//!
//! Every RPC except `Register` needs `authorization: Bearer fnr_…` of an enabled runner.
//!
//! Each claim mints a **job token** (`fnj_<64 hex>`, only its SHA-256 is stored) that grants
//! read access to the pipeline's repository for the git checkout (D-018). It is revoked on the
//! attempt's first step `Update` (the agent sends it only after the checkout finished), on
//! `Done`, and when the lease is lost. The job payload is decoded with `fornax_ci`'s format
//! (see the `payload` module).
//!
//! A claim whose job cannot be built for a transient reason is given back without consuming
//! an attempt; only an undecodable payload fails the job. An attempt whose runner never
//! showed life (lost `Next` reply) is released the same way by the reaper.
//!
//! # Security: runner trust
//!
//! The registration token (`--runner-registration-token`) and every runner token grant
//! **instance-wide** job access: any registered runner can claim any trusted job of any
//! repository (subject to labels), and receives that job's secrets and a job token for its
//! repository. Runners are not yet scoped to an owner or repository. Host runners therefore
//! must only serve **trusted, single-tenant** workloads, and the registration token must be
//! handled like an admin credential (or left unset, using `fornax admin create-runner`).
//!
//! The service is mounted on the main HTTP port with [`RunnerService::router`] (gRPC over
//! h2c/h2 next to the REST API). [`RunnerService::spawn_reaper`] expires leases and prunes job
//! tokens every [`DEFAULT_REAP_INTERVAL`].

mod payload;
mod service;

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fornax_db::{Db, DbError, Reaped, Runner};
use fornax_proto::runner::v1::StepState;
use fornax_proto::runner::v1::runner_server::RunnerServer;
use sha2::{Digest, Sha256};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tonic::server::NamedService;

pub use fornax_proto::RUNNER_TOKEN_PREFIX;
pub use payload::PAYLOAD_VERSION;

/// Prefix of job tokens (`fnj_` + 64 lowercase hex). Must match `fornax-api`'s
/// `JOB_TOKEN_PREFIX`.
pub const JOB_TOKEN_PREFIX: &str = "fnj_";
/// Default lease: 60 s = 6 × the agent's default 10 s heartbeat (D-014 asks for ≥ 3 ×; the
/// extra slack absorbs slow DB writes and GC pauses on busy runners).
pub const DEFAULT_LEASE_SECS: i64 = 60;
/// Longest time `Next` holds a call open.
pub const DEFAULT_MAX_POLL_WAIT: Duration = Duration::from_secs(25);
/// Lease reaper period.
pub const DEFAULT_REAP_INTERVAL: Duration = Duration::from_secs(15);
/// Job-token lifetime budget for jobs whose payload has no timeout.
pub const DEFAULT_JOB_TIMEOUT_SECS: u64 = 3600;
/// Longest runner name / label.
pub const MAX_NAME_LEN: usize = 64;

/// Errors of the non-RPC API (runner creation, reaper).
#[derive(Debug, thiserror::Error)]
pub enum RunnerError {
    /// Runner name not allowed.
    #[error("invalid runner name {0:?}: 1-64 characters [A-Za-z0-9._-], starting alphanumeric")]
    InvalidName(String),
    /// Label not allowed.
    #[error("invalid label {0:?}: 1-64 characters [A-Za-z0-9._:-], starting alphanumeric")]
    InvalidLabel(String),
    /// The OS random source failed.
    #[error("random source failed: {0}")]
    Random(String),
    /// Database error.
    #[error(transparent)]
    Db(#[from] DbError),
}

impl From<getrandom::Error> for RunnerError {
    fn from(e: getrandom::Error) -> Self {
        Self::Random(e.to_string())
    }
}

/// Runner-service configuration.
#[derive(Clone)]
pub struct RunnerConfig {
    /// Base URL agents use for git checkout (`<public_url>/<owner>/<repo>.git`).
    pub public_url: String,
    /// Attempt lease in seconds (renewed by `Extend`). Default [`DEFAULT_LEASE_SECS`].
    pub lease_secs: i64,
    /// Longest `Next` wait. Default [`DEFAULT_MAX_POLL_WAIT`].
    pub max_poll_wait: Duration,
    /// Shared secret accepted by `Register`; `None` disables self-registration (admins use
    /// `fornax admin create-runner`).
    pub registration_token: Option<String>,
    /// Send a clone URL so the agent checks out the commit. `false` sends an empty
    /// `clone_url` (the agent then skips checkout) — for tests and checkout-less jobs.
    pub checkout: bool,
    /// Assumed job duration for the job-token expiry when the payload sets no timeout.
    pub default_job_timeout_secs: u64,
}

impl RunnerConfig {
    /// Defaults for a server reachable at `public_url`.
    pub fn new(public_url: impl Into<String>) -> Self {
        Self {
            public_url: public_url.into(),
            lease_secs: DEFAULT_LEASE_SECS,
            max_poll_wait: DEFAULT_MAX_POLL_WAIT,
            registration_token: None,
            checkout: true,
            default_job_timeout_secs: DEFAULT_JOB_TIMEOUT_SECS,
        }
    }
}

impl std::fmt::Debug for RunnerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunnerConfig")
            .field("public_url", &self.public_url)
            .field("lease_secs", &self.lease_secs)
            .field("max_poll_wait", &self.max_poll_wait)
            .field(
                "registration_token",
                &self.registration_token.as_ref().map(|_| "<redacted>"),
            )
            .field("checkout", &self.checkout)
            .field("default_job_timeout_secs", &self.default_job_timeout_secs)
            .finish()
    }
}

/// Wakes runners waiting in `Next`. Cheap to clone; call [`QueueNotifier::notify`] after
/// anything that can make a job claimable (`create_pipeline`, a retry, `queue_changed`).
///
/// It wraps a plain `Arc<tokio::sync::Notify>` so crates that must not depend on this one
/// (e.g. the HTTP layer) can share the same `Notify` ([`QueueNotifier::from_notify`]).
#[derive(Debug, Clone, Default)]
pub struct QueueNotifier(Arc<Notify>);

impl QueueNotifier {
    /// A new notifier.
    pub fn new() -> Self {
        Self::default()
    }

    /// Use an existing `Notify` (shared with code that only knows `tokio::sync::Notify`).
    pub fn from_notify(notify: Arc<Notify>) -> Self {
        Self(notify)
    }

    /// The underlying `Notify`; `notify_waiters()` on it equals [`QueueNotifier::notify`].
    pub fn as_notify(&self) -> Arc<Notify> {
        self.0.clone()
    }

    /// Wake every runner currently waiting in `Next` so it re-checks the queue.
    pub fn notify(&self) {
        self.0.notify_waiters();
    }
}

/// Last reported state of one step (from `Update`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StepRecord {
    pub state: StepState,
    /// Unix milliseconds; 0 = unset.
    pub started_at: i64,
    /// Unix milliseconds; 0 = unset.
    pub finished_at: i64,
    pub exit_code: i32,
}

/// Step states of running attempts, bounded so a misbehaving runner can't exhaust memory.
pub(crate) const MAX_TRACKED_ATTEMPTS: usize = 10_000;
pub(crate) const MAX_TRACKED_STEPS: usize = 1_000;

pub(crate) type StepMap = HashMap<i64, BTreeMap<u32, StepRecord>>;

pub(crate) struct Inner {
    pub db: Db,
    pub config: RunnerConfig,
    pub notifier: QueueNotifier,
    pub steps: Mutex<StepMap>,
    pub stopping: CancellationToken,
}

/// The `fornax.runner.v1.Runner` gRPC service. Cheap to clone.
#[derive(Clone)]
pub struct RunnerService {
    pub(crate) inner: Arc<Inner>,
}

impl std::fmt::Debug for RunnerService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunnerService")
            .field("config", &self.inner.config)
            .finish_non_exhaustive()
    }
}

impl RunnerService {
    /// A service over `db`, waking `Next` calls through `notifier`.
    pub fn new(db: Db, config: RunnerConfig, notifier: QueueNotifier) -> Self {
        Self {
            inner: Arc::new(Inner {
                db,
                config,
                notifier,
                steps: Mutex::new(HashMap::new()),
                stopping: CancellationToken::new(),
            }),
        }
    }

    /// The notifier `Next` waits on (same handle as passed to [`RunnerService::new`]).
    pub fn notifier(&self) -> QueueNotifier {
        self.inner.notifier.clone()
    }

    /// The configuration.
    pub fn config(&self) -> &RunnerConfig {
        &self.inner.config
    }

    /// Make pending and future `Next` calls return immediately (no job), e.g. before a
    /// graceful HTTP shutdown so long-polls don't hold it up.
    pub fn shutdown(&self) {
        self.inner.stopping.cancel();
    }

    /// The tonic server for this service (e.g. for `tonic::transport::Server`).
    pub fn into_server(self) -> RunnerServer<Self> {
        RunnerServer::new(self)
    }

    /// An axum router serving the gRPC service at `/fornax.runner.v1.Runner/*`, to be
    /// `merge`d into the main HTTP router (it has no fallback, so merging is allowed).
    /// gRPC needs HTTP/2: enable axum's `http2` feature; `axum::serve` then accepts h2c
    /// (prior knowledge) next to HTTP/1.1 on the same port.
    pub fn router(self) -> axum::Router {
        let path = format!("/{}/{{*rest}}", RunnerServer::<Self>::NAME);
        axum::Router::new().route_service(&path, self.into_server())
    }

    /// Last reported step states of an attempt, by step index (empty once it finished).
    pub fn step_states(&self, attempt_id: i64) -> BTreeMap<u32, StepRecord> {
        self.inner
            .steps
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&attempt_id)
            .cloned()
            .unwrap_or_default()
    }

    /// One reaper pass at `now`: expire leases (waking runners on requeue), prune job tokens
    /// of finished/lost/expired attempts and step states of attempts no longer running.
    pub async fn reap_once(&self, now: i64) -> Result<Reaped, RunnerError> {
        let db = &self.inner.db;
        let reaped = db.expire_leases(now).await?;
        if reaped.requeued > 0 {
            self.inner.notifier.notify();
        }
        if reaped.lost > 0 || reaped.released > 0 {
            tracing::info!(
                lost = reaped.lost,
                requeued = reaped.requeued,
                released = reaped.released,
                "expired attempt leases"
            );
        }
        db.prune_job_tokens(now).await?;
        let tracked: Vec<i64> = self
            .inner
            .steps
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .copied()
            .collect();
        for id in tracked {
            let running = db
                .attempt(id)
                .await?
                .is_some_and(|a| a.status == fornax_core::ci::AttemptStatus::Running);
            if !running {
                self.forget_steps(id);
            }
        }
        Ok(reaped)
    }

    /// Run [`RunnerService::reap_once`] every `interval` until [`RunnerService::shutdown`].
    pub fn spawn_reaper(&self, interval: Duration) -> JoinHandle<()> {
        let svc = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(interval);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    () = svc.inner.stopping.cancelled() => return,
                    _ = tick.tick() => {}
                }
                if let Err(e) = svc.reap_once(fornax_core::now_unix()).await {
                    tracing::warn!(error = %e, "lease reaper failed");
                }
            }
        })
    }

    pub(crate) fn forget_steps(&self, attempt_id: i64) {
        self.inner
            .steps
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&attempt_id);
    }
}

/// SHA-256 (hex) of a token — the same hash `fornax-api` uses, so stored runner and job
/// token hashes are interchangeable between the crates.
pub fn hash_token(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

fn random_token(prefix: &str) -> Result<String, getrandom::Error> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes)?;
    Ok(format!("{prefix}{}", hex::encode(bytes)))
}

/// A new runner token `fnr_<64 hex>`: `(plain token to show once, hash to store)`.
pub fn generate_runner_token() -> Result<(String, String), getrandom::Error> {
    let token = random_token(RUNNER_TOKEN_PREFIX)?;
    let hash = hash_token(&token);
    Ok((token, hash))
}

/// A new job token `fnj_<64 hex>`: `(plain token, hash to store)`.
pub fn generate_job_token() -> Result<(String, String), getrandom::Error> {
    let token = random_token(JOB_TOKEN_PREFIX)?;
    let hash = hash_token(&token);
    Ok((token, hash))
}

fn valid_ident(s: &str, extra: &[u8]) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= MAX_NAME_LEN
        && b[0].is_ascii_alphanumeric()
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || extra.contains(c))
}

/// Check a runner name: 1-64 characters `[A-Za-z0-9._-]`, starting alphanumeric.
pub fn validate_runner_name(name: &str) -> Result<(), RunnerError> {
    if valid_ident(name, b"._-") {
        Ok(())
    } else {
        Err(RunnerError::InvalidName(name.to_owned()))
    }
}

/// Check and normalise labels: each 1-64 characters `[A-Za-z0-9._:-]`, starting
/// alphanumeric; sorted, duplicates removed. Empty entries (e.g. from `"a,,b"`) are dropped.
pub fn normalize_labels<S: AsRef<str>>(labels: &[S]) -> Result<Vec<String>, RunnerError> {
    let mut out = Vec::with_capacity(labels.len());
    for l in labels {
        let l = l.as_ref().trim();
        if l.is_empty() {
            continue;
        }
        if !valid_ident(l, b"._:-") {
            return Err(RunnerError::InvalidLabel(l.to_owned()));
        }
        out.push(l.to_owned());
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// Register a runner and return it with its plain token (shown once). `trusted_only`
/// should be `true` for host runners (D-008): they then only get jobs of trusted refs.
pub async fn create_runner<S: AsRef<str>>(
    db: &Db,
    name: &str,
    labels: &[S],
    trusted_only: bool,
    now: i64,
) -> Result<(Runner, String), RunnerError> {
    validate_runner_name(name)?;
    let labels = normalize_labels(labels)?;
    let (token, hash) = generate_runner_token()?;
    let runner = db
        .register_runner(name, &hash, &labels, trusted_only, now)
        .await?;
    Ok((runner, token))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_have_expected_shape() {
        let (t, h) = generate_runner_token().expect("random");
        assert!(fornax_proto::is_runner_token_format(&t));
        assert_eq!(h, hash_token(&t));
        let (j, _) = generate_job_token().expect("random");
        assert!(j.starts_with(JOB_TOKEN_PREFIX));
        assert_eq!(j.len(), 4 + 64);
    }

    #[test]
    fn names_and_labels() {
        assert!(validate_runner_name("build-01.host_a").is_ok());
        for bad in ["", "-x", "a b", "a/b", &"x".repeat(65)] {
            assert!(validate_runner_name(bad).is_err(), "{bad:?}");
        }
        assert_eq!(
            normalize_labels(&["linux", " docker ", "", "linux", "arch:x86_64"]).expect("ok"),
            vec!["arch:x86_64", "docker", "linux"]
        );
        assert!(normalize_labels(&["a,b"]).is_err());
        assert!(normalize_labels(&["$(x)"]).is_err());
    }

    #[test]
    fn config_debug_redacts_secret() {
        let mut c = RunnerConfig::new("http://x");
        c.registration_token = Some("supersecret".into());
        let s = format!("{c:?}");
        assert!(!s.contains("supersecret"));
        assert!(s.contains("redacted"));
    }

    #[tokio::test]
    async fn notifier_wakes_enabled_waiter() {
        let n = QueueNotifier::new();
        let notify = n.as_notify();
        let fut = notify.notified();
        tokio::pin!(fut);
        fut.as_mut().enable();
        n.notify();
        tokio::time::timeout(Duration::from_secs(1), fut)
            .await
            .expect("woken");
    }
}
