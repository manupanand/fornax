// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! The `fornax` binary: admin commands and backup → restore round trip (H-015).

use std::path::Path;
use std::process::{Command, Output};

fn fornax(data: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_fornax"))
        .arg("--data-dir")
        .arg(data)
        .args(args)
        .env("RUST_LOG", "warn")
        .output()
        .expect("run fornax")
}

fn ok(out: &Output) -> String {
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn git(dir: &Path, args: &[&str]) {
    let st = Command::new("git")
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args([
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@e",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .status()
        .expect("git");
    assert!(st.success(), "git {args:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn backup_and_restore_round_trip() {
    let tmp = tempfile::tempdir().expect("tmp");
    let (data, backup, restored) = (
        tmp.path().join("data"),
        tmp.path().join("backup"),
        tmp.path().join("restored"),
    );

    let out = ok(&fornax(
        &data,
        &["admin", "create-user", "alice", "--admin"],
    ));
    assert!(out.contains("fnx_"));
    assert!(
        !fornax(&data, &["admin", "create-user", "ALICE"])
            .status
            .success(),
        "duplicate"
    );
    assert!(
        !fornax(&data, &["admin", "create-user", "api"])
            .status
            .success(),
        "reserved"
    );
    assert!(
        ok(&fornax(
            &data,
            &["admin", "create-token", "alice", "--name", "ci"]
        ))
        .contains("fnx_")
    );
    assert!(
        !fornax(&data, &["admin", "create-token", "nobody"])
            .status
            .success()
    );

    // A repository with content, created through the library like the API does.
    {
        let db = fornax_db::Db::open(&data.join("fornax.db"))
            .await
            .expect("db");
        let alice = db.user_by_name("alice").await.expect("q").expect("user");
        db.create_repo(&fornax_db::NewRepo {
            owner_id: alice.id,
            name: "keep",
            description: "",
            is_private: true,
            default_branch: "main",
        })
        .await
        .expect("repo");
        let store = fornax_git::RepoStore::new(data.join("repos"));
        let bare = store.create("alice", "keep", "main").await.expect("disk");
        let work = tmp.path().join("w");
        std::fs::create_dir_all(&work).expect("mkdir");
        std::fs::write(work.join("f.txt"), "precious\n").expect("write");
        git(&work, &["init", "-q"]);
        git(&work, &["add", "."]);
        git(&work, &["commit", "-q", "-m", "keep me"]);
        git(&work, &["push", "-q", bare.to_str().expect("utf8"), "main"]);
        db.close().await;
    }
    std::fs::write(data.join("ssh_host_ed25519_key"), "not-a-real-key").expect("fake key");

    assert!(
        ok(&fornax(
            &data,
            &["admin", "backup", "--out", backup.to_str().expect("utf8")]
        ))
        .contains("1 repositories")
    );
    assert!(backup.join("manifest.json").exists() && backup.join("fornax.db").exists());
    assert!(
        !fornax(
            &data,
            &["admin", "backup", "--out", backup.to_str().expect("utf8")]
        )
        .status
        .success(),
        "no overwrite"
    );

    assert!(
        ok(&fornax(
            &restored,
            &["admin", "restore", "--from", backup.to_str().expect("utf8")]
        ))
        .contains("restored 1")
    );
    assert!(
        !fornax(
            &restored,
            &["admin", "restore", "--from", backup.to_str().expect("utf8")]
        )
        .status
        .success(),
        "target not empty"
    );
    assert_eq!(
        std::fs::read_to_string(restored.join("ssh_host_ed25519_key")).expect("key"),
        "not-a-real-key"
    );

    let db = fornax_db::Db::open(&restored.join("fornax.db"))
        .await
        .expect("db");
    let repo = db
        .repo_by_name("alice", "keep")
        .await
        .expect("q")
        .expect("repo");
    assert!(repo.is_private);
    let store = fornax_git::RepoStore::new(restored.join("repos"));
    let blob = store
        .blob(&store.repo_path("alice", "keep"), "main", "f.txt", 1024)
        .await
        .expect("blob");
    assert_eq!(blob, b"precious\n");
    let audit = db.recent_audit(20).await.expect("audit");
    assert!(audit.iter().any(|e| e.action == "admin.restore"));
}
