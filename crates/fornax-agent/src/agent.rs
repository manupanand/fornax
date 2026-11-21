// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! The agent loop: poll `Next`, run jobs, heartbeat `Extend`, upload logs, report `Done`.

use std::collections::VecDeque;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fornax_proto::runner::v1::runner_client::RunnerClient;
use fornax_proto::runner::v1::{
    DoneRequest, ExtendRequest, ExtendState, Job, JobResult, LogChunk, LogStream, NextRequest,
    RegisterRequest, RegisterResponse, UpdateRequest,
};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio::time::{Instant, sleep};
use tokio_util::sync::CancellationToken;
use tonic::metadata::{AsciiMetadataValue, MetadataValue};
use tonic::service::Interceptor;
use tonic::service::interceptor::InterceptedService;
use tonic::transport::{Channel, ClientTlsConfig, Endpoint};
use tonic::{Code, Request, Status};

use crate::executor::{Executor, JobOutcome};
use crate::host::{HostExecutor, basic_auth_value, harden_process};
use crate::masker::Masker;
use crate::sink::{LogEvents, LogSink, SinkEvent, StepUpdate};
use crate::{AgentConfig, AgentError};

/// Maximum size of one log chunk.
pub const MAX_CHUNK: usize = 64 * 1024;
/// Maximum age of a partly filled chunk before it is sent.
pub const FLUSH_INTERVAL: Duration = Duration::from_millis(500);
/// Unacknowledged log bytes after which the agent stops reading job output (back-pressure).
const MAX_PENDING_BYTES: usize = 16 * 1024 * 1024;
/// Timeout of unary calls other than `Next`.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// Attempts for a step `Update` before it is dropped (logs and `Done` are never dropped).
const UPDATE_ATTEMPTS: u32 = 5;
/// Minimum time between two `Next` calls when the server answers immediately.
const MIN_POLL_GAP: Duration = Duration::from_millis(500);
/// The server's per-attempt log limit (64 MiB, `fornax_db::LogAppend::MAX_BYTES`). The agent
/// stops [`TRUNCATION_RESERVE`] bytes below it and appends [`TRUNCATION_MARKER`].
pub const MAX_LOG_BYTES: u64 = 64 * 1024 * 1024;
/// Room kept below [`MAX_LOG_BYTES`] for the truncation marker.
const TRUNCATION_RESERVE: u64 = 4096;
/// Appended (system stream) once when a job's log reaches the limit; later output is dropped.
pub const TRUNCATION_MARKER: &[u8] =
    b"\n[fornax] log truncated: 64 MiB per job limit reached, further output discarded\n";

/// Status codes after which retrying a `Log`, `Update` or `Done` cannot succeed: the attempt
/// is unknown, belongs to someone else, is no longer running, the request is malformed, or the
/// log limit is reached.
fn is_permanent(code: Code) -> bool {
    matches!(
        code,
        Code::NotFound
            | Code::PermissionDenied
            | Code::FailedPrecondition
            | Code::InvalidArgument
            | Code::ResourceExhausted
    )
}

type Client = RunnerClient<InterceptedService<Channel, BearerAuth>>;

/// Adds `authorization: Bearer <runner token>` to every call.
#[derive(Clone)]
struct BearerAuth(AsciiMetadataValue);

impl Interceptor for BearerAuth {
    fn call(&mut self, mut req: Request<()>) -> Result<Request<()>, Status> {
        req.metadata_mut()
            .insert(fornax_proto::AUTHORIZATION_HEADER, self.0.clone());
        Ok(req)
    }
}

fn endpoint(server_url: &str) -> Result<Endpoint, AgentError> {
    let mut ep = Endpoint::from_shared(server_url.to_owned())
        .map_err(|e| AgentError::InvalidConfig(format!("server_url: {e}")))?
        .connect_timeout(Duration::from_secs(10))
        .tcp_keepalive(Some(Duration::from_secs(30)))
        .http2_keep_alive_interval(Duration::from_secs(30))
        .keep_alive_while_idle(true);
    if server_url.starts_with("https://") {
        ep = ep
            .tls_config(ClientTlsConfig::new().with_webpki_roots())
            .map_err(|e| AgentError::InvalidConfig(format!("tls: {e}")))?;
    }
    Ok(ep)
}

