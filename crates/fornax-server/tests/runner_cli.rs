// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! `fornax admin create-runner` and `fornax agent`.

use std::path::Path;
use std::process::{Command, Output};

use fornax_db::Db;

fn fornax(data: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_fornax"))
        .arg("--data-dir")
        .arg(data)
        .args(args)
        .env("RUST_LOG", "warn")
        .env_remove("FORNAX_RUNNER_TOKEN")
        .output()
        .expect("run fornax")
}

fn stdout(out: &Output) -> String {
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn printed_token(out: &str) -> String {
    out.lines()
        .find_map(|l| l.strip_prefix("runner token (shown once): "))
        .expect("token line")
        .trim()
        .to_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn create_runner_prints_token_and_defaults_to_trusted_only() {
    let tmp = tempfile::tempdir().expect("tmp");
    let data = tmp.path().join("data");

    let out = stdout(&fornax(
        &data,
        &[
            "admin",
            "create-runner",
            "host-1",
            "--labels",
            "linux,x86_64",
        ],
    ));
    let t1 = printed_token(&out);
    assert!(fornax_proto_format(&t1), "{t1}");
    assert!(out.contains("trusted only: true"), "{out}");

    let out = stdout(&fornax(
        &data,
        &["admin", "create-runner", "docker-1", "--allow-untrusted"],
    ));
    let t2 = printed_token(&out);
    assert_ne!(t1, t2);
    assert!(out.contains("trusted only: false"), "{out}");

    let bad = fornax(&data, &["admin", "create-runner", "bad name"]);
    assert!(!bad.status.success());
    let bad = fornax(&data, &["admin", "create-runner", "x", "--labels", "$(id)"]);
    assert!(!bad.status.success());

    let db = Db::open(&data.join("fornax.db")).await.expect("db");
    let runners = db.list_runners().await.expect("runners");
    assert_eq!(runners.len(), 2);
    assert_eq!(runners[0].name, "host-1");
    assert_eq!(runners[0].labels, vec!["linux", "x86_64"]);
    assert!(runners[0].trusted_only);
    assert!(!runners[1].trusted_only);
    // Only the hash is stored.
    let stored = db
        .runner_by_token_hash(&fornax_runner::hash_token(&t1))
        .await
        .expect("lookup")
        .expect("found");
    assert_eq!(stored.id, runners[0].id);
}

fn fornax_proto_format(t: &str) -> bool {
    t.strip_prefix("fnr_")
        .is_some_and(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()))
}

fn fornax_env(data: &Path, args: &[&str], token: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_fornax"))
        .arg("--data-dir")
        .arg(data)
        .args(args)
        .env("RUST_LOG", "warn")
        .env("FORNAX_RUNNER_TOKEN", token)
        .output()
        .expect("run fornax")
}

#[test]
fn agent_help_and_config_errors() {
    let tmp = tempfile::tempdir().expect("tmp");
    let out = stdout(&fornax(tmp.path(), &["agent", "--help"]));
    for flag in [
        "--server",
        "--token-file",
        "--work-dir",
        "--capacity",
        "FORNAX_RUNNER_TOKEN",
    ] {
        assert!(out.contains(flag), "{flag} missing in:\n{out}");
    }
    // No --token flag: tokens on the command line are visible in `ps`.
    let out = fornax(
        tmp.path(),
        &[
            "agent",
            "--server",
            "http://127.0.0.1:1",
            "--token",
            "fnr_x",
            "--work-dir",
            "w",
        ],
    );
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("--token"));

    // Missing token.
    let out = fornax(
        tmp.path(),
        &["agent", "--server", "http://127.0.0.1:1", "--work-dir", "w"],
    );
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("FORNAX_RUNNER_TOKEN"));

    // Malformed token (from the environment) is rejected before connecting.
    let args = ["agent", "--server", "http://127.0.0.1:1", "--work-dir", "w"];
    let out = fornax_env(tmp.path(), &args, "nope");
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("runner_token"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Token from a file works the same way; capacity must be ≥ 1.
    let file = tmp.path().join("token");
    std::fs::write(&file, format!("fnr_{}\n", "a".repeat(64))).expect("write");
    let file = file.to_str().expect("utf8");
    let out = fornax(
        tmp.path(),
        &[
            "agent",
            "--server",
            "http://127.0.0.1:1",
            "--token-file",
            file,
            "--work-dir",
            "w",
            "--capacity",
            "0",
        ],
    );
    assert!(!out.status.success());
    let out = fornax(
        tmp.path(),
        &[
            "agent",
            "--server",
            "http://127.0.0.1:1",
            "--token-file",
            "/nonexistent",
            "--work-dir",
            "w",
        ],
    );
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("reading runner token"));
}
