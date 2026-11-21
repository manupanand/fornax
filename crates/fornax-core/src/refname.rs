// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! Git ref-name validation, following the rules of `git check-ref-format`.
//!
//! Parity unit U9 (`docs/research/rust-anything.md`) will record Gitea's
//! `modules/git/ref.go` and `git check-ref-format --branch` against generated names and
//! replay them against this module; until then the rules below are written from the
//! `git-check-ref-format(1)` manual.

use crate::ValidationError;

/// Validate a branch name as typed by a user (e.g. `main`, `feature/login`), i.e. the part
/// after `refs/heads/`. Applies every `git check-ref-format` rule plus the `--branch`
/// restriction that a branch must not start with `-`.
pub fn validate_branch_name(name: &str) -> Result<(), ValidationError> {
    if name.starts_with('-') {
        return Err(ValidationError::new("branch", "must not start with '-'"));
    }
    validate_ref_path(name, "branch")
}

/// Validate a ref path (with or without the `refs/` prefix) against the
/// `git check-ref-format` rules, with `--allow-onelevel` semantics.
pub fn validate_ref_path(name: &str, field: &'static str) -> Result<(), ValidationError> {
    let err = |reason| Err(ValidationError::new(field, reason));
    if name.is_empty() {
        return err("must not be empty");
    }
    if name == "@" {
        return err("must not be '@'");
    }
    if name.starts_with('/') || name.ends_with('/') || name.contains("//") {
        return err("must not start or end with '/' or contain '//'");
    }
    if name.ends_with('.') {
        return err("must not end with '.'");
    }
    if name.contains("..") {
        return err("must not contain '..'");
    }
    if name.contains("@{") {
        return err("must not contain '@{'");
    }
    for b in name.bytes() {
        if b < 0x20 || b == 0x7f {
            return err("must not contain control characters");
        }
        if matches!(b, b' ' | b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\') {
            return err("must not contain space, '~', '^', ':', '?', '*', '[' or '\\'");
        }
    }
    for component in name.split('/') {
        if component.starts_with('.') {
            return err("components must not start with '.'");
        }
        if component.ends_with(".lock") {
            return err("components must not end with '.lock'");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_branches() {
        for ok in [
            "main",
            "feature/login",
            "release-1.0",
            "a.b",
            "fix_1",
            "x/y/z",
            "ü",
        ] {
            assert!(validate_branch_name(ok).is_ok(), "{ok}");
        }
    }

    #[test]
    fn invalid_branches() {
        for bad in [
            "",
            "@",
            "-x",
            "/a",
            "a/",
            "a//b",
            "a.",
            "a..b",
            "a@{b",
            "a b",
            "a~1",
            "a^",
            "a:b",
            "a?",
            "a*",
            "a[",
            "a\\b",
            ".a",
            "a/.b",
            "a.lock",
            "a/b.lock/c",
            "a\u{7f}",
            "a\tb",
        ] {
            assert!(validate_branch_name(bad).is_err(), "{bad:?}");
        }
    }
}
