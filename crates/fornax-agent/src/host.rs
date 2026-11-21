// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! The host (shell) executor — **no isolation** (LOG D-008, D-018).
//!
//! # Security warning
//!
//! Jobs run as the agent's own OS user, directly on the runner host. A job can read and modify
//! everything that user can: other jobs' workspaces while they run, the agent's files and
//! configuration, caches, SSH keys, cloud credentials in `$HOME`, and it can leave processes
//! behind that escape its process group. **Only run trusted code on a host runner**: the server
//! must never route fork / external pull-request jobs to host runners, and the agent should run
//! as a dedicated unprivileged user on a machine that holds nothing else of value.
//!
//! Mitigations implemented here (they reduce accidents, they are not a sandbox):
//! - a fresh workspace `work_dir/<attempt_id>/`, removed after the job (also on failure,
//!   timeout and cancel);
//! - steps get a cleared environment (only `PATH`, `HOME`, `USER`, `LOGNAME`, `LANG`, `TZ`,
//!   `TMPDIR` are inherited from the agent), so the runner token never reaches a job;
//! - the job token is used only for checkout, passed to `git` through `GIT_CONFIG_*`
//!   environment variables of that one process (not argv, not the URL, not step env);
//! - secrets are given only to user steps, never to checkout;
//! - every process runs in its own process group; on timeout, cancel, or when a step ends,
//!   the whole group gets `SIGTERM`, then `SIGKILL` after a grace period. Processes started
//!   with `setsid` (daemons) escape this.
//! - the agent process is made non-dumpable at start ([`harden_process`], Linux
//!   `PR_SET_DUMPABLE = 0`): a step running as the same user can no longer read
//!   `/proc/<agent>/environ`, `mem`, `maps`, … (they require ptrace-read access), which
//!   would otherwise reveal the runner token (`FORNAX_RUNNER_TOKEN`) and, in memory, every
//!   job's token and secrets. `/proc/<agent>/cmdline` is world-readable regardless, so the
//!   runner token must not be passed as an argument;
//! - the job token is revoked by the server on the first step update, i.e. once checkout is
//!   over, so a step cannot reuse it later;
//! - leftover workspaces of a previous agent run (e.g. after `SIGKILL`) are removed at start
//!   ([`HostExecutor::sweep_workspaces`]).
//!
//! Known limits (no safe API exists for them, and `unsafe`/`pre_exec` is forbidden):
//! - the `git fetch` child is an exec'd program and therefore dumpable again (execve resets
//!   the flag; `PR_SET_DUMPABLE` in `pre_exec` would not survive exec either). Its
//!   environment carries the job token while it runs. The job's own steps start only after
//!   it has exited, but with `capacity > 1` a *concurrent* job of the same agent could read it
//!   during that window. Use capacity 1, or separate OS users per agent, when jobs must not
//!   see each other's checkout credentials;
//! - `PR_SET_PDEATHSIG` (kill job processes when the agent dies) must be set in the child
//!   between fork and exec, i.e. `pre_exec`, which is `unsafe`; it is not used. Processes
//!   of an agent killed with `SIGKILL` keep running; their workspaces are swept on the next
//!   start.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use fornax_proto::runner::v1::{Job, JobResult, LogStream, Step, StepState};
use nix::errno::Errno;
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};
use tokio::time::{Instant, sleep, sleep_until};
use tokio_util::sync::CancellationToken;

use crate::executor::{Executor, JobOutcome};
use crate::sink::{LogSink, StepUpdate};

/// Default whole-job timeout when `Job.timeout_secs` is 0.
pub const DEFAULT_JOB_TIMEOUT: Duration = Duration::from_secs(60 * 60);
/// Default time between `SIGTERM` and `SIGKILL`.
pub const DEFAULT_KILL_GRACE: Duration = Duration::from_secs(10);

