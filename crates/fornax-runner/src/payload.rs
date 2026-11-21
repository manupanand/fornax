// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! Job payloads: decoded with [`fornax_ci::JobPayload`], the single definition shared with the
//! push path that writes them, then converted to protobuf.
//!
//! An earlier private mirror of the format drifted from the writer (`timeout_secs: null` for
//! steps without a timeout was rejected); using the writer's own type removes that class of
//! bug (D-040).

use std::collections::HashMap;
use std::ops::Deref;

use fornax_proto::runner::v1::Step;

pub use fornax_ci::PAYLOAD_VERSION;

/// A decoded job payload plus protobuf conversions.
#[derive(Debug, Clone)]
pub(crate) struct JobPayload(fornax_ci::JobPayload);

impl Deref for JobPayload {
    type Target = fornax_ci::JobPayload;
    fn deref(&self) -> &fornax_ci::JobPayload {
        &self.0
    }
}

impl JobPayload {
    /// Decode a payload written by `fornax-ci`. Unknown fields are ignored; a different
    /// version is an error.
    pub fn parse(json: &str) -> Result<Self, fornax_ci::CiError> {
        fornax_ci::JobPayload::from_json(json).map(Self)
    }

    /// Job-level environment for the protobuf `Job`.
    pub fn proto_env(&self) -> HashMap<String, String> {
        self.0
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// Steps for the protobuf `Job` (`0` = no step timeout, empty = workspace root).
    pub fn proto_steps(&self) -> Vec<Step> {
        self.0
            .steps
            .iter()
            .map(|s| Step {
                name: s.name.clone(),
                run: s.run.clone(),
                env: s.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                timeout_secs: s.timeout_secs.unwrap_or(0),
                working_directory: s.working_directory.clone().unwrap_or_default(),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn written_by_ci(step_timeout: Option<u64>) -> String {
        fornax_ci::JobPayload {
            v: PAYLOAD_VERSION,
            name: "build".into(),
            steps: vec![fornax_ci::StepPayload {
                name: "s".into(),
                run: "echo hi".into(),
                env: BTreeMap::from([("X".into(), "y".into())]),
                timeout_secs: step_timeout,
                working_directory: None,
            }],
            env: BTreeMap::from([("A".into(), "1".into())]),
            timeout_secs: 60,
            repo_full_name: "o/r".into(),
            sha: "a".repeat(40),
            git_ref: "refs/heads/main".into(),
        }
        .to_json()
        .expect("json")
    }

    #[test]
    fn round_trips_what_fornax_ci_writes() {
        for timeout in [None, Some(5)] {
            let p = JobPayload::parse(&written_by_ci(timeout)).expect("parse");
            assert_eq!((p.name.as_str(), p.timeout_secs), ("build", 60));
            let steps = p.proto_steps();
            assert_eq!(steps[0].timeout_secs, timeout.unwrap_or(0));
            assert_eq!(steps[0].working_directory, "");
            assert_eq!(steps[0].env.get("X").map(String::as_str), Some("y"));
            assert_eq!(p.proto_env().get("A").map(String::as_str), Some("1"));
        }
    }

    #[test]
    fn rejects_garbage_and_other_versions() {
        assert!(JobPayload::parse("").is_err());
        assert!(JobPayload::parse("{").is_err());
        assert!(JobPayload::parse(r#"{"v":2}"#).is_err());
    }
}
