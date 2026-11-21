// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! A real `git push` over HTTP with a `.fornax/` pipeline creates CI pipelines (C-003 wiring).

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use common::{Harness, git, local_repo};
use fornax_api::ApiConfig;
use serde_json::{Value, json};

const CI: &[u8] = b"kind: Pipeline\nversion: 1\non:\n  push:\n    branches: [main]\njobs:\n  build:\n    runs-on: [linux]\n    steps:\n      - run: echo build\n  test:\n    needs: [build]\n    steps:\n      - run: echo test\n";

/// Poll the API until `repo` has exactly `n` pipelines (CI runs after git exits).
async fn pipelines(h: &Harness, token: &str, repo: &str, n: usize) -> Value {
    let mut last = Value::Null;
    for _ in 0..100 {
        let (_, list) = h
            .call(
                "GET",
                &format!("/api/v1/repos/{repo}/pipelines"),
                Some(token),
                None,
            )
            .await;
        if list.as_array().map(Vec::len) == Some(n) {
            return list;
        }
        last = list;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("expected {n} pipelines in {repo}, got {last}");
}

#[tokio::test(flavor = "multi_thread")]
async fn push_creates_pipeline_and_wakes_runners() {
    let wakes = Arc::new(AtomicUsize::new(0));
    let counter = wakes.clone();
    let wakeup = fornax_ci::Wakeup::new(move || {
        counter.fetch_add(1, Ordering::SeqCst);
    });
    let h = Harness::start_full(None, ApiConfig::default(), wakeup).await;
    let (_, alice) = h.user("alice").await;
    h.call(
        "POST",
        "/api/v1/user/repos",
        Some(&alice),
        Some(json!({ "name": "app" })),
    )
    .await;
    let url = format!("{}/alice/app.git", h.authed("alice", &alice));

    let src = local_repo(
        &h.work,
        "src",
        &[(".fornax/ci.yaml", CI), ("README.md", b"x\n")],
    );
    assert!(git(&src, &["push", "-q", &url, "main"]));
    let list = pipelines(&h, &alice, "alice/app", 1).await;
    assert_eq!(list[0]["git_ref"], "refs/heads/main");
    assert_eq!(list[0]["event"], "push");
    let (_, detail) = h
        .call(
            "GET",
            "/api/v1/repos/alice/app/pipelines/1",
            Some(&alice),
            None,
        )
        .await;
    let statuses: Vec<_> = detail["jobs"]
        .as_array()
        .expect("jobs")
        .iter()
        .map(|j| j["status"].as_str().unwrap_or("").to_owned())
        .collect();
    assert_eq!(statuses, ["queued", "waiting"]);
    assert!(wakes.load(Ordering::SeqCst) >= 1, "runners were woken");

    // Excluded branch → nothing; repository without .fornax → nothing.
    assert!(git(&src, &["push", "-q", &url, "main:feature/x"]));
    h.call(
        "POST",
        "/api/v1/user/repos",
        Some(&alice),
        Some(json!({ "name": "plain" })),
    )
    .await;
    let plain = local_repo(&h.work, "plain", &[("README.md", b"x\n")]);
    assert!(git(
        &plain,
        &[
            "push",
            "-q",
            &format!("{}/alice/plain.git", h.authed("alice", &alice)),
            "main"
        ]
    ));
    tokio::time::sleep(Duration::from_millis(400)).await;
    pipelines(&h, &alice, "alice/app", 1).await;
    pipelines(&h, &alice, "alice/plain", 0).await;

    // Another push to main → pipeline #2.
    std::fs::write(src.join("README.md"), "y\n").expect("write");
    assert!(git(&src, &["commit", "-qam", "second"]));
    assert!(git(&src, &["push", "-q", &url, "main"]));
    let list = pipelines(&h, &alice, "alice/app", 2).await;
    assert_eq!(list[0]["number"], 2);
}