fn client(config: &AgentConfig) -> Result<Client, AgentError> {
    let value: AsciiMetadataValue =
        MetadataValue::try_from(format!("Bearer {}", config.runner_token))
            .map_err(|_| AgentError::InvalidConfig("runner_token is not valid ASCII".into()))?;
    let channel = endpoint(&config.server_url)?.connect_lazy();
    Ok(RunnerClient::with_interceptor(channel, BearerAuth(value)))
}

/// Registers a new runner (`Register`, the only unauthenticated call) and returns its id and
/// runner token. Store the token securely; it is shown only once.
pub async fn register(
    server_url: &str,
    registration_token: &str,
    name: &str,
    labels: &[String],
) -> Result<RegisterResponse, AgentError> {
    let channel = endpoint(server_url)?.connect().await?;
    let mut client = RunnerClient::new(channel);
    let resp = client
        .register(RegisterRequest {
            registration_token: registration_token.to_owned(),
            name: name.to_owned(),
            labels: labels.to_vec(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            os: std::env::consts::OS.to_owned(),
            arch: std::env::consts::ARCH.to_owned(),
        })
        .await?;
    Ok(resp.into_inner())
}

/// Runs the agent with the [`HostExecutor`] until `shutdown` completes.
///
/// Before polling it removes leftover workspaces of an earlier run from `config.work_dir`
/// ([`HostExecutor::sweep_workspaces`]).
pub async fn run(
    config: AgentConfig,
    shutdown: impl Future<Output = ()>,
) -> Result<(), AgentError> {
    config.validate()?;
    let executor = HostExecutor::new(&config.work_dir);
    match executor.sweep_workspaces().await {
        Ok(0) => {}
        Ok(n) => tracing::info!(removed = n, "removed leftover workspaces"),
        Err(e) => tracing::warn!(error = %e, "cannot sweep the work directory"),
    }
    run_with_executor(config, executor, shutdown).await
}

/// Runs the agent with a custom executor until `shutdown` completes.
///
/// On shutdown the agent stops polling, cancels running jobs (reported as `ERROR`, "agent
/// shutting down", so the server's retry policy applies) and waits up to
/// `config.shutdown_timeout` for their logs and `Done` to be delivered.
///
/// Returns an error only for invalid configuration; transport errors are retried with
/// exponential backoff and jitter.
pub async fn run_with_executor<E: Executor>(
    config: AgentConfig,
    executor: E,
    shutdown: impl Future<Output = ()>,
) -> Result<(), AgentError> {
    config.validate()?;
    if let Err(e) = harden_process() {
        tracing::warn!(error = %e, "cannot make the agent non-dumpable; jobs may read its memory");
    }
    if config.is_insecure_remote() {
        tracing::warn!(
            server = %config.server_url,
            "INSECURE: plain http:// to a non-loopback server; the runner token, job tokens, \
             secrets and logs travel unencrypted. Use https://"
        );
    }
    let client = client(&config)?;
    let executor = Arc::new(executor);
    let stopping = CancellationToken::new();
    let give_up = CancellationToken::new();
    let slots = Arc::new(Semaphore::new(config.capacity));
    let mut jobs = JoinSet::new();
    let mut backoff = Backoff::new();
    tokio::pin!(shutdown);

    loop {
        while jobs.try_join_next().is_some() {}
        let permit = tokio::select! {
            () = &mut shutdown => break,
            p = slots.clone().acquire_owned() => match p {
                Ok(p) => p,
                Err(_) => break,
            },
        };
        let free = u32::try_from(slots.available_permits() + 1).unwrap_or(u32::MAX);
        let started = Instant::now();
        let res = tokio::select! {
            () = &mut shutdown => break,
            r = poll_next(client.clone(), free, config.poll_timeout) => r,
        };
        match res {
            Ok(Some(job)) => {
                backoff.reset();
                tracing::info!(attempt = %job.attempt_id, job = %job.name, "job received");
                let ctx = JobCtx {
                    client: client.clone(),
                    heartbeat: config.heartbeat_interval,
                    stopping: stopping.clone(),
                    give_up: give_up.clone(),
                };
                let executor = executor.clone();
                jobs.spawn(async move {
                    run_job(ctx, executor, job).await;
                    drop(permit);
                });
            }
            Ok(None) => {
                backoff.reset();
                drop(permit);
                let gap = MIN_POLL_GAP.saturating_sub(started.elapsed());
                if !gap.is_zero() {
                    tokio::select! {
                        () = &mut shutdown => break,
                        () = sleep(gap) => {}
                    }
                }
            }
            Err(status) => {
                drop(permit);
                let wait = backoff.next_delay();
                tracing::warn!(error = %status, retry_in = ?wait, "Next failed");
                tokio::select! {
                    () = &mut shutdown => break,
                    () = sleep(wait) => {}
                }
            }
        }
    }

    tracing::info!(running = jobs.len(), "agent shutting down");
    stopping.cancel();
    let drained = tokio::time::timeout(config.shutdown_timeout, async {
        while jobs.join_next().await.is_some() {}
    })
    .await;
    if drained.is_err() {
        give_up.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(2), async {
            while jobs.join_next().await.is_some() {}
        })
        .await;
        jobs.abort_all();
    }
    Ok(())
}

