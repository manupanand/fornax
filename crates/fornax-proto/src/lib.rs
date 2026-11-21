// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! Fornax runner protocol (task C-005, LOG D-015).
//!
//! A small, versioned gRPC contract between the Fornax server and runner agents. The agent
//! initiates every call (`Register`, long-poll `Next`, `Extend`, `Update`, client-streaming
//! `Log`, `Done`); the server never dials the agent and never ships code to it.
//!
//! The source of truth is `proto/fornax/runner/v1/runner.proto`. It is compiled at build time by
//! `protox` (pure Rust, no `protoc` needed) and `tonic-prost-build`.
//!
//! # Compatibility rules
//!
//! - Fields and enum values are only **added**; existing numbers are never renumbered, retyped
//!   or reused. Removed fields become `reserved`.
//! - Enum value `0` is always `*_UNSPECIFIED`; receivers treat unknown values as unspecified.
//! - New behaviour must be safe when the peer ignores the new field (old agents / old servers).
//! - A breaking change creates a new package (`fornax.runner.v2`), served next to `v1`.
//!
//! # Authentication
//!
//! Every RPC except `Register` carries `authorization: Bearer fnr_<64 hex>` metadata
//! ([`AUTHORIZATION_HEADER`], [`RUNNER_TOKEN_PREFIX`]).

/// Metadata key carrying the runner token on every call except `Register`.
pub const AUTHORIZATION_HEADER: &str = "authorization";

/// Prefix of runner tokens (`fnr_` followed by 64 lowercase hex characters).
pub const RUNNER_TOKEN_PREFIX: &str = "fnr_";

/// Returns `true` when `token` has the runner-token shape `fnr_<64 lowercase hex>`.
///
/// This only checks the format; the server must still look the (hashed) token up.
pub fn is_runner_token_format(token: &str) -> bool {
    token.strip_prefix(RUNNER_TOKEN_PREFIX).is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

/// Generated code for the `fornax.runner` protobuf packages.
pub mod runner {
    /// Version 1 of the runner protocol: messages, `runner_client::RunnerClient` and
    /// `runner_server::{Runner, RunnerServer}`.
    #[allow(missing_docs, clippy::all, clippy::pedantic)]
    pub mod v1 {
        tonic::include_proto!("fornax.runner.v1");
    }
}

#[cfg(test)]
mod tests {
    use super::runner::v1::{Job, NextResponse, Step};
    use super::*;
    use prost::Message;

    #[test]
    fn token_format() {
        let ok = format!("fnr_{}", "a1".repeat(32));
        assert!(is_runner_token_format(&ok));
        assert!(!is_runner_token_format("fnr_abc"));
        assert!(!is_runner_token_format(&ok.to_uppercase()));
        assert!(!is_runner_token_format(&format!("fnx_{}", "a1".repeat(32))));
    }

    #[test]
    fn job_roundtrip_and_empty_next() {
        let job = Job {
            attempt_id: "a1".into(),
            r#ref: "refs/heads/main".into(),
            steps: vec![Step {
                name: "build".into(),
                run: "make".into(),
                ..Default::default()
            }],
            mask: vec!["s3cr3t".into()],
            ..Default::default()
        };
        let resp = NextResponse {
            job: Some(job.clone()),
        };
        let decoded = NextResponse::decode(resp.encode_to_vec().as_slice()).expect("decode");
        assert_eq!(decoded.job, Some(job));
        let empty = NextResponse::decode(&[][..]).expect("decode empty");
        assert!(empty.job.is_none());
    }
}