/// Environment variables inherited from the agent by every process the executor starts.
const INHERITED_ENV: &[&str] = &["PATH", "HOME", "USER", "LOGNAME", "LANG", "TZ", "TMPDIR"];
const FALLBACK_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
/// How long to wait for output pipes to drain after the process group is gone.
const PIPE_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// Runs jobs as plain processes on the runner host. See the module docs for the risks.
#[derive(Debug, Clone)]
pub struct HostExecutor {
    work_dir: PathBuf,
    default_job_timeout: Duration,
    kill_grace: Duration,
    /// `Some(bash)` when bash was found on `PATH`, otherwise `sh -ec` is used.
    bash: Option<PathBuf>,
}

impl HostExecutor {
    /// Creates an executor that puts workspaces under `work_dir`.
    pub fn new(work_dir: impl Into<PathBuf>) -> Self {
        Self {
            work_dir: work_dir.into(),
            default_job_timeout: DEFAULT_JOB_TIMEOUT,
            kill_grace: DEFAULT_KILL_GRACE,
            bash: find_on_path("bash"),
        }
    }

    /// Sets the job timeout used when `Job.timeout_secs` is 0.
    pub fn with_default_job_timeout(mut self, timeout: Duration) -> Self {
        self.default_job_timeout = timeout;
        self
    }

    /// Sets the grace period between `SIGTERM` and `SIGKILL`.
    pub fn with_kill_grace(mut self, grace: Duration) -> Self {
        self.kill_grace = grace;
        self
    }

    /// The workspace directory a job with `attempt_id` would use.
    pub fn workspace_for(&self, attempt_id: &str) -> PathBuf {
        self.work_dir.join(attempt_id)
    }

    /// Removes leftover attempt workspaces from `work_dir`: directories (not symlinks) whose
    /// name is an all-digit attempt id, as the server issues them. Anything else is left
    /// alone. Call only while no job runs (agent start); `work_dir` must belong to this agent.
    /// Returns how many were removed; a missing `work_dir` counts as empty.
    pub async fn sweep_workspaces(&self) -> std::io::Result<usize> {
        let mut entries = match tokio::fs::read_dir(&self.work_dir).await {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(e),
        };
        let mut removed = 0;
        while let Some(entry) = entries.next_entry().await? {
            let name = entry.file_name();
            let looks_like_attempt = name.to_str().is_some_and(|n| {
                !n.is_empty() && n.len() <= 20 && n.bytes().all(|b| b.is_ascii_digit())
            });
            if !looks_like_attempt || !entry.file_type().await?.is_dir() {
                continue;
            }
            tokio::fs::remove_dir_all(entry.path()).await?;
            removed += 1;
        }
        Ok(removed)
    }
}

/// Hardens the agent process against the jobs it runs (Linux): `PR_SET_DUMPABLE = 0`, so
/// processes of the same user can no longer read `/proc/<agent>/environ`, `mem` and similar
/// files, nor ptrace the agent. No-op on other systems. Called by
/// [`crate::run_with_executor`] at start.
pub fn harden_process() -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    nix::sys::prctl::set_dumpable(false).map_err(std::io::Error::from)?;
    Ok(())
}

impl Executor for HostExecutor {
    async fn run(&self, job: &Job, logs: LogSink, cancel: CancellationToken) -> JobOutcome {
        let timeout = if job.timeout_secs > 0 {
            Duration::from_secs(job.timeout_secs)
        } else {
            self.default_job_timeout
        };
        let deadline = Instant::now() + timeout;

        if !is_safe_component(&job.attempt_id) {
            skip_from(&logs, job, 0).await;
            return JobOutcome::new(JobResult::Error, 0, "invalid attempt id");
        }
        let ws = self.workspace_for(&job.attempt_id);
        let ws = match prepare_workspace(&ws).await {
            Ok(ws) => ws,
            Err(e) => {
                logs.system(&format!("error: cannot create workspace: {e}"))
                    .await;
                skip_from(&logs, job, 0).await;
                return JobOutcome::new(JobResult::Error, 0, format!("workspace: {e}"));
            }
        };

        let outcome = self.run_in(job, &ws, &logs, &cancel, deadline).await;

        if let Err(e) = tokio::fs::remove_dir_all(&ws).await {
            logs.system(&format!("warning: cannot remove workspace: {e}"))
                .await;
        }
        logs.flush().await;
        outcome
    }
}

