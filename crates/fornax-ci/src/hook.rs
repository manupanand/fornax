// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! Glue for the push transports (HTTP and SSH): snapshot refs before `receive-pack`, then
//! turn the difference into pipelines and wake waiting runners.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use fornax_db::{Db, Repo};
use fornax_git::RepoStore;

use crate::{CiError, PushOutcome, diff_refs, on_push};

/// Callback that wakes runners waiting for work (the runner server's notifier). Cheap to
/// clone; [`Wakeup::noop`] when no runner server is attached.
#[derive(Clone)]
pub struct Wakeup(Arc<dyn Fn() + Send + Sync>);

impl Wakeup {
    /// Wrap a wake-up function.
    pub fn new(f: impl Fn() + Send + Sync + 'static) -> Self {
        Self(Arc::new(f))
    }

    /// A wake-up that does nothing.
    pub fn noop() -> Self {
        Self::new(|| {})
    }

    /// Wake waiting runners.
    pub fn wake(&self) {
        (self.0)();
    }
}

impl Default for Wakeup {
    fn default() -> Self {
        Self::noop()
    }
}

impl fmt::Debug for Wakeup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Wakeup")
    }
}

/// Branch and tag heads of `repo` before a push; pass the result to [`after_push`].
pub async fn before_push(
    repos: &RepoStore,
    repo: &Repo,
) -> Result<BTreeMap<String, String>, CiError> {
    let (owner, name) = repo.storage_key();
    Ok(repos.snapshot_refs(&repos.repo_path(&owner, &name)).await?)
}

/// After a successful `receive-pack`: diff refs against `before`, create pipelines for the
/// updated refs ([`on_push`]) and wake runners if anything was queued.
///
/// Concurrent pushes to the same repository may attribute ref updates to the "wrong" push;
/// each update is still processed (by whichever push observes it first). A server-side
/// `post-receive` hook would make this exact; tracked as a follow-up.
pub async fn after_push(
    db: &Db,
    repos: &RepoStore,
    repo: &Repo,
    pusher: Option<i64>,
    before: &BTreeMap<String, String>,
    wakeup: &Wakeup,
    now: i64,
) -> Result<PushOutcome, CiError> {
    let after = before_push(repos, repo).await?;
    let updates = diff_refs(before, &after);
    if updates.is_empty() {
        return Ok(PushOutcome::default());
    }
    let outcome = on_push(db, repos, repo, pusher, &updates, now).await?;
    if !outcome.created.is_empty() {
        wakeup.wake();
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[test]
    fn wakeup_calls_through() {
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        let w = Wakeup::new(move || {
            h.fetch_add(1, Ordering::SeqCst);
        });
        w.clone().wake();
        w.wake();
        Wakeup::noop().wake();
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        assert_eq!(format!("{w:?}"), "Wakeup");
    }
}