/// One `Next` long-poll. `DeadlineExceeded` counts as "no job".
async fn poll_next(
    mut client: Client,
    capacity_free: u32,
    timeout: Duration,
) -> Result<Option<Job>, Status> {
    let mut req = Request::new(NextRequest { capacity_free });
    req.set_timeout(timeout);
    match tokio::time::timeout(timeout + Duration::from_secs(5), client.next(req)).await {
        Err(_) => Ok(None),
        Ok(Ok(resp)) => Ok(resp.into_inner().job),
        Ok(Err(s)) if s.code() == Code::DeadlineExceeded => Ok(None),
        Ok(Err(s)) => Err(s),
    }
}

#[derive(Clone)]
struct JobCtx {
    client: Client,
    heartbeat: Duration,
    /// Agent is shutting down: cancel jobs.
    stopping: CancellationToken,
    /// Shutdown deadline passed: stop retrying uploads and `Done`.
    give_up: CancellationToken,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopReason {
    Canceled,
    Lost,
    Shutdown,
    /// The server permanently refused the job's logs or step updates.
    Rejected,
}

type Reason = Arc<Mutex<Option<StopReason>>>;

fn set_reason(reason: &Reason, r: StopReason) {
    let mut g = reason
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if g.is_none() {
        *g = Some(r);
    }
}

/// Every phrase masked in a job's logs: `Job.mask`, secret values, the job token and its
/// HTTP Basic encoding.
pub fn job_mask_phrases(job: &Job) -> Vec<String> {
    let mut phrases: Vec<String> = job.mask.clone();
    phrases.extend(job.secrets.values().cloned());
    if !job.job_token.is_empty() {
        phrases.push(job.job_token.clone());
        phrases.push(basic_auth_value(&job.job_token));
    }
    phrases.retain(|p| !p.is_empty());
    phrases
}

async fn run_job<E: Executor>(ctx: JobCtx, executor: Arc<E>, job: Job) {
    let attempt = job.attempt_id.clone();
    let phrases = job_mask_phrases(&job);
    let (sink, events) = LogSink::new(&phrases, 256);
    let rejected = CancellationToken::new();
    let uploader = tokio::spawn(upload_loop(
        ctx.client.clone(),
        attempt.clone(),
        events,
        ctx.give_up.clone(),
        rejected.clone(),
    ));

    let cancel = CancellationToken::new();
    let reason: Reason = Arc::new(Mutex::new(None));
    let job_done = CancellationToken::new();
    let heartbeat = tokio::spawn(heartbeat_loop(
        ctx.client.clone(),
        attempt.clone(),
        ctx.heartbeat,
        cancel.clone(),
        reason.clone(),
        job_done.clone(),
    ));
    let watch_shutdown = {
        let (stopping, cancel, reason, job_done) = (
            ctx.stopping.clone(),
            cancel.clone(),
            reason.clone(),
            job_done.clone(),
        );
        tokio::spawn(async move {
            tokio::select! {
                () = stopping.cancelled() => {
                    set_reason(&reason, StopReason::Shutdown);
                    cancel.cancel();
                }
                () = rejected.cancelled() => {
                    set_reason(&reason, StopReason::Rejected);
                    cancel.cancel();
                }
                () = job_done.cancelled() => {}
            }
        })
    };

    let mut outcome = executor.run(&job, sink.clone(), cancel).await;
    job_done.cancel();
    let _ = heartbeat.await;
    let _ = watch_shutdown.await;

    let stop = *reason
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match stop {
        Some(StopReason::Canceled) => outcome = JobOutcome::new(JobResult::Canceled, -1, ""),
        Some(StopReason::Lost) => {
            outcome = JobOutcome::new(JobResult::Error, -1, "lease lost");
        }
        Some(StopReason::Shutdown) => {
            outcome = JobOutcome::new(JobResult::Error, -1, "agent shutting down");
        }
        Some(StopReason::Rejected) => {
            outcome = JobOutcome::new(JobResult::Error, -1, "server rejected the job's logs");
        }
        None => {}
    }
    sink.system(&format!("job finished: {}", result_name(outcome.result)))
        .await;
    sink.flush().await;
    drop(sink);
    // Logs are complete on the server before Done is sent.
    let _ = uploader.await;

    let error = String::from_utf8_lossy(&Masker::new(&phrases).mask_all(outcome.error.as_bytes()))
        .into_owned();
    let done = DoneRequest {
        attempt_id: attempt.clone(),
        result: outcome.result as i32,
        exit_code: outcome.exit_code,
        error,
    };
    send_done(ctx.client, done, &ctx.give_up).await;
    tracing::info!(attempt = %attempt, result = result_name(outcome.result), "job done");
}

fn result_name(r: JobResult) -> &'static str {
    match r {
        JobResult::Success => "success",
        JobResult::Failure => "failure",
        JobResult::Canceled => "canceled",
        JobResult::Error => "error",
        JobResult::Unspecified => "unspecified",
    }
}

