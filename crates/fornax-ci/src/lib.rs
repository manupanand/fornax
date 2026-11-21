// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! CI triggers: from a push to pipelines in the database (C-003).
//!
//! The Git transports (smart HTTP, SSH) take a ref snapshot
//! ([`fornax_git::RepoStore::snapshot_refs`]) before and after `receive-pack`, turn the two
//! into [`RefUpdate`]s with [`diff_refs`] and call [`on_push`]. For every created or
//! updated branch or tag, `on_push` reads `.fornax/` at the new commit, loads the spec,
//! picks the pipelines whose `on.push` filter matches, evaluates job `if:` conditions and
//! creates one database pipeline per triggered spec pipeline. Each job's resolved
//! definition travels to the runner as a [`JobPayload`] (JSON, versioned).
//!
//! Spec problems never fail the push: they are returned in [`PushOutcome::errors`] for the
//! caller to surface.

#![deny(missing_docs)]

mod hook;
mod payload;
mod push;
mod refs;

pub use hook::{Wakeup, after_push, before_push};
pub use payload::{JobPayload, PAYLOAD_VERSION, StepPayload};
pub use push::{
    MAX_JOB_PAYLOAD_BYTES, MAX_JOBS_PER_PUSH, MAX_PIPELINES_PER_REF, MAX_REF_UPDATES,
    MAX_STEPS_PER_JOB, PushOutcome, on_push,
};
pub use refs::{RefUpdate, diff_refs};

/// Errors from this crate. Spec problems are not errors (see [`PushOutcome::errors`]).
#[derive(Debug, thiserror::Error)]
pub enum CiError {
    /// The database failed.
    #[error(transparent)]
    Db(#[from] fornax_db::DbError),
    /// Reading the repository failed.
    #[error(transparent)]
    Git(#[from] fornax_git::GitError),
    /// A job payload could not be encoded or decoded.
    #[error("job payload: {0}")]
    Payload(#[from] serde_json::Error),
    /// A job payload has a version this build does not understand.
    #[error("unsupported job payload version {0}")]
    PayloadVersion(u32),
    /// Loading or evaluating a spec on the blocking thread pool failed (panicked or was
    /// cancelled).
    #[error("spec evaluation task failed: {0}")]
    Task(#[from] tokio::task::JoinError),
}
