// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! User and repository name rules.
//!
//! Names become path segments on disk (`repos/<owner>/<name>.git`) and URL segments, so
//! the rules are deliberately strict: ASCII only, no leading dot or dash, no `.git` suffix.

use crate::ValidationError;

/// First URL segments that can never be user names because the server routes them.
pub const RESERVED_NAMES: &[&str] = &[
    "admin",
    "api",
    "assets",
    "explore",
    "favicon.ico",
    "health",
    "login",
    "logout",
    "metrics",
    "new",
    "oauth",
    "settings",
    "static",
    "user",
    "users",
];

const MAX_USERNAME_LEN: usize = 39;
const MAX_REPO_NAME_LEN: usize = 100;

/// Validate a user (or, later, organisation) name: 1–39 chars of `[A-Za-z0-9_-]`,
/// starting with a letter or digit, not reserved.
pub fn validate_username(name: &str) -> Result<(), ValidationError> {
    const F: &str = "username";
    if name.is_empty() || name.len() > MAX_USERNAME_LEN {
        return Err(ValidationError::new(F, "must be 1-39 characters"));
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(ValidationError::new(
            F,
            "may only contain letters, digits, '-' and '_'",
        ));
    }
    if !name.as_bytes()[0].is_ascii_alphanumeric() {
        return Err(ValidationError::new(F, "must start with a letter or digit"));
    }
    if RESERVED_NAMES.contains(&name.to_ascii_lowercase().as_str()) {
        return Err(ValidationError::new(F, "is reserved"));
    }
    Ok(())
}

/// Validate a repository name: 1–100 chars of `[A-Za-z0-9._-]`, not starting with `.` or
/// `-`, not ending in `.git`, and not `.`/`..`.
pub fn validate_repo_name(name: &str) -> Result<(), ValidationError> {
    const F: &str = "name";
    if name.is_empty() || name.len() > MAX_REPO_NAME_LEN {
        return Err(ValidationError::new(F, "must be 1-100 characters"));
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        return Err(ValidationError::new(
            F,
            "may only contain letters, digits, '-', '_' and '.'",
        ));
    }
    if name.starts_with('.') || name.starts_with('-') {
        return Err(ValidationError::new(F, "must not start with '.' or '-'"));
    }
    if name.to_ascii_lowercase().ends_with(".git") {
        return Err(ValidationError::new(F, "must not end with '.git'"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usernames() {
        for ok in ["alice", "Bob_2", "a", "x-y", &"a".repeat(39)] {
            assert!(validate_username(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "-a",
            "_a",
            "a.b",
            "a b",
            "ä",
            "API",
            "users",
            &"a".repeat(40),
        ] {
            assert!(validate_username(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn repo_names() {
        for ok in ["fornax", "my.repo", "a-b_c", "v1.0", "A"] {
            assert!(validate_repo_name(ok).is_ok(), "{ok}");
        }
        for bad in [
            "", ".", "..", ".hidden", "-x", "x.git", "X.GIT", "a/b", "a b", "ü",
        ] {
            assert!(validate_repo_name(bad).is_err(), "{bad}");
        }
    }
}
