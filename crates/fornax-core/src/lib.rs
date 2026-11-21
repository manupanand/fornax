// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! Fornax domain types and validation rules.
//!
//! This crate performs no I/O. Everything that crosses a trust boundary (user names,
//! repository names, Git ref names) is validated here before it reaches storage or `git`.

pub mod ci;
pub mod names;
pub mod perm;
pub mod refname;

use std::time::{SystemTime, UNIX_EPOCH};

/// Current time as Unix seconds (UTC).
pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// A validation failure with a human-readable reason.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{field}: {reason}")]
pub struct ValidationError {
    pub field: &'static str,
    pub reason: &'static str,
}

impl ValidationError {
    pub const fn new(field: &'static str, reason: &'static str) -> Self {
        Self { field, reason }
    }
}
