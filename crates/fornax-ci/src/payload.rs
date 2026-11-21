// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! The job payload stored with every job and handed to runners.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::CiError;

/// Current [`JobPayload::v`].
pub const PAYLOAD_VERSION: u32 = 1;

/// A resolved job, as stored in `jobs.payload` (JSON, opaque to the database).
///
/// The runner server turns it into a protocol `Job`. Evolution is additive: new optional
/// fields may be added within a version (unknown fields are ignored when decoding); an
/// incompatible change bumps [`PAYLOAD_VERSION`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobPayload {
    /// Payload format version ([`PAYLOAD_VERSION`]).
    pub v: u32,
    /// Concrete job name (matrix jobs include their values, e.g. `build (os=linux)`).
    pub name: String,
    /// Steps in order (never empty for jobs created from a spec).
    pub steps: Vec<StepPayload>,
    /// Job environment: pipeline `variables` overlaid by the job's `env`. Step `env`
    /// overrides these per step.
    pub env: BTreeMap<String, String>,
    /// Job timeout in seconds.
    pub timeout_secs: u64,
    /// Repository as `owner/name`.
    pub repo_full_name: String,
    /// Commit to check out.
    pub sha: String,
    /// Full ref that triggered the job, e.g. `refs/heads/main`.
    #[serde(rename = "ref")]
    pub git_ref: String,
}

/// One step of a [`JobPayload`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepPayload {
    /// Display name: the step's `name:`, or `step N` (1-based).
    pub name: String,
    /// Shell script to run.
    pub run: String,
    /// Step environment (overrides the job's).
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Step timeout in seconds, if set.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// Working directory relative to the checkout, if set.
    #[serde(default)]
    pub working_directory: Option<String>,
}

impl JobPayload {
    /// Encode as JSON.
    pub fn to_json(&self) -> Result<String, CiError> {
        Ok(serde_json::to_string(self)?)
    }

    /// Decode from JSON, rejecting versions other than [`PAYLOAD_VERSION`].
    pub fn from_json(json: &str) -> Result<Self, CiError> {
        #[derive(Deserialize)]
        struct Version {
            v: u32,
        }
        let Version { v } = serde_json::from_str(json)?;
        if v != PAYLOAD_VERSION {
            return Err(CiError::PayloadVersion(v));
        }
        Ok(serde_json::from_str(json)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> JobPayload {
        JobPayload {
            v: PAYLOAD_VERSION,
            name: "build".into(),
            steps: vec![StepPayload {
                name: "step 1".into(),
                run: "make".into(),
                env: BTreeMap::from([("A".into(), "1".into())]),
                timeout_secs: Some(60),
                working_directory: Some("src".into()),
            }],
            env: BTreeMap::new(),
            timeout_secs: 3600,
            repo_full_name: "alice/demo".into(),
            sha: "abc".into(),
            git_ref: "refs/heads/main".into(),
        }
    }

    #[test]
    fn round_trip_and_field_names() {
        let p = sample();
        let json = p.to_json().expect("encode");
        let value: serde_json::Value = serde_json::from_str(&json).expect("json");
        assert_eq!(value["v"], 1);
        assert_eq!(value["ref"], "refs/heads/main");
        assert_eq!(value["steps"][0]["working_directory"], "src");
        assert_eq!(JobPayload::from_json(&json).expect("decode"), p);
    }

    #[test]
    fn rejects_other_versions_and_ignores_unknown_fields() {
        let mut value = serde_json::to_value(sample()).expect("value");
        value["future_field"] = serde_json::json!(true);
        assert!(JobPayload::from_json(&value.to_string()).is_ok());
        value["v"] = serde_json::json!(2);
        assert!(matches!(
            JobPayload::from_json(&value.to_string()),
            Err(CiError::PayloadVersion(2))
        ));
        assert!(matches!(
            JobPayload::from_json("{}"),
            Err(CiError::Payload(_))
        ));
    }
}
