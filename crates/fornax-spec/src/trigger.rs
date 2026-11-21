// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! Trigger filters (`on:`) and the events they are matched against (C-009).
//!
//! Rules (GitHub Actions / Gitea semantics):
//!
//! - Branch/tag filters use [`PatternList`] ("last matching pattern wins"); an `*-ignore`
//!   list excludes a ref when the ref is included by that list. An empty list means "no
//!   filter".
//! - If only branch filters are set, tag pushes do not trigger (and vice versa). With
//!   neither, both branch and tag pushes trigger. Pushes of other refs (e.g. `refs/notes/…`)
//!   trigger only when no branch/tag filter is set.
//! - `paths`: the event triggers if **any** changed path is included. `paths-ignore`: the
//!   event does not trigger if **every** changed path is ignored.
//! - Path filters are not evaluated for tag pushes.
//! - **Pushes without path information** (`changed_paths: None`, e.g. a new branch with no
//!   base to diff against) **pass path filters**: we would rather run CI once too often
//!   than silently skip it. `Some(vec![])` (known to change nothing) fails a `paths`
//!   filter and is fully ignored by a `paths-ignore` filter.
//! - More than [`MAX_FILTER_PATHS`] changed paths count as "no path information" too, which
//!   bounds the matching work per event.

use crate::cron::CronSchedule;
use crate::pattern::PatternList;

/// Events with more changed paths than this are treated as having no path information
/// (path filters pass) instead of matching every path against every pattern.
pub const MAX_FILTER_PATHS: usize = 1000;

/// Filters for `on.push`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PushFilter {
    /// `branches`.
    pub branches: PatternList,
    /// `branches-ignore`.
    pub branches_ignore: PatternList,
    /// `tags`.
    pub tags: PatternList,
    /// `tags-ignore`.
    pub tags_ignore: PatternList,
    /// `paths`.
    pub paths: PatternList,
    /// `paths-ignore`.
    pub paths_ignore: PatternList,
}

/// Filters for `on.pull_request` (branches are matched against the **base** branch).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PullRequestFilter {
    /// `branches`.
    pub branches: PatternList,
    /// `branches-ignore`.
    pub branches_ignore: PatternList,
    /// `paths`.
    pub paths: PatternList,
    /// `paths-ignore`.
    pub paths_ignore: PatternList,
}

/// The `on:` section of a pipeline.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct On {
    /// `on.push`, if enabled.
    pub push: Option<PushFilter>,
    /// `on.pull_request`, if enabled.
    pub pull_request: Option<PullRequestFilter>,
    /// `on.schedule` cron entries (UTC).
    pub schedule: Vec<CronSchedule>,
    /// `on.manual` is enabled.
    pub manual: bool,
}

/// Something that may trigger a pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A push.
    Push {
        /// Full ref name: `refs/heads/…` or `refs/tags/…`.
        ref_name: String,
        /// Changed paths relative to the repository root; `None` when unknown.
        changed_paths: Option<Vec<String>>,
    },
    /// A pull request was opened or updated.
    PullRequest {
        /// Target branch (`main` or `refs/heads/main`).
        base_branch: String,
        /// Changed paths; `None` when unknown.
        changed_paths: Option<Vec<String>>,
    },
    /// A cron schedule fired.
    Schedule,
    /// A user started the pipeline by hand.
    Manual,
}

impl Event {
    /// The event name as seen by expressions (`event`).
    pub fn name(&self) -> &'static str {
        match self {
            Event::Push { .. } => "push",
            Event::PullRequest { .. } => "pull_request",
            Event::Schedule => "schedule",
            Event::Manual => "manual",
        }
    }
}

fn ref_passes(include: &PatternList, ignore: &PatternList, name: &str) -> bool {
    (include.is_empty() || include.includes(name)) && (ignore.is_empty() || !ignore.includes(name))
}

fn paths_pass(include: &PatternList, ignore: &PatternList, paths: Option<&[String]>) -> bool {
    let Some(paths) = paths.filter(|p| p.len() <= MAX_FILTER_PATHS) else {
        return true;
    };
    if !include.is_empty() && !paths.iter().any(|p| include.includes(p)) {
        return false;
    }
    if !ignore.is_empty() && paths.iter().all(|p| ignore.includes(p)) {
        return false;
    }
    true
}

