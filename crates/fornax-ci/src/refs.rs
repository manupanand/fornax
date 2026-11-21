// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! Ref updates of a push.

use std::collections::BTreeMap;

/// One ref changed by a push.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefUpdate {
    /// Full ref name, e.g. `refs/heads/main` or `refs/tags/v1`.
    pub ref_name: String,
    /// Object before the push; `None` when the ref was created.
    pub old: Option<String>,
    /// Object after the push; `None` when the ref was deleted.
    pub new: Option<String>,
}

impl RefUpdate {
    /// The ref did not exist before the push.
    pub fn is_create(&self) -> bool {
        self.old.is_none() && self.new.is_some()
    }

    /// The ref no longer exists after the push.
    pub fn is_delete(&self) -> bool {
        self.new.is_none()
    }

    /// Branch name without `refs/heads/`, if this is a branch.
    pub fn branch(&self) -> Option<&str> {
        self.ref_name.strip_prefix("refs/heads/")
    }

    /// Tag name without `refs/tags/`, if this is a tag.
    pub fn tag(&self) -> Option<&str> {
        self.ref_name.strip_prefix("refs/tags/")
    }
}

/// The refs that differ between two snapshots (full ref name → object ID), sorted by ref
/// name: created (only in `after`), updated (different object) and deleted (only in
/// `before`). Unchanged refs are left out.
pub fn diff_refs(
    before: &BTreeMap<String, String>,
    after: &BTreeMap<String, String>,
) -> Vec<RefUpdate> {
    let mut updates: Vec<RefUpdate> = after
        .iter()
        .filter(|(name, new)| before.get(*name) != Some(*new))
        .map(|(name, new)| RefUpdate {
            ref_name: name.clone(),
            old: before.get(name).cloned(),
            new: Some(new.clone()),
        })
        .collect();
    updates.extend(
        before
            .iter()
            .filter(|(name, _)| !after.contains_key(*name))
            .map(|(name, old)| RefUpdate {
                ref_name: name.clone(),
                old: Some(old.clone()),
                new: None,
            }),
    );
    updates.sort_by(|a, b| a.ref_name.cmp(&b.ref_name));
    updates
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    fn update(name: &str, old: Option<&str>, new: Option<&str>) -> RefUpdate {
        RefUpdate {
            ref_name: name.to_owned(),
            old: old.map(str::to_owned),
            new: new.map(str::to_owned),
        }
    }

    #[test]
    fn created_updated_deleted_and_unchanged() {
        let before = snap(&[
            ("refs/heads/main", "a1"),
            ("refs/heads/old", "b1"),
            ("refs/tags/v1", "c1"),
        ]);
        let after = snap(&[
            ("refs/heads/main", "a2"),
            ("refs/heads/new", "d1"),
            ("refs/tags/v1", "c1"),
        ]);
        assert_eq!(
            diff_refs(&before, &after),
            vec![
                update("refs/heads/main", Some("a1"), Some("a2")),
                update("refs/heads/new", None, Some("d1")),
                update("refs/heads/old", Some("b1"), None),
            ]
        );
    }

    #[test]
    fn empty_snapshots() {
        let empty = BTreeMap::new();
        assert!(diff_refs(&empty, &empty).is_empty());
        let one = snap(&[("refs/heads/main", "a")]);
        assert_eq!(
            diff_refs(&empty, &one),
            vec![update("refs/heads/main", None, Some("a"))]
        );
        assert_eq!(
            diff_refs(&one, &empty),
            vec![update("refs/heads/main", Some("a"), None)]
        );
        assert!(diff_refs(&one, &one).is_empty());
    }

    #[test]
    fn classification() {
        let created = update("refs/heads/x", None, Some("a"));
        let updated = update("refs/tags/v1", Some("a"), Some("b"));
        let deleted = update("refs/heads/x", Some("a"), None);
        assert!(created.is_create() && !created.is_delete());
        assert!(!updated.is_create() && !updated.is_delete());
        assert!(deleted.is_delete() && !deleted.is_create());
        assert_eq!(created.branch(), Some("x"));
        assert_eq!(updated.tag(), Some("v1"));
        assert_eq!(updated.branch(), None);
    }
}