/// Retries `Done` until the server accepts it, it reports the attempt as unknown, or the
/// shutdown deadline passes.
async fn send_done(mut client: Client, done: DoneRequest, give_up: &CancellationToken) {
    let mut backoff = Backoff::new();
    loop {
        let mut req = Request::new(done.clone());
        req.set_timeout(CALL_TIMEOUT);
        match client.done(req).await {
            Ok(_) => return,
            Err(s) if is_permanent(s.code()) => {
                tracing::warn!(attempt = %done.attempt_id, error = %s, "Done rejected");
                return;
            }
            Err(s) => {
                let wait = backoff.next_delay();
                tracing::warn!(attempt = %done.attempt_id, error = %s, retry_in = ?wait, "Done failed");
                tokio::select! {
                    () = give_up.cancelled() => {
                        tracing::error!(attempt = %done.attempt_id, "giving up on Done");
                        return;
                    }
                    () = sleep(wait) => {}
                }
            }
        }
    }
}

async fn heartbeat_loop(
    mut client: Client,
    attempt: String,
    interval: Duration,
    cancel: CancellationToken,
    reason: Reason,
    job_done: CancellationToken,
) {
    loop {
        tokio::select! {
            () = job_done.cancelled() => return,
            () = sleep(interval) => {}
        }
        let mut req = Request::new(ExtendRequest {
            attempt_id: attempt.clone(),
        });
        req.set_timeout(CALL_TIMEOUT.min(interval * 3));
        let res = tokio::select! {
            () = job_done.cancelled() => return,
            r = client.extend(req) => r,
        };
        match res.map(|r| r.into_inner().state()) {
            Ok(ExtendState::Cancel) => {
                tracing::info!(attempt = %attempt, "job canceled by server");
                set_reason(&reason, StopReason::Canceled);
                cancel.cancel();
                return;
            }
            Ok(ExtendState::Lost) => {
                tracing::warn!(attempt = %attempt, "lease lost");
                set_reason(&reason, StopReason::Lost);
                cancel.cancel();
                return;
            }
            Ok(_) => {}
            Err(s) => tracing::warn!(attempt = %attempt, error = %s, "Extend failed"),
        }
    }
}

