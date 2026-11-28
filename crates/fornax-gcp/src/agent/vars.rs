// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! Pure helpers shared by the agent and the deploy: version tags, tag selection and
//! `release.vars` parsing (exactly as agent.py / deploy.sh do it).

use std::path::Path;

/// `v[0-9]+(\.[0-9]+){0,3}` — the version part of every tag.
pub fn is_version(s: &str) -> bool {
    let Some(rest) = s.strip_prefix('v') else {
        return false;
    };
    let parts: Vec<&str> = rest.split('.').collect();
    (1..=4).contains(&parts.len())
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

/// `^<prefix>v[0-9]+(\.[0-9]+){0,3}$`.
pub fn is_tag_for(prefix: &str, tag: &str) -> bool {
    tag.strip_prefix(prefix).is_some_and(is_version)
}

/// The newest valid tag from `git tag -l '<prefix>v*' --sort=-v:refname` output (first match).
pub fn select_latest(prefix: &str, git_tag_output: &str) -> Option<String> {
    git_tag_output
        .lines()
        .map(str::trim)
        .find(|t| is_tag_for(prefix, t))
        .map(str::to_owned)
}

/// `KEY=value` pairs of a `release.vars` file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReleaseVars {
    text: String,
}

impl ReleaseVars {
    /// Parse file content.
    pub fn parse(text: &str) -> Self {
        Self {
            text: text.to_owned(),
        }
    }

    /// Read `<dir>/release.vars`; a missing file has no keys.
    pub fn load(dir: &Path) -> Self {
        Self::parse(&std::fs::read_to_string(dir.join("release.vars")).unwrap_or_default())
    }

    /// Value of `key`, like deploy.sh's `var`: the last line `KEY=...` wins, one trailing CR is
    /// removed, then one pair of surrounding double quotes. Empty when not set.
    pub fn get(&self, key: &str) -> String {
        let prefix = format!("{key}=");
        let Some(raw) = self
            .text
            .split('\n')
            .filter_map(|l| l.strip_prefix(prefix.as_str()))
            .next_back()
        else {
            return String::new();
        };
        let v = raw.strip_suffix('\r').unwrap_or(raw);
        match v.strip_prefix('"').and_then(|x| x.strip_suffix('"')) {
            Some(inner) => inner.to_owned(),
            None => v.to_owned(),
        }
    }

    /// Whitespace-separated list (`read -ra` on the value).
    pub fn list(&self, key: &str) -> Vec<String> {
        self.get(key)
            .split([' ', '\t', '\n'])
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_pattern() {
        for ok in ["v1", "v0.1.0", "v1.2.3.4", "v10.0"] {
            assert!(is_version(ok), "{ok}");
        }
        for bad in [
            "v",
            "1.0",
            "v1.2.3.4.5",
            "v1..2",
            "v1.",
            "vfoo",
            "v1.0.0-rc1",
            "v9.9;touch-pwned",
            "v1 ",
        ] {
            assert!(!is_version(bad), "{bad}");
        }
        assert!(is_tag_for("cache-", "cache-v0.1.0"));
        assert!(!is_tag_for("cache-", "v0.1.0") && !is_tag_for("", "cache-v0.1.0"));
    }

    #[test]
    fn tag_selection_takes_first_valid_in_git_order() {
        // git already sorted (-v:refname); hostile and pre-release tags are skipped.
        let out = "v9.9;touch-pwned\nvfoo\nv1.0.0-rc1\nv0.13.0\nv0.12.0\n";
        assert_eq!(select_latest("", out).as_deref(), Some("v0.13.0"));
        assert_eq!(select_latest("", "vfoo\n"), None);
        assert_eq!(select_latest("", ""), None);
        let shared = "cache-v0.2.0\ncache-v0.1.0\n";
        assert_eq!(
            select_latest("cache-", shared).as_deref(),
            Some("cache-v0.2.0")
        );
        assert_eq!(select_latest("", shared), None);
    }

    #[test]
    fn release_vars_last_wins_quotes_and_cr() {
        let v = ReleaseVars::parse(
            "IMAGE=a:1\nWATCH_PATHS=\"build\"\r\nWATCH_PATHS=\"dist  web\"\r\nX_IMAGE=no\n\
             HEALTH_TIMEOUT=300\r\nQ='single'\nHALF=\"open\n",
        );
        assert_eq!(v.get("IMAGE"), "a:1");
        assert_eq!(v.get("WATCH_PATHS"), "dist  web");
        assert_eq!(v.list("WATCH_PATHS"), ["dist", "web"]);
        assert_eq!(v.get("HEALTH_TIMEOUT"), "300");
        assert_eq!(v.get("Q"), "'single'");
        assert_eq!(v.get("HALF"), "\"open");
        assert_eq!(v.get("JOB_SERVICE"), "");
        assert!(v.list("JOB_SERVICE").is_empty());
        // `KEY=` must start the line; a longer key does not match.
        assert_eq!(
            ReleaseVars::parse(" IMAGE=x\nMY_IMAGE=y\n").get("IMAGE"),
            ""
        );
        assert_eq!(
            ReleaseVars::load(Path::new("/nonexistent-dir")).get("IMAGE"),
            ""
        );
    }
}
