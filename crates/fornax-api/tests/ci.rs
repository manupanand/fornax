// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! CI read API: pipelines, jobs, logs, cancel — and their permissions.

mod common;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use common::Harness;
use fornax_db::{NewJob, NewPipeline, Outcome};
use serde_json::json;
use tower::ServiceExt;

fn job(name: &str, needs: &[&str]) -> NewJob {
    NewJob {
        name: name.into(),
        labels: vec!["linux".into()],
        payload: json!({ "v": 1, "steps": [{ "run": "echo secret-steps" }] }).to_string(),
        needs: needs.iter().map(|s| (*s).to_owned()).collect(),
        max_attempts: 1,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn pipelines_jobs_logs_and_cancel() {
    let h = Harness::start().await;
    let (alice, alice_t) = h.user("alice").await;
    let (_, bob_t) = h.user("bob").await;
    h.call(
        "POST",
        "/api/v1/user/repos",
        Some(&alice_t),
        Some(json!({ "name": "app", "private": true })),
    )
    .await;
    let repo = h
        .state
        .db
        .repo_by_name("alice", "app")
        .await
        .expect("q")
        .expect("repo");
    let db = &h.state.db;
    let now = fornax_core::now_unix();
    let new = |jobs| NewPipeline {
        repo_id: repo.id,
        file: ".fornax/ci.yaml".into(),
        name: "ci".into(),
        git_ref: "refs/heads/main".into(),
        sha: "a".repeat(40),
        event: "push".into(),
        trusted: true,
        triggered_by: Some(alice.id),
        jobs,
    };
    let p1 = db
        .create_pipeline(&new(vec![job("build", &[]), job("test", &["build"])]), now)
        .await
        .expect("p1");
    let p2 = db
        .create_pipeline(&new(vec![job("lint", &[])]), now)
        .await
        .expect("p2");
    assert_eq!((p1.number, p2.number), (1, 2));

    // Run "build" through the queue with a log.
    let runner = db
        .register_runner("r1", "hash-r1", &["linux".into()], true, now)
        .await
        .expect("runner");
    let claimed = db
        .claim_job(&runner, 60, now)
        .await
        .expect("claim")
        .expect("job");
    assert_eq!(claimed.job.name, "build");
    db.append_log(claimed.attempt.id, 0, b"$ build\nhello\n")
        .await
        .expect("log");
    let outcome = Outcome {
        success: true,
        exit_code: Some(0),
        error: None,
    };
    db.finish_attempt(claimed.attempt.id, runner.id, &outcome, now)
        .await
        .expect("finish");

    // Listing and detail.
    let (status, list) = h
        .call(
            "GET",
            "/api/v1/repos/alice/app/pipelines",
            Some(&alice_t),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        list.as_array()
            .map(|a| a.iter().map(|p| p["number"].as_i64()).collect::<Vec<_>>()),
        Some(vec![Some(2), Some(1)])
    );
    let (_, detail) = h
        .call(
            "GET",
            "/api/v1/repos/alice/app/pipelines/1",
            Some(&alice_t),
            None,
        )
        .await;
    let jobs = detail["jobs"].as_array().expect("jobs");
    assert_eq!(jobs[0]["name"], "build");
    assert_eq!(jobs[0]["status"], "success");
    assert_eq!(
        jobs[1]["status"], "queued",
        "dependent promoted after build succeeded"
    );
    assert_eq!(jobs[0]["attempts"].as_array().map(Vec::len), Some(1));
    assert!(
        !detail.to_string().contains("secret-steps"),
        "payload is not exposed"
    );
    assert_eq!(detail["needs"].as_array().map(Vec::len), Some(1));

    // Log, incremental.
    let build_id = jobs[0]["id"].as_i64().expect("id");
    let uri = format!("/api/v1/repos/alice/app/pipelines/1/jobs/{build_id}/log");
    let res = h
        .app
        .clone()
        .oneshot(
            Request::get(&uri)
                .header("authorization", format!("Bearer {alice_t}"))
                .body(Body::empty())
                .expect("req"),
        )
        .await
        .expect("res");
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()["x-fornax-next-offset"], "14");
    assert_eq!(
        &to_bytes(res.into_body(), usize::MAX).await.expect("body")[..],
        b"$ build\nhello\n"
    );
    let res = h
        .app
        .clone()
        .oneshot(
            Request::get(format!("{uri}?from=8"))
                .header("authorization", format!("Bearer {alice_t}"))
                .body(Body::empty())
                .expect("req"),
        )
        .await
        .expect("res");
    assert_eq!(
        &to_bytes(res.into_body(), usize::MAX).await.expect("body")[..],
        b"hello\n"
    );

    // Permissions: private repo hidden from others; job of another pipeline is 404.
    assert_eq!(
        h.status(
            "GET",
            "/api/v1/repos/alice/app/pipelines",
            Some(&bob_t),
            None
        )
        .await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        h.status("GET", "/api/v1/repos/alice/app/pipelines", None, None)
            .await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        h.status(
            "GET",
            &format!("/api/v1/repos/alice/app/pipelines/2/jobs/{build_id}/log"),
            Some(&alice_t),
            None
        )
        .await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        h.status(
            "GET",
            "/api/v1/repos/alice/app/pipelines/99",
            Some(&alice_t),
            None
        )
        .await,
        StatusCode::NOT_FOUND
    );

    // Cancel without a JSON body is refused (no simple cross-site POST).
    assert_eq!(
        h.status(
            "POST",
            "/api/v1/repos/alice/app/pipelines/2/cancel",
            Some(&alice_t),
            None
        )
        .await,
        StatusCode::UNSUPPORTED_MEDIA_TYPE
    );
    // Cancel needs write.
    h.call(
        "PUT",
        "/api/v1/repos/alice/app/collaborators/bob",
        Some(&alice_t),
        Some(json!({ "permission": "read" })),
    )
    .await;
    assert_eq!(
        h.status(
            "POST",
            "/api/v1/repos/alice/app/pipelines/2/cancel",
            Some(&bob_t),
            Some(json!({}))
        )
        .await,
        StatusCode::FORBIDDEN
    );
    let (status, body) = h
        .call(
            "POST",
            "/api/v1/repos/alice/app/pipelines/2/cancel",
            Some(&alice_t),
            Some(json!({})),
        )
        .await;
    assert_eq!(
        (status, body["canceled_jobs"].as_u64()),
        (StatusCode::ACCEPTED, Some(1))
    );
    let (_, p2) = h
        .call(
            "GET",
            "/api/v1/repos/alice/app/pipelines/2",
            Some(&bob_t),
            None,
        )
        .await;
    assert_eq!(p2["status"], "canceled");
}