/// Packs masked events into offset-tracked chunks and uploads them with `Log`; forwards step
/// updates with `Update`. Ends when every sink clone is dropped and all chunks are acked
/// (or `give_up` fires). When the server refuses permanently ([`is_permanent`]) it drops
/// everything pending, fires `rejected` (the job is then stopped) and only drains events.
async fn upload_loop(
    client: Client,
    attempt: String,
    mut events: LogEvents,
    give_up: CancellationToken,
    rejected: CancellationToken,
) {
    let mut up = Uploader::new(client, attempt, MAX_LOG_BYTES - TRUNCATION_RESERVE);
    let mut tick = tokio::time::interval(FLUSH_INTERVAL / 2);
    loop {
        if up.rejected.is_some() {
            rejected.cancel();
        }
        tokio::select! {
            ev = events.recv(), if up.pending_bytes < MAX_PENDING_BYTES => match ev {
                Some(SinkEvent::Log { stream, data }) => up.push(stream, &data),
                Some(SinkEvent::Step(update)) => {
                    up.seal();
                    up.upload().await;
                    up.update(update).await;
                }
                None => break,
            },
            _ = tick.tick() => {
                if up.current.as_ref().is_some_and(|(_, t)| t.elapsed() >= FLUSH_INTERVAL) {
                    up.seal();
                }
                if !up.pending.is_empty() && Instant::now() >= up.retry_at {
                    up.upload().await;
                }
            }
        }
    }
    up.seal();
    while !up.pending.is_empty() && up.rejected.is_none() {
        let wait = up.retry_at.saturating_duration_since(Instant::now());
        tokio::select! {
            () = give_up.cancelled() => {
                tracing::error!(attempt = %up.attempt, bytes = up.pending_bytes, "giving up on log upload");
                return;
            }
            () = sleep(wait) => {}
        }
        up.upload().await;
    }
}

struct Uploader {
    client: Client,
    attempt: String,
    /// Log bytes accepted before the truncation marker is appended.
    limit: u64,
    /// The truncation marker was appended; later output is dropped.
    truncated: bool,
    /// The server refused permanently with this code; nothing more is sent.
    rejected: Option<Code>,
    /// Offset after the last byte appended to a chunk.
    next_offset: u64,
    /// Chunk being filled and when it was started.
    current: Option<(LogChunk, Instant)>,
    /// Sealed, unacknowledged chunks in offset order.
    pending: VecDeque<LogChunk>,
    pending_bytes: usize,
    backoff: Backoff,
    retry_at: Instant,
}

impl Uploader {
    fn new(client: Client, attempt: String, limit: u64) -> Self {
        Self {
            client,
            attempt,
            limit,
            truncated: false,
            rejected: None,
            next_offset: 0,
            current: None,
            pending: VecDeque::new(),
            pending_bytes: 0,
            backoff: Backoff::new(),
            retry_at: Instant::now(),
        }
    }

    /// Appends `data`, up to `limit` bytes per job; the first time the limit is hit, the
    /// fitting prefix is kept and [`TRUNCATION_MARKER`] appended, later data is dropped.
    fn push(&mut self, stream: LogStream, data: &[u8]) {
        if self.truncated || self.rejected.is_some() {
            return;
        }
        let room =
            usize::try_from(self.limit.saturating_sub(self.next_offset)).unwrap_or(usize::MAX);
        if data.len() <= room {
            self.push_raw(stream, data);
            return;
        }
        self.push_raw(stream, &data[..room]);
        self.push_raw(LogStream::System, TRUNCATION_MARKER);
        self.truncated = true;
        tracing::warn!(attempt = %self.attempt, limit = self.limit, "job log truncated");
    }

