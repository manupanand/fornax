// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! CI state machine types: pipeline, job and attempt statuses, waiting reasons, the allowed
//! transitions, and pure helpers (DAG validation, label matching, pipeline result).
//!
//! No I/O. The persistence layer (`fornax-db`) stores the `as_str` forms and only performs
//! transitions that [`JobStatus::can_transition_to`] allows.
//!
//! Job state machine (architecture §6):
//!
//! ```text
//! waiting(dependency) --all needs success--> queued --claim--> running --> success
//!        |                                     ^                  |  \--> failed (no attempts left)
//!        | a need failed / was skipped         |                  |
//!        v                                     +-- failed/lost, --+
//!     skipped                                      attempts left
//!
//! waiting | queued | running --cancel--> canceled
//! ```

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::str::FromStr;

use crate::ValidationError;

macro_rules! str_enum {
    ($(#[$m:meta])* $name:ident, $field:literal { $($(#[$vm:meta])* $var:ident => $s:literal),+ $(,)? }) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name {
            $($(#[$vm])* $var),+
        }

        impl $name {
            /// Every value, in declaration order.
            pub const ALL: &'static [Self] = &[$(Self::$var),+];

            /// Stable lowercase form, as stored in the database and shown in the API.
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$var => $s),+
                }
            }
        }

        impl FromStr for $name {
            type Err = ValidationError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                match s {
                    $($s => Ok(Self::$var),)+
                    _ => Err(ValidationError::new($field, "unknown value")),
                }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

str_enum!(
    /// Status of a whole pipeline run.
    PipelineStatus, "pipeline status" {
        /// Created; no job has been claimed yet.
        Pending => "pending",
        /// At least one job has been claimed by a runner.
        Running => "running",
        /// Every job finished and none failed or was canceled.
        Success => "success",
        /// Every job is terminal and at least one failed.
        Failed => "failed",
        /// The pipeline was canceled (or a job was canceled and none failed).
        Canceled => "canceled",
    }
);

str_enum!(
    /// Status of one job of a pipeline.
    JobStatus, "job status" {
        /// Not runnable yet; see [`WaitingReason`].
        Waiting => "waiting",
        /// Runnable; waiting for a matching runner to claim it.
        Queued => "queued",
        /// Claimed by a runner (a `running` attempt with a lease exists).
        Running => "running",
        /// Finished successfully.
        Success => "success",
        /// Finished unsuccessfully with no attempts left.
        Failed => "failed",
        /// Canceled by a user before or while running.
        Canceled => "canceled",
        /// Not run because a job it needs failed (or was skipped).
        Skipped => "skipped",
    }
);

str_enum!(
    /// Why a job is [`JobStatus::Waiting`] (Jenkins-style visibility).
    WaitingReason, "waiting reason" {
        /// One or more jobs it `needs` have not succeeded yet.
        Dependency => "dependency",
        /// Waiting for a manual approval (untrusted refs, protected environments). Reserved.
        Approval => "approval",
        /// Waiting for a concurrency group / resource lock. Reserved.
        Resource => "resource",
        /// No registered runner can match its labels. Reserved.
        NoRunner => "no_runner",
    }
);

str_enum!(
    /// Status of one execution attempt of a job.
    AttemptStatus, "attempt status" {
        /// The runner holds the lease.
        Running => "running",
        /// The runner reported success.
        Success => "success",
        /// The runner reported failure.
        Failed => "failed",
        /// The lease expired before the runner reported a result.
        Lost => "lost",
        /// The job was canceled while this attempt ran.
        Canceled => "canceled",
    }
);

impl PipelineStatus {
    /// `success`, `failed` or `canceled`.
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Success | Self::Failed | Self::Canceled)
    }
}

impl JobStatus {
    /// `success`, `failed`, `canceled` or `skipped`.
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Success | Self::Failed | Self::Canceled | Self::Skipped
        )
    }

    /// Whether the scheduler may move a job from `self` to `next`.
    pub const fn can_transition_to(self, next: Self) -> bool {
        use JobStatus::*;
        matches!(
            (self, next),
            (Waiting, Queued | Skipped | Canceled)
                | (Queued, Running | Canceled)
                | (Running, Success | Failed | Queued | Canceled)
        )
    }
}

impl AttemptStatus {
    /// Everything except `running`.
    pub const fn is_terminal(self) -> bool {
        !matches!(self, Self::Running)
    }
}

/// What a runner learns when it renews the lease of its attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseState {
    /// Lease renewed; keep running.
    Ok,
    /// Lease renewed, but the job was canceled: stop the job and report a result.
    CancelRequested,
    /// The runner no longer holds this attempt (expired, already finished, or not its
    /// attempt). Stop immediately; a result will be rejected.
    Lost,
}

/// Final pipeline status once every job is terminal; `None` while any job is not.
///
/// `cancel_requested` wins; otherwise any `failed` job ⇒ `failed`; otherwise any
/// `canceled` job ⇒ `canceled`; otherwise `success` (`skipped` jobs only occur below a
/// failed job, so they never decide the result on their own). An empty pipeline succeeds.
pub fn pipeline_result(
    jobs: impl IntoIterator<Item = JobStatus>,
    cancel_requested: bool,
) -> Option<PipelineStatus> {
    let (mut failed, mut canceled) = (false, false);
    for status in jobs {
        match status {
            JobStatus::Failed => failed = true,
            JobStatus::Canceled => canceled = true,
            s if !s.is_terminal() => return None,
            _ => {}
        }
    }
    Some(if cancel_requested {
        PipelineStatus::Canceled
    } else if failed {
        PipelineStatus::Failed
    } else if canceled {
        PipelineStatus::Canceled
    } else {
        PipelineStatus::Success
    })
}

