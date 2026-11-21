// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! `git clone`/`push` over the built-in SSH server with the system `ssh` client (H-014).

use std::path::Path;
use std::process::{Command, Output};

use fornax_core::perm::Permission;
use fornax_db::{Db, NewRepo};
use fornax_git::RepoStore;
use fornax_ssh::{SshState, load_or_create_host_key, serve};

fn keygen(path: &Path) -> String {
    let ok = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(path)
        .status()
        .expect("ssh-keygen")
        .success();
    assert!(ok);
    std::fs::read_to_string(path.with_extension("pub")).expect("pub key")
}

fn fingerprint(pub_key: &Path) -> String {
    let out = Command::new("ssh-keygen")
        .arg("-lf")
        .arg(pub_key)
        .output()
        .expect("ssh-keygen -lf");
    String::from_utf8(out.stdout)
        .expect("utf8")
        .split_whitespace()
        .nth(1)
        .expect("fp")
        .to_owned()
}

fn git(dir: &Path, key: &Path, args: &[&str]) -> Output {
    let ssh = format!(
        "ssh -i {} -o IdentitiesOnly=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR",
        key.display()
    );
    Command::new("git")
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_SSH_COMMAND", ssh)
        .args([
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@e",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .output()
        .expect("git")
}

#[tokio::test(flavor = "multi_thread")]
async fn clone_and_push_over_ssh() {
    let tmp = tempfile::tempdir().expect("tmp");
    let db = Db::open(&tmp.path().join("fornax.db")).await.expect("db");
    let repos = RepoStore::new(tmp.path().join("repos"));
    let alice = db.create_user("alice", false).await.expect("u");
    let bob = db.create_user("bob", false).await.expect("u");
    for (user, name, private) in [(&alice, "demo", false), (&alice, "secret", true)] {
        db.create_repo(&NewRepo {
            owner_id: user.id,
            name,
            description: "",
            is_private: private,
            default_branch: "main",
        })
        .await
        .expect("repo");
        repos.create("alice", name, "main").await.expect("disk");
    }

    // Keys: alice registered, bob registered, mallory unknown.
    let keys = tmp.path().join("keys");
    std::fs::create_dir_all(&keys).expect("mkdir");
    let (ka, kb, km) = (keys.join("alice"), keys.join("bob"), keys.join("mallory"));
    for (user, key) in [(&alice, &ka), (&bob, &kb)] {
        let public = keygen(key);
        db.add_ssh_key(
            user.id,
            "test",
            &fingerprint(&key.with_extension("pub")),
            public.trim(),
        )
        .await
        .expect("key");
    }
    keygen(&km);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let host_key = load_or_create_host_key(&tmp.path().join("host_key")).expect("host key");
    let state = SshState {
        db: db.clone(),
        repos,
        ci_wakeup: fornax_ci::Wakeup::noop(),
    };
    tokio::spawn(async move { serve(listener, state, host_key).await.expect("ssh") });
    let url = |r: &str| format!("ssh://git@127.0.0.1:{port}/alice/{r}.git");

    let work = tmp.path().join("work");
    std::fs::create_dir_all(work.join("src")).expect("mkdir");
    std::fs::write(work.join("src/README.md"), "over ssh\n").expect("write");
    let src = work.join("src");
    for args in [
        &["init", "-q"][..],
        &["add", "."],
        &["commit", "-q", "-m", "ssh commit"],
    ] {
        assert!(git(&src, &ka, args).status.success());
    }

    let push = git(&src, &ka, &["push", "-q", &url("demo"), "main"]);
    assert!(
        push.status.success(),
        "{}",
        String::from_utf8_lossy(&push.stderr)
    );
    let clone = git(&work, &kb, &["clone", "-q", &url("demo"), "bob-clone"]);
    assert!(
        clone.status.success(),
        "{}",
        String::from_utf8_lossy(&clone.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(work.join("bob-clone/README.md")).expect("read"),
        "over ssh\n"
    );

    let denied = git(&src, &kb, &["push", "-q", &url("demo"), "main:other"]);
    assert!(!denied.status.success());
    assert!(String::from_utf8_lossy(&denied.stderr).contains("permission denied"));
    let hidden = git(&work, &kb, &["clone", "-q", &url("secret"), "x"]);
    assert!(String::from_utf8_lossy(&hidden.stderr).contains("repository not found"));
    assert!(
        !git(&work, &km, &["clone", "-q", &url("demo"), "y"])
            .status
            .success(),
        "unknown key rejected"
    );

    // Grants apply over SSH too.
    let secret = db
        .repo_by_name("alice", "secret")
        .await
        .expect("q")
        .expect("repo");
    db.set_collaborator(secret.id, bob.id, Permission::Read)
        .await
        .expect("collab");
    assert!(
        git(&work, &kb, &["clone", "-q", &url("secret"), "z"])
            .status
            .success()
    );

    // Interactive shells and other commands are refused.
    let shell = Command::new("ssh")
        .args([
            "-i",
            kb.to_str().expect("utf8"),
            "-o",
            "IdentitiesOnly=yes",
            "-o",
            "StrictHostKeyChecking=no",
            "-o",
            "UserKnownHostsFile=/dev/null",
            "-o",
            "LogLevel=ERROR",
            "-p",
            &port.to_string(),
            "git@127.0.0.1",
            "cat /etc/passwd",
        ])
        .output()
        .expect("ssh");
    assert!(!shell.status.success());
    assert!(shell.stdout.is_empty());

    let audit = db.recent_audit(10).await.expect("audit");
    assert!(
        audit
            .iter()
            .any(|e| e.action == "repo.push" && e.detail.contains("ssh"))
    );
}