    /// Stops all uploads after a permanent refusal and drops what is pending.
    fn reject(&mut self, status: &Status) {
        tracing::error!(attempt = %self.attempt, error = %status,
            "server refused the job's logs permanently; stopping the job");
        self.rejected = Some(status.code());
        self.current = None;
        self.pending.clear();
        self.pending_bytes = 0;
    }

    fn push_raw(&mut self, stream: LogStream, mut data: &[u8]) {
        while !data.is_empty() {
            if self
                .current
                .as_ref()
                .is_some_and(|(c, _)| c.stream != stream as i32 || c.data.len() >= MAX_CHUNK)
            {
                self.seal();
            }
            let (chunk, _) = self.current.get_or_insert_with(|| {
                (
                    LogChunk {
                        attempt_id: self.attempt.clone(),
                        offset: self.next_offset,
                        data: Vec::new(),
                        stream: stream as i32,
                    },
                    Instant::now(),
                )
            });
            let take = (MAX_CHUNK - chunk.data.len()).min(data.len());
            chunk.data.extend_from_slice(&data[..take]);
            self.next_offset += take as u64;
            data = &data[take..];
        }
    }

    fn seal(&mut self) {
        if let Some((chunk, _)) = self.current.take()
            && !chunk.data.is_empty()
        {
            self.pending_bytes += chunk.data.len();
            self.pending.push_back(chunk);
        }
    }

    /// Sends all pending chunks in one client stream; drops the acknowledged ones.
    async fn upload(&mut self) {
        if self.pending.is_empty() || self.rejected.is_some() {
            return;
        }
        let chunks: Vec<LogChunk> = self.pending.iter().cloned().collect();
        let mut req = Request::new(tokio_stream::iter(chunks));
        req.set_timeout(CALL_TIMEOUT);
        match self.client.log(req).await {
            Ok(ack) => {
                let committed = ack.into_inner().committed_offset;
                while let Some(front) = self.pending.front() {
                    if front.offset + front.data.len() as u64 <= committed {
                        self.pending_bytes -= front.data.len();
                        self.pending.pop_front();
                    } else {
                        break;
                    }
                }
                if self.pending.is_empty() {
                    self.backoff.reset();
                    self.retry_at = Instant::now();
                } else {
                    self.retry_at = Instant::now() + self.backoff.next_delay();
                }
            }
            Err(s) if is_permanent(s.code()) => self.reject(&s),
            Err(s) => {
                let wait = self.backoff.next_delay();
                tracing::warn!(attempt = %self.attempt, error = %s, retry_in = ?wait, "Log failed");
                self.retry_at = Instant::now() + wait;
            }
        }
    }

    async fn update(&mut self, u: StepUpdate) {
        let msg = UpdateRequest {
            attempt_id: self.attempt.clone(),
            step_index: u.index,
            step_state: u.state as i32,
            started_at: u.started_at,
            finished_at: u.finished_at,
            exit_code: u.exit_code,
        };
        if self.rejected.is_some() {
            return;
        }
        let mut backoff = Backoff::new();
        for attempt in 1..=UPDATE_ATTEMPTS {
            let mut req = Request::new(msg.clone());
            req.set_timeout(CALL_TIMEOUT);
            match self.client.update(req).await {
                Ok(_) => return,
                Err(s) if is_permanent(s.code()) => {
                    self.reject(&s);
                    return;
                }
                Err(s) if attempt == UPDATE_ATTEMPTS => {
                    tracing::warn!(attempt = %self.attempt, error = %s, "dropping step update");
                }
                Err(_) => sleep(backoff.next_delay()).await,
            }
        }
    }
}