/// How a process ended.
#[derive(Debug)]
enum ProcEnd {
    Exited(i32),
    TimedOut,
    Canceled,
    SpawnFailed(std::io::Error),
}

impl HostExecutor {
    async fn run_in(
        &self,
        job: &Job,
        ws: &Path,
        logs: &LogSink,
        cancel: &CancellationToken,
        deadline: Instant,
    ) -> JobOutcome {
        if !job.clone_url.is_empty()
            && let Err(outcome) = self.checkout(job, ws, logs, cancel, deadline).await
        {
            skip_from(logs, job, 0).await;
            return outcome;
        }

        let ws_str = ws.to_string_lossy().into_owned();
        let mut last_exit = 0;
        for (i, step) in job.steps.iter().enumerate() {
            let index = u32::try_from(i).unwrap_or(u32::MAX);
            let started_at = now_ms();
            logs.step(StepUpdate {
                index,
                state: StepState::Running,
                started_at,
                finished_at: 0,
                exit_code: 0,
            })
            .await;
            logs.system(&format!("$ {}", step.name)).await;

            let finish = |state: StepState, exit_code: i32| StepUpdate {
                index,
                state,
                started_at,
                finished_at: now_ms(),
                exit_code,
            };

            let cwd = match resolve_workdir(ws, &step.working_directory).await {
                Ok(cwd) => cwd,
                Err(msg) => {
                    logs.system(&format!("error: {msg}")).await;
                    logs.step(finish(StepState::Failure, -1)).await;
                    skip_from(logs, job, i + 1).await;
                    return JobOutcome::new(
                        JobResult::Failure,
                        -1,
                        format!("step {:?}: {msg}", step.name),
                    );
                }
            };

            let step_deadline = if step.timeout_secs > 0 {
                (Instant::now() + Duration::from_secs(step.timeout_secs)).min(deadline)
            } else {
                deadline
            };
            let cmd = self.step_command(job, step, &cwd, &ws_str);
            let end = self.run_process(cmd, logs, cancel, step_deadline).await;
            logs.flush().await;

            let failure = match end {
                ProcEnd::Exited(0) => {
                    logs.step(finish(StepState::Success, 0)).await;
                    last_exit = 0;
                    continue;
                }
                ProcEnd::Exited(code) => {
                    logs.system(&format!(
                        "step {:?} failed with exit code {code}",
                        step.name
                    ))
                    .await;
                    (
                        JobResult::Failure,
                        code,
                        format!("step {:?} exited with {code}", step.name),
                    )
                }
                ProcEnd::TimedOut => {
                    let what = if step_deadline < deadline {
                        "step"
                    } else {
                        "job"
                    };
                    logs.system(&format!("error: {what} timed out; processes killed"))
                        .await;
                    (JobResult::Failure, -1, format!("{what} timed out"))
                }
                ProcEnd::Canceled => {
                    logs.system("job canceled; processes killed").await;
                    (JobResult::Canceled, -1, String::new())
                }
                ProcEnd::SpawnFailed(e) => {
                    logs.system(&format!("error: cannot start shell: {e}"))
                        .await;
                    (JobResult::Error, -1, format!("cannot start shell: {e}"))
                }
            };
            logs.step(finish(StepState::Failure, failure.1)).await;
            skip_from(logs, job, i + 1).await;
            return JobOutcome::new(failure.0, failure.1, failure.2);
        }
        JobOutcome::new(JobResult::Success, last_exit, String::new())
    }

