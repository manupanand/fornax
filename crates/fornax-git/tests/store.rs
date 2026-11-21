// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! RepoStore against real bare repositories created with the system `git`.

use std::path::Path;
use std::process::Command;

use fornax_git::{EntryKind, GitError, RepoStore};

/// Run git with an isolated config so the developer's settings can't affect results.
fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?} failed");
}

#[tokio::test]
async fn browse_a_pushed_repository() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let store = RepoStore::new(tmp.path().join("repos"));
    let bare = store.create("Alice", "Demo", "main").await.expect("create");
    assert!(bare.ends_with("alice/demo.git"));
    assert!(matches!(
        store.create("alice", "demo", "main").await,
        Err(GitError::InvalidInput(_))
    ));

    // Empty repository: no branches, unknown revision.
    assert!(store.branches(&bare).await.expect("branches").is_empty());
    assert!(matches!(
        store.commits(&bare, "main", 10).await,
        Err(GitError::NotFound(_))
    ));

    let work = tmp.path().join("work");
    std::fs::create_dir_all(work.join("src")).expect("mkdir");
    std::fs::write(work.join("README.md"), "hello\n").expect("write");
    std::fs::write(work.join("src/main.rs"), "fn main() {}\n").expect("write");
    git(&work, &["init", "-q"]);
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "first commit"]);
    git(&work, &["checkout", "-q", "-b", "feature/x"]);
    std::fs::write(work.join("README.md"), "hello feature\n").expect("write");
    git(&work, &["commit", "-q", "-am", "second commit"]);
    git(
        &work,
        &[
            "push",
            "-q",
            bare.to_str().expect("utf8"),
            "main",
            "feature/x",
        ],
    );

    let branches = store.branches(&bare).await.expect("branches");
    let names: Vec<_> = branches.iter().map(|b| b.name.as_str()).collect();
    assert_eq!(names, ["feature/x", "main"]);

    let log = store.commits(&bare, "feature/x", 10).await.expect("log");
    assert_eq!(
        log.iter().map(|c| c.summary.as_str()).collect::<Vec<_>>(),
        ["second commit", "first commit"]
    );
    assert_eq!(log[0].author_email, "test@example.com");
    assert_eq!(
        store
            .commits(&bare, "feature/x", 1)
            .await
            .expect("log")
            .len(),
        1
    );

    let root = store.tree(&bare, "main", "").await.expect("tree");
    let listing: Vec<_> = root.iter().map(|e| (e.name.as_str(), e.kind)).collect();
    assert_eq!(
        listing,
        [("src", EntryKind::Tree), ("README.md", EntryKind::Blob)]
    );
    assert_eq!(
        store.tree(&bare, "main", "/src/").await.expect("tree")[0].name,
        "main.rs"
    );

    assert_eq!(
        store
            .blob(&bare, "main", "README.md", 1024)
            .await
            .expect("blob"),
        b"hello\n"
    );
    assert_eq!(
        store
            .blob(&bare, "feature/x", "README.md", 1024)
            .await
            .expect("blob"),
        b"hello feature\n"
    );
    assert!(matches!(
        store.blob(&bare, "main", "README.md", 2).await,
        Err(GitError::InvalidInput(_))
    ));
    assert!(matches!(
        store.blob(&bare, "main", "src", 1024).await,
        Err(GitError::InvalidInput(_))
    ));
    assert!(matches!(
        store.blob(&bare, "main", "missing", 1024).await,
        Err(GitError::NotFound(_))
    ));
    assert!(matches!(
        store.tree(&bare, "main", "../x").await,
        Err(GitError::InvalidInput(_))
    ));

    // Option injection is impossible: the revision is never parsed as an option.
    assert!(matches!(
        store.commits(&bare, "--output=/tmp/pwned", 1).await,
        Err(GitError::NotFound(_))
    ));
    assert!(matches!(
        store.commits(&bare, "main bad", 1).await,
        Err(GitError::InvalidInput(_))
    ));

    // Default branch and rename.
    assert_eq!(
        store.head(&bare).await.expect("head").as_deref(),
        Some("main")
    );
    store.set_head(&bare, "feature/x").await.expect("set head");
    assert_eq!(
        store.head(&bare).await.expect("head").as_deref(),
        Some("feature/x")
    );
    assert!(matches!(
        store.set_head(&bare, "-bad").await,
        Err(GitError::InvalidInput(_))
    ));
    let moved = store
        .rename("alice", "demo", "Renamed")
        .await
        .expect("rename");
    assert!(moved.ends_with("alice/renamed.git") && !bare.exists());
    assert_eq!(store.branches(&moved).await.expect("branches").len(), 2);

    store.remove("alice", "renamed").await.expect("remove");
    assert!(!moved.exists());
}
