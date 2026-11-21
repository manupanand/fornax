// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! Fornax runner agent (task C-006).
//!
//! The agent connects to a Fornax server over the runner protocol ([`fornax_proto`]),
//! long-polls for jobs, runs them with an [`Executor`], streams masked logs, heartbeats the
//! attempt lease and reports the result. The orchestrating binary exposes it as
//! `fornax agent`.
//!
//! # Security warning — host executor
//!
//! The only executor so far, [`HostExecutor`], runs job scripts **directly on the runner host
//! with no isolation** (LOG D-008). Any job can read or change everything the agent's OS user
//! can, including other jobs and the agent itself. Use it only for **trusted** repositories and
//! refs; the server must never send fork / external pull-request jobs to host runners. Run the
//! agent as a dedicated unprivileged user. See [`host`] for the mitigations it does apply.
//!
//! **Runner tokens are instance-wide.** A runner token (and the server's shared registration
//! token that mints them) lets its holder claim *any* trusted job of *any* repository on the
//! server (subject to labels) and receive that job's secrets. Runners are not scoped to an
//! owner or repository yet, so a host runner must only serve **trusted, single-tenant**
//! workloads. At start the agent makes itself non-dumpable ([`host::harden_process`]) so job
//! processes running as the same user cannot read the runner token from
//! `/proc/<agent>/environ` or `/proc/<agent>/mem`. `/proc/<pid>/cmdline` stays world-readable
//! whatever the agent does: pass the token through the environment (`FORNAX_RUNNER_TOKEN`), not
//! as a command-line argument.
//!
//! # Protocol flow
//!
//! ```text
//! loop: Next(capacity_free) ──job──▶ spawn:
//!         Extend every heartbeat_interval  (CANCEL / LOST → cancel the job)
//!         Update per step state            (RUNNING / SUCCESS / FAILURE / SKIPPED)
//!         Log    chunks ≤ 64 KiB / 500 ms, byte offsets, retried until acked
//!         Done   after all logs are acked; retried until accepted
//! ```
//!
//! Secrets (`Job.mask`, `Job.secrets` values, the job token) are masked with the streaming
//! [`Masker`] before any byte leaves the agent.

pub mod agent;
pub mod executor;
pub mod host;
pub mod masker;
pub mod sink;

use std::path::PathBuf;
use std::time::Duration;

pub use agent::{job_mask_phrases, register, run, run_with_executor};
pub use executor::{Executor, JobOutcome};
pub use host::HostExecutor;
pub use masker::{MASK, Masker};
pub use sink::{LogEvents, LogSink, SinkEvent, StepUpdate};

/// Errors returned by the agent.
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    /// The configuration is unusable.
    #[error("invalid agent configuration: {0}")]
    InvalidConfig(String),
    /// Connecting to the server failed.
    #[error("transport error: {0}")]
    Transport(#[from] tonic::transport::Error),
    /// The server rejected a call.
    #[error("server error: {0}")]
    Rpc(#[from] tonic::Status),
}

/// Agent configuration.
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// Server URL, e.g. `https://fornax.example.com` (TLS with WebPKI roots) or `http://…`.
    /// Plain `http://` is meant for loopback; to any other host the agent logs a warning,
    /// since the runner token, job tokens, secrets and logs would travel unencrypted.
    pub server_url: String,
    /// Runner token (`fnr_<64 hex>`) obtained from [`register`].
    pub runner_token: String,
    /// Runner labels (sent at registration; informational here).
    pub labels: Vec<String>,
    /// Directory under which per-job workspaces are created. Must be used by this agent
    /// only: [`run`] removes leftover workspaces (all-digit entries) from it at start.
    pub work_dir: PathBuf,
    /// Maximum number of concurrent jobs (≥ 1).
    pub capacity: usize,
    /// Deadline of one `Next` long-poll (≈ 35 s; the server answers before it).
    pub poll_timeout: Duration,
    /// Interval of `Extend` heartbeats (the server's lease is 3 × this).
    pub heartbeat_interval: Duration,
    /// How long shutdown waits for running jobs to report before giving up.
    pub shutdown_timeout: Duration,
}

impl AgentConfig {
    /// A configuration with defaults: capacity 1, 35 s poll, 10 s heartbeat, 30 s shutdown.
    pub fn new(
        server_url: impl Into<String>,
        runner_token: impl Into<String>,
        work_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            server_url: server_url.into(),
            runner_token: runner_token.into(),
            labels: Vec::new(),
            work_dir: work_dir.into(),
            capacity: 1,
            poll_timeout: Duration::from_secs(35),
            heartbeat_interval: Duration::from_secs(10),
            shutdown_timeout: Duration::from_secs(30),
        }
    }

    /// Checks the configuration.
    pub fn validate(&self) -> Result<(), AgentError> {
        let bad = |m: &str| Err(AgentError::InvalidConfig(m.to_owned()));
        if !(self.server_url.starts_with("http://") || self.server_url.starts_with("https://")) {
            return bad("server_url must start with http:// or https://");
        }
        if !fornax_proto::is_runner_token_format(&self.runner_token) {
            return bad("runner_token must look like fnr_<64 hex>");
        }
        if self.capacity == 0 {
            return bad("capacity must be at least 1");
        }
        if self.poll_timeout.is_zero() || self.heartbeat_interval.is_zero() {
            return bad("poll_timeout and heartbeat_interval must be non-zero");
        }
        Ok(())
    }

    /// `true` when `server_url` is plain `http://` to a host that is not loopback
    /// (`localhost`, `*.localhost`, `127.0.0.0/8`, `::1`): credentials would cross the
    /// network unencrypted. The agent logs a warning at start in that case.
    pub fn is_insecure_remote(&self) -> bool {
        let Some(rest) = self.server_url.strip_prefix("http://") else {
            return false;
        };
        let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
        let authority = authority.rsplit('@').next().unwrap_or_default();
        let host = match authority.strip_prefix('[') {
            Some(v6) => v6.split(']').next().unwrap_or_default(),
            None => authority.split(':').next().unwrap_or_default(),
        };
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        let loopback = host == "localhost"
            || host.ends_with(".localhost")
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback());
        !loopback
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_validation() {
        let token = format!("fnr_{}", "0".repeat(64));
        let ok = AgentConfig::new("http://127.0.0.1:1", &token, "/tmp/w");
        assert!(ok.validate().is_ok());
        assert!(
            AgentConfig::new("ftp://x", &token, "/tmp")
                .validate()
                .is_err()
        );
        assert!(
            AgentConfig::new("http://x", "bad", "/tmp")
                .validate()
                .is_err()
        );
        let mut zero = ok.clone();
        zero.capacity = 0;
        assert!(zero.validate().is_err());
    }

    #[test]
    fn insecure_remote_detection() {
        let token = format!("fnr_{}", "0".repeat(64));
        let insecure = |url: &str| AgentConfig::new(url, &token, "/tmp").is_insecure_remote();
        for local in [
            "http://127.0.0.1:3000",
            "http://127.1.2.3",
            "http://localhost:3000/",
            "http://LOCALHOST",
            "http://ci.localhost:80",
            "http://[::1]:3000",
            "https://ci.example.com",
            "https://10.0.0.5:443",
        ] {
            assert!(!insecure(local), "{local}");
        }
        for remote in [
            "http://ci.example.com",
            "http://10.0.0.5:3000",
            "http://[2001:db8::1]:3000",
            "http://localhost.evil.com",
            "http://127.0.0.1@evil.com",
            "http://0.0.0.0:3000",
        ] {
            assert!(insecure(remote), "{remote}");
        }
    }
}