/// Exponential backoff (0.5 s … 30 s) with "equal jitter" (delay in `[d/2, d]`).
#[derive(Debug)]
pub(crate) struct Backoff {
    current: Duration,
}

impl Backoff {
    const BASE: Duration = Duration::from_millis(500);
    const MAX: Duration = Duration::from_secs(30);

    pub(crate) fn new() -> Self {
        Self {
            current: Self::BASE,
        }
    }

    pub(crate) fn reset(&mut self) {
        self.current = Self::BASE;
    }

    pub(crate) fn next_delay(&mut self) -> Duration {
        let d = self.current;
        self.current = (self.current * 2).min(Self::MAX);
        let half = d / 2;
        let mut buf = [0u8; 4];
        let frac = match getrandom::getrandom(&mut buf) {
            Ok(()) => f64::from(u32::from_le_bytes(buf)) / f64::from(u32::MAX),
            Err(_) => 0.5,
        };
        half + half.mul_f64(frac)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_with_jitter_and_caps() {
        let mut b = Backoff::new();
        let d1 = b.next_delay();
        assert!(d1 >= Duration::from_millis(250) && d1 <= Duration::from_millis(500));
        for _ in 0..20 {
            let d = b.next_delay();
            assert!(d <= Backoff::MAX);
        }
        assert!(b.next_delay() >= Backoff::MAX / 2);
        b.reset();
        assert!(b.next_delay() <= Duration::from_millis(500));
    }

    fn test_client() -> Client {
        let token = format!("fnr_{}", "0".repeat(64));
        client(&AgentConfig::new("http://127.0.0.1:1", token, "/tmp")).expect("client")
    }

    fn uploaded(up: &mut Uploader) -> Vec<u8> {
        up.seal();
        up.pending.iter().flat_map(|c| c.data.clone()).collect()
    }

    /// S5: the agent stops at its limit and appends one truncation marker.
    #[tokio::test]
    async fn log_is_truncated_once_at_the_limit() {
        let mut up = Uploader::new(test_client(), "1".into(), 10);
        up.push(LogStream::Stdout, b"12345");
        up.push(LogStream::Stdout, b"6789abcdef");
        up.push(LogStream::Stdout, b"dropped");
        up.push(LogStream::Stderr, b"dropped too");
        let mut want = b"123456789a".to_vec();
        want.extend_from_slice(TRUNCATION_MARKER);
        assert_eq!(uploaded(&mut up), want);
        assert!(up.truncated);
        assert_eq!(up.next_offset, want.len() as u64);
        assert!(up.next_offset <= 10 + TRUNCATION_RESERVE);
    }

    #[tokio::test]
    async fn log_below_limit_is_untouched() {
        let mut up = Uploader::new(test_client(), "1".into(), 10);
        up.push(LogStream::Stdout, b"0123456789");
        assert_eq!(uploaded(&mut up), b"0123456789");
        assert!(!up.truncated);
    }

    #[test]
    fn limits_are_consistent() {
        assert!(TRUNCATION_MARKER.len() as u64 <= TRUNCATION_RESERVE);
        for code in [
            Code::NotFound,
            Code::PermissionDenied,
            Code::FailedPrecondition,
            Code::InvalidArgument,
            Code::ResourceExhausted,
        ] {
            assert!(is_permanent(code), "{code:?}");
        }
        for code in [Code::Unavailable, Code::Internal, Code::DeadlineExceeded] {
            assert!(!is_permanent(code), "{code:?}");
        }
    }

    #[test]
    fn mask_phrases_include_token_and_secrets() {
        let mut job = Job {
            job_token: "tok".into(),
            mask: vec!["m".into(), String::new()],
            ..Default::default()
        };
        job.secrets.insert("K".into(), "v".into());
        let p = job_mask_phrases(&job);
        assert!(p.contains(&"m".to_string()));
        assert!(p.contains(&"v".to_string()));
        assert!(p.contains(&"tok".to_string()));
        assert!(p.contains(&basic_auth_value("tok")));
        assert!(!p.contains(&String::new()));
    }
}