    /// `git init` + shallow fetch of `sha` + detached checkout. Returns the outcome to report
    /// when it fails.
    async fn checkout(
        &self,
        job: &Job,
        ws: &Path,
        logs: &LogSink,
        cancel: &CancellationToken,
        deadline: Instant,
    ) -> Result<(), JobOutcome> {
        let err = |msg: String| JobOutcome::new(JobResult::Error, -1, msg);
        if !is_allowed_clone_url(&job.clone_url) {
            logs.system("error: unsupported clone URL scheme").await;
            return Err(err("checkout: unsupported clone URL scheme".into()));
        }
        if !is_commit_id(&job.sha) {
            logs.system("error: invalid commit id").await;
            return Err(err("checkout: invalid commit id".into()));
        }
        logs.system(&format!("$ checkout {} @ {}", job.repo_full_name, job.sha))
            .await;

        let header = (!job.job_token.is_empty()).then(|| basic_auth_header(&job.job_token));
        let steps: [(&[&str], bool); 3] = [
            (&["init", "-q"], false),
            (&["fetch", "--depth", "1", "--no-tags"], true),
            (&["checkout", "-q", "--detach", "FETCH_HEAD"], false),
        ];
        for (args, is_fetch) in steps {
            let mut cmd = Command::new("git");
            cmd.args(args);
            if is_fetch {
                cmd.arg(&job.clone_url).arg(&job.sha);
            }
            cmd.current_dir(ws).env_clear().envs(base_env());
            cmd.env("GIT_TERMINAL_PROMPT", "0");
            if is_fetch && let Some(h) = &header {
                // Same effect as `-c http.extraHeader=…` but invisible in argv (`ps`).
                cmd.env("GIT_CONFIG_COUNT", "1")
                    .env("GIT_CONFIG_KEY_0", "http.extraHeader")
                    .env("GIT_CONFIG_VALUE_0", h);
            }
            match self.run_process(cmd, logs, cancel, deadline).await {
                ProcEnd::Exited(0) => {}
                ProcEnd::Exited(code) => {
                    logs.system(&format!("error: git {} failed ({code})", args[0]))
                        .await;
                    return Err(err(format!("checkout: git {} exited with {code}", args[0])));
                }
                ProcEnd::TimedOut => {
                    logs.system("error: job timed out during checkout").await;
                    return Err(JobOutcome::new(JobResult::Failure, -1, "job timed out"));
                }
                ProcEnd::Canceled => {
                    logs.system("job canceled").await;
                    return Err(JobOutcome::new(JobResult::Canceled, -1, ""));
                }
                ProcEnd::SpawnFailed(e) => {
                    logs.system(&format!("error: cannot run git: {e}")).await;
                    return Err(err(format!("checkout: cannot run git: {e}")));
                }
            }
        }
        logs.flush().await;
        Ok(())
    }

    fn step_command(&self, job: &Job, step: &Step, cwd: &Path, ws: &str) -> Command {
        let mut cmd = match &self.bash {
            Some(bash) => {
                let mut c = Command::new(bash);
                c.args(["--noprofile", "--norc", "-eo", "pipefail", "-c"]);
                c
            }
            None => {
                let mut c = Command::new("sh");
                c.arg("-ec");
                c
            }
        };
        cmd.arg(&step.run);
        cmd.current_dir(cwd).env_clear().envs(base_env());
        cmd.envs(step_env(job, step, ws));
        cmd
    }

    /// Spawns `cmd` in its own process group, streams its output into `logs`, and enforces
    /// `deadline` and `cancel`. The whole group is killed when the process ends for any reason.
    async fn run_process(
        &self,
        mut cmd: Command,
        logs: &LogSink,
        cancel: &CancellationToken,
        deadline: Instant,
    ) -> ProcEnd {
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => return ProcEnd::SpawnFailed(e),
        };
        let pgid = child
            .id()
            .and_then(|id| i32::try_from(id).ok())
            .map(Pid::from_raw);
        let mut pumps = Vec::new();
        if let Some(out) = child.stdout.take() {
            pumps.push(tokio::spawn(pump(out, logs.clone(), LogStream::Stdout)));
        }
        if let Some(err) = child.stderr.take() {
            pumps.push(tokio::spawn(pump(err, logs.clone(), LogStream::Stderr)));
        }