fn push_matches(f: &PushFilter, ref_name: &str, paths: Option<&[String]>) -> bool {
    let has_branch = !f.branches.is_empty() || !f.branches_ignore.is_empty();
    let has_tag = !f.tags.is_empty() || !f.tags_ignore.is_empty();
    if let Some(branch) = ref_name.strip_prefix("refs/heads/") {
        if has_tag && !has_branch {
            return false;
        }
        ref_passes(&f.branches, &f.branches_ignore, branch)
            && paths_pass(&f.paths, &f.paths_ignore, paths)
    } else if let Some(tag) = ref_name.strip_prefix("refs/tags/") {
        if has_branch && !has_tag {
            return false;
        }
        ref_passes(&f.tags, &f.tags_ignore, tag)
    } else {
        !has_branch && !has_tag && paths_pass(&f.paths, &f.paths_ignore, paths)
    }
}

/// Whether `event` triggers a pipeline with this `on:` section.
pub fn on_matches(on: &On, event: &Event) -> bool {
    match event {
        Event::Push {
            ref_name,
            changed_paths,
        } => on
            .push
            .as_ref()
            .is_some_and(|f| push_matches(f, ref_name, changed_paths.as_deref())),
        Event::PullRequest {
            base_branch,
            changed_paths,
        } => on.pull_request.as_ref().is_some_and(|f| {
            let base = base_branch
                .strip_prefix("refs/heads/")
                .unwrap_or(base_branch);
            ref_passes(&f.branches, &f.branches_ignore, base)
                && paths_pass(&f.paths, &f.paths_ignore, changed_paths.as_deref())
        }),
        Event::Schedule => !on.schedule.is_empty(),
        Event::Manual => on.manual,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pl(p: &[&str]) -> PatternList {
        PatternList::new(p).expect("valid")
    }

    fn push(r: &str, paths: Option<&[&str]>) -> Event {
        Event::Push {
            ref_name: r.into(),
            changed_paths: paths.map(|p| p.iter().map(|s| s.to_string()).collect()),
        }
    }

    fn on_push(f: PushFilter) -> On {
        On {
            push: Some(f),
            ..On::default()
        }
    }

    #[test]
    fn unfiltered_push_matches_branches_and_tags() {
        let on = on_push(PushFilter::default());
        assert!(on_matches(&on, &push("refs/heads/x", None)));
        assert!(on_matches(&on, &push("refs/tags/v1", None)));
        assert!(!on_matches(&on, &Event::Manual));
        assert!(!on_matches(&on, &Event::Schedule));
    }

    #[test]
    fn branch_filters() {
        let on = on_push(PushFilter {
            branches: pl(&["main", "release/**", "!release/old/**"]),
            ..PushFilter::default()
        });
        assert!(on_matches(&on, &push("refs/heads/main", None)));
        assert!(on_matches(&on, &push("refs/heads/release/1.0", None)));
        assert!(!on_matches(&on, &push("refs/heads/release/old/1", None)));
        assert!(!on_matches(&on, &push("refs/heads/dev", None)));
        assert!(
            !on_matches(&on, &push("refs/tags/v1", None)),
            "only branches set"
        );
    }

    #[test]
    fn branches_ignore() {
        let on = on_push(PushFilter {
            branches_ignore: pl(&["wip/*"]),
            ..PushFilter::default()
        });
        assert!(!on_matches(&on, &push("refs/heads/wip/x", None)));
        assert!(on_matches(&on, &push("refs/heads/main", None)));
    }

    #[test]
    fn tag_filters_skip_branches_and_ignore_paths() {
        let on = on_push(PushFilter {
            tags: pl(&["v[0-9]+.[0-9]+.[0-9]+"]),
            paths: pl(&["src/**"]),
            ..PushFilter::default()
        });
        assert!(on_matches(
            &on,
            &push("refs/tags/v1.2.3", Some(&["README.md"]))
        ));
        assert!(!on_matches(&on, &push("refs/tags/v1.2", None)));
        assert!(!on_matches(
            &on,
            &push("refs/heads/main", Some(&["src/a.rs"]))
        ));
    }

    #[test]
    fn path_filters() {
        let on = on_push(PushFilter {
            paths: pl(&["src/**", "!src/**/*.md"]),
            ..PushFilter::default()
        });
        assert!(on_matches(&on, &push("refs/heads/m", Some(&["src/a.rs"]))));
        assert!(on_matches(
            &on,
            &push("refs/heads/m", Some(&["docs/x", "src/a.rs"]))
        ));
        assert!(!on_matches(
            &on,
            &push("refs/heads/m", Some(&["src/doc/x.md"]))
        ));
        assert!(!on_matches(&on, &push("refs/heads/m", Some(&["docs/x"]))));
        assert!(!on_matches(&on, &push("refs/heads/m", Some(&[]))));
        assert!(
            on_matches(&on, &push("refs/heads/m", None)),
            "unknown paths pass"
        );
    }

    #[test]
    fn paths_ignore() {
        let on = on_push(PushFilter {
            paths_ignore: pl(&["docs/**", "*.md"]),
            ..PushFilter::default()
        });
        assert!(!on_matches(
            &on,
            &push("refs/heads/m", Some(&["docs/a", "README.md"]))
        ));
        assert!(on_matches(
            &on,
            &push("refs/heads/m", Some(&["docs/a", "src/x"]))
        ));
        assert!(on_matches(&on, &push("refs/heads/m", None)));
    }

    #[test]
    fn too_many_paths_count_as_unknown() {
        let on = on_push(PushFilter {
            paths: pl(&["src/**"]),
            ..PushFilter::default()
        });
        let at_limit: Vec<String> = (0..MAX_FILTER_PATHS).map(|i| format!("docs/{i}")).collect();
        let over: Vec<String> = (0..=MAX_FILTER_PATHS)
            .map(|i| format!("docs/{i}"))
            .collect();
        let ev = |p: &[String]| Event::Push {
            ref_name: "refs/heads/m".into(),
            changed_paths: Some(p.to_vec()),
        };
        assert!(!on_matches(&on, &ev(&at_limit)), "at the limit: filtered");
        assert!(
            on_matches(&on, &ev(&over)),
            "over the limit: unknown, passes"
        );
        let ignore = on_push(PushFilter {
            paths_ignore: pl(&["docs/**"]),
            ..PushFilter::default()
        });
        assert!(!on_matches(&ignore, &ev(&at_limit)));
        assert!(on_matches(&ignore, &ev(&over)));
    }

    #[test]
    fn other_refs() {
        let on = on_push(PushFilter::default());
        assert!(on_matches(&on, &push("refs/notes/x", None)));
        let on = on_push(PushFilter {
            branches: pl(&["*"]),
            ..PushFilter::default()
        });
        assert!(!on_matches(&on, &push("refs/notes/x", None)));
    }

    #[test]
    fn pull_request() {
        let on = On {
            pull_request: Some(PullRequestFilter {
                branches: pl(&["main"]),
                paths_ignore: pl(&["docs/**"]),
                ..PullRequestFilter::default()
            }),
            ..On::default()
        };
        let pr = |b: &str, p: Option<Vec<String>>| Event::PullRequest {
            base_branch: b.into(),
            changed_paths: p,
        };
        assert!(on_matches(&on, &pr("main", None)));
        assert!(on_matches(
            &on,
            &pr("refs/heads/main", Some(vec!["src/x".into()]))
        ));
        assert!(!on_matches(&on, &pr("main", Some(vec!["docs/x".into()]))));
        assert!(!on_matches(&on, &pr("dev", None)));
        assert!(!on_matches(&on, &push("refs/heads/main", None)));
    }

    #[test]
    fn schedule_and_manual() {
        let on = On {
            schedule: vec![CronSchedule::parse("0 3 * * *").expect("valid")],
            manual: true,
            ..On::default()
        };
        assert!(on_matches(&on, &Event::Schedule));
        assert!(on_matches(&on, &Event::Manual));
        assert!(!on_matches(&on, &push("refs/heads/main", None)));
    }
}
