// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! The executor abstraction: host now, Docker (C-011) and Kubernetes later, without changing
//! the runner protocol (LOG D-008).

use std::future::Future;

use fornax_proto::runner::v1::{Job, JobResult};
use tokio_util::sync::CancellationToken;

use crate::sink::LogSink;

/// Final result of running a job, sent to the server with `Done`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobOutcome {
    /// Overall result.
    pub result: JobResult,
    /// Exit code of the last step that ran (0 if none).
    pub exit_code: i32,
    /// Human-readable error for `Failure` / `Error`; empty otherwise. Masked by the agent.
    pub error: String,
}

impl JobOutcome {
    /// A successful outcome.
    pub fn success() -> Self {
        Self {
            result: JobResult::Success,
            exit_code: 0,
            error: String::new(),
        }
    }

    /// An outcome with `result`, `exit_code` and an error message.
    pub fn new(result: JobResult, exit_code: i32, error: impl Into<String>) -> Self {
        Self {
            result,
            exit_code,
            error: error.into(),
        }
    }
}

/// Runs one job.
///
/// Contract:
/// - emit log output and step updates through `logs` while running (steps that do not run are
///   reported `SKIPPED`);
/// - stop promptly when `cancel` fires and return `JobResult::Canceled`;
/// - clean up everything the job created before returning;
/// - never panic for job-level failures: map them to `Failure` / `Error`.
pub trait Executor: Send + Sync + 'static {
    /// Runs `job` to completion (or cancellation).
    fn run(
        &self,
        job: &Job,
        logs: LogSink,
        cancel: CancellationToken,
    ) -> impl Future<Output = JobOutcome> + Send;
}