        let end = tokio::select! {
            status = child.wait() => match status {
                Ok(s) => ProcEnd::Exited(exit_code(s)),
                Err(e) => ProcEnd::SpawnFailed(e),
            },
            () = sleep_until(deadline) => ProcEnd::TimedOut,
            () = cancel.cancelled() => ProcEnd::Canceled,
        };

        // Kill the whole group: on timeout/cancel, and also leftovers (background processes)
        // after a normal exit, which would otherwise keep the output pipes open.
        if let Some(pgid) = pgid {
            kill_group(pgid, &mut child, self.kill_grace).await;
        }
        let _ = child.wait().await;

        for mut p in pumps {
            if tokio::time::timeout(PIPE_DRAIN_TIMEOUT, &mut p)
                .await
                .is_err()
            {
                // Something outside the group (e.g. a `setsid` daemon) holds the pipe.
                p.abort();
            }
        }
        end
    }
}

/// `SIGTERM` the process group, wait up to `grace` for it to disappear (reaping our direct
/// child meanwhile so it does not linger as a zombie group member), then `SIGKILL`.
async fn kill_group(pgid: Pid, child: &mut Child, grace: Duration) {
    if killpg(pgid, Signal::SIGTERM) == Err(Errno::ESRCH) {
        return;
    }
    let until = Instant::now() + grace;
    loop {
        let _ = child.try_wait();
        if killpg(pgid, None) == Err(Errno::ESRCH) {
            return;
        }
        if Instant::now() >= until {
            break;
        }
        sleep(Duration::from_millis(25)).await;
    }
    let _ = killpg(pgid, Signal::SIGKILL);
}

async fn pump<R: AsyncRead + Unpin>(mut reader: R, logs: LogSink, stream: LogStream) {
    let mut buf = vec![0u8; 8192];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => logs.write(stream, &buf[..n]).await,
        }
    }
}

fn exit_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt as _;
    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}

async fn skip_from(logs: &LogSink, job: &Job, from: usize) {
    for i in from..job.steps.len() {
        logs.step(StepUpdate {
            index: u32::try_from(i).unwrap_or(u32::MAX),
            state: StepState::Skipped,
            started_at: 0,
            finished_at: 0,
            exit_code: 0,
        })
        .await;
    }
}