/// A runner offering `offered` labels may run a job that requires `required` iff
/// `required ⊆ offered`.
pub fn labels_match(required: &[String], offered: &[String]) -> bool {
    required.iter().all(|l| offered.contains(l))
}

/// Why a set of jobs and their `needs` is not a valid DAG.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DagError {
    #[error("duplicate job name {0:?}")]
    DuplicateJob(String),
    #[error("job {job:?} needs unknown job {need:?}")]
    UnknownNeed { job: String, need: String },
    #[error("dependency cycle through job {0:?}")]
    Cycle(String),
}

/// Check that job names are unique, every need names a job of the same set, and there is
/// no cycle (a job needing itself is a cycle). `jobs` yields `(name, needs)`.
pub fn validate_dag<'a, I, N>(jobs: I) -> Result<(), DagError>
where
    I: IntoIterator<Item = (&'a str, N)>,
    N: IntoIterator<Item = &'a str>,
{
    let mut graph: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut order = Vec::new();
    for (name, needs) in jobs {
        if graph.insert(name, needs.into_iter().collect()).is_some() {
            return Err(DagError::DuplicateJob(name.to_owned()));
        }
        order.push(name);
    }
    for &job in &order {
        let needs = graph.get(job).map(Vec::as_slice).unwrap_or_default();
        if let Some(need) = needs.iter().find(|n| !graph.contains_key(*n)) {
            return Err(DagError::UnknownNeed {
                job: job.to_owned(),
                need: (*need).to_owned(),
            });
        }
    }
    // Iterative depth-first search; `on_path` holds the current chain.
    let mut done: HashSet<&str> = HashSet::new();
    let mut on_path: HashSet<&str> = HashSet::new();
    for &start in &order {
        if done.contains(start) {
            continue;
        }
        let mut stack: Vec<(&str, usize)> = vec![(start, 0)];
        on_path.insert(start);
        while let Some((node, idx)) = stack.pop() {
            let needs = graph.get(node).map(Vec::as_slice).unwrap_or_default();
            if let Some(&next) = needs.get(idx) {
                stack.push((node, idx + 1));
                if on_path.contains(next) {
                    return Err(DagError::Cycle(next.to_owned()));
                }
                if !done.contains(next) {
                    on_path.insert(next);
                    stack.push((next, 0));
                }
            } else {
                on_path.remove(node);
                done.insert(node);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_strings() {
        for s in PipelineStatus::ALL {
            assert_eq!(s.as_str().parse::<PipelineStatus>(), Ok(*s));
        }
        for s in JobStatus::ALL {
            assert_eq!(s.as_str().parse::<JobStatus>(), Ok(*s));
        }
        for s in WaitingReason::ALL {
            assert_eq!(s.as_str().parse::<WaitingReason>(), Ok(*s));
        }
        for s in AttemptStatus::ALL {
            assert_eq!(s.as_str().parse::<AttemptStatus>(), Ok(*s));
        }
        assert!("bogus".parse::<JobStatus>().is_err());
        assert_eq!(WaitingReason::NoRunner.as_str(), "no_runner");
    }

    #[test]
    fn transitions() {
        use JobStatus::*;
        assert!(Waiting.can_transition_to(Queued));
        assert!(Running.can_transition_to(Queued));
        assert!(!Waiting.can_transition_to(Running));
        assert!(!Queued.can_transition_to(Success));
        for t in [Success, Failed, Canceled, Skipped] {
            assert!(t.is_terminal());
            assert!(JobStatus::ALL.iter().all(|n| !t.can_transition_to(*n)));
        }
    }

    #[test]
    fn result_of_pipeline() {
        use JobStatus::*;
        assert_eq!(pipeline_result([Success, Running], false), None);
        assert_eq!(
            pipeline_result([Success, Success], false),
            Some(PipelineStatus::Success)
        );
        assert_eq!(
            pipeline_result([Success, Failed, Skipped], false),
            Some(PipelineStatus::Failed)
        );
        assert_eq!(
            pipeline_result([Canceled, Success], false),
            Some(PipelineStatus::Canceled)
        );
        assert_eq!(
            pipeline_result([Failed], true),
            Some(PipelineStatus::Canceled)
        );
        assert_eq!(pipeline_result([], false), Some(PipelineStatus::Success));
    }

    #[test]
    fn label_subset() {
        let l = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert!(labels_match(&l(&[]), &l(&["linux"])));
        assert!(labels_match(&l(&["linux"]), &l(&["docker", "linux"])));
        assert!(!labels_match(&l(&["linux", "docker"]), &l(&["linux"])));
    }

    #[test]
    fn dag_validation() {
        let ok: [(&str, Vec<&str>); 4] = [
            ("a", vec![]),
            ("b", vec!["a"]),
            ("c", vec!["a"]),
            ("d", vec!["b", "c"]),
        ];
        assert_eq!(
            validate_dag(ok.iter().map(|(n, d)| (*n, d.clone()))),
            Ok(())
        );
        assert_eq!(
            validate_dag([("a", vec![]), ("a", vec![])]),
            Err(DagError::DuplicateJob("a".into()))
        );
        assert!(matches!(
            validate_dag([("a", vec!["x"])]),
            Err(DagError::UnknownNeed { .. })
        ));
        assert!(matches!(
            validate_dag([("a", vec!["a"])]),
            Err(DagError::Cycle(_))
        ));
        assert!(matches!(
            validate_dag([("a", vec!["c"]), ("b", vec!["a"]), ("c", vec!["b"])]),
            Err(DagError::Cycle(_))
        ));
    }
}