/// Creates a fresh, empty workspace and returns its canonical path.
async fn prepare_workspace(ws: &Path) -> std::io::Result<PathBuf> {
    if let Some(parent) = ws.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    match tokio::fs::remove_dir_all(ws).await {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    tokio::fs::create_dir(ws).await?;
    tokio::fs::canonicalize(ws).await
}

/// Resolves a step's `working_directory` inside the (canonical) workspace `ws`.
///
/// Rejects absolute paths, `..` components and symlinks that lead outside the workspace.
async fn resolve_workdir(ws: &Path, dir: &str) -> Result<PathBuf, String> {
    if dir.is_empty() {
        return Ok(ws.to_path_buf());
    }
    let rel = Path::new(dir);
    if !rel
        .components()
        .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
    {
        return Err(format!(
            "working_directory {dir:?} must be a relative path inside the workspace"
        ));
    }
    let full = tokio::fs::canonicalize(ws.join(rel))
        .await
        .map_err(|e| format!("working_directory {dir:?}: {e}"))?;
    if !full.starts_with(ws) {
        return Err(format!("working_directory {dir:?} escapes the workspace"));
    }
    Ok(full)
}

/// Environment of a user step, in increasing precedence: job env, step env, secrets, then the
/// variables Fornax defines (`CI`, `FORNAX_*`), which cannot be overridden.
fn step_env(job: &Job, step: &Step, ws: &str) -> HashMap<String, String> {
    let mut env: HashMap<String, String> = HashMap::new();
    env.extend(job.env.iter().map(|(k, v)| (k.clone(), v.clone())));
    env.extend(step.env.iter().map(|(k, v)| (k.clone(), v.clone())));
    env.extend(job.secrets.iter().map(|(k, v)| (k.clone(), v.clone())));
    env.insert("CI".into(), "true".into());
    env.insert("FORNAX_JOB".into(), job.name.clone());
    env.insert("FORNAX_SHA".into(), job.sha.clone());
    env.insert("FORNAX_REF".into(), job.r#ref.clone());
    env.insert("FORNAX_REPO".into(), job.repo_full_name.clone());
    env.insert("FORNAX_WORKSPACE".into(), ws.to_owned());
    env
}

/// The inherited part of every process environment.
fn base_env() -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = INHERITED_ENV
        .iter()
        .filter_map(|k| std::env::var(k).ok().map(|v| ((*k).to_owned(), v)))
        .collect();
    if !env.iter().any(|(k, _)| k == "PATH") {
        env.push(("PATH".into(), FALLBACK_PATH.into()));
    }
    env
}

/// The `http.extraHeader` value carrying the job token (`fornax-job:<token>`, HTTP Basic).
pub fn basic_auth_header(job_token: &str) -> String {
    format!("Authorization: Basic {}", basic_auth_value(job_token))
}

/// Base64 of `fornax-job:<token>` (also added to the mask list by the agent).
pub fn basic_auth_value(job_token: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(format!("fornax-job:{job_token}"))
}

fn is_allowed_clone_url(url: &str) -> bool {
    ["https://", "http://", "file://"]
        .iter()
        .any(|p| url.starts_with(p))
}

fn is_commit_id(sha: &str) -> bool {
    matches!(sha.len(), 40 | 64) && sha.bytes().all(|b| b.is_ascii_hexdigit())
}

fn is_safe_component(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && s.len() <= 128
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

fn find_on_path(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_else(|| FALLBACK_PATH.into());
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|p| p.is_file())
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validators() {
        assert!(is_commit_id(&"a".repeat(40)));
        assert!(!is_commit_id("HEAD"));
        assert!(!is_commit_id(&format!("-{}", "a".repeat(39))));
        assert!(is_allowed_clone_url("https://h/r.git"));
        assert!(!is_allowed_clone_url("--upload-pack=x"));
        assert!(!is_allowed_clone_url("ext::sh -c x"));
        assert!(is_safe_component("att-1_2"));
        assert!(!is_safe_component(".."));
        assert!(!is_safe_component("a/b"));
        assert!(!is_safe_component(""));
    }

    #[test]
    fn basic_auth() {
        assert_eq!(basic_auth_value("t"), "Zm9ybmF4LWpvYjp0");
        assert_eq!(
            basic_auth_header("t"),
            "Authorization: Basic Zm9ybmF4LWpvYjp0"
        );
    }

    #[tokio::test]
    async fn workdir_resolution() {
        let tmp = tempfile::tempdir().expect("tmp");
        let ws = tokio::fs::canonicalize(tmp.path()).await.expect("canon");
        tokio::fs::create_dir(ws.join("sub")).await.expect("mkdir");
        assert_eq!(resolve_workdir(&ws, "").await, Ok(ws.clone()));
        assert_eq!(resolve_workdir(&ws, "sub").await, Ok(ws.join("sub")));
        assert_eq!(resolve_workdir(&ws, "./sub").await, Ok(ws.join("sub")));
        assert!(resolve_workdir(&ws, "../x").await.is_err());
        assert!(resolve_workdir(&ws, "sub/../../x").await.is_err());
        assert!(resolve_workdir(&ws, "/etc").await.is_err());
        assert!(resolve_workdir(&ws, "missing").await.is_err());
        std::os::unix::fs::symlink("/", ws.join("root")).expect("symlink");
        assert!(resolve_workdir(&ws, "root").await.is_err());
    }
}
