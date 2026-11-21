// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! CI job tokens (`fornax-job:fnj_…`, D-018): read-only git fetch of exactly the job's
//! repository while the attempt runs; nothing else.

mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::extract::FromRequestParts;
use axum::http::{Request, StatusCode, header};
use base64::Engine;
use common::Harness;
use fornax_api::AppState;
use fornax_api::auth::{JOB_TOKEN_USER, MaybeUser, authorize_repo, hash_token, random_hex};
use fornax_api::error::ApiError;
use fornax_core::now_unix;
use fornax_core::perm::Permission;
use fornax_db::{NewJob, NewPipeline, Outcome};
use serde_json::json;
use tower::ServiceExt;

struct Setup {
    h: Harness,
    token: String,
    attempt_id: i64,
    runner_id: i64,
}

/// alice owns private `demo` and `other` and public `open`; a job of a `demo` pipeline is
/// running and holds `token`.
async fn setup() -> Setup {
    let h = Harness::start().await;
    let (alice, alice_token) = h.user("alice").await;
    for (name, private) in [("demo", true), ("other", true), ("open", false)] {
        assert_eq!(
            h.status(
                "POST",
                "/api/v1/user/repos",
                Some(&alice_token),
                Some(json!({ "name": name, "private": private })),
            )
            .await,
            StatusCode::CREATED
        );
    }
    let db = &h.state.db;
    let repo = db
        .repo_by_name("alice", "demo")
        .await
        .expect("db")
        .expect("repo");
    let now = now_unix();
    db.create_pipeline(
        &NewPipeline {
            repo_id: repo.id,
            file: ".fornax/ci.yaml".into(),
            name: "ci".into(),
            git_ref: "refs/heads/main".into(),
            sha: "0".repeat(40),
            event: "push".into(),
            trusted: true,
            triggered_by: Some(alice.id),
            jobs: vec![NewJob {
                name: "build".into(),
                labels: vec![],
                payload: "{}".into(),
                needs: vec![],
                max_attempts: 1,
            }],
        },
        now,
    )
    .await
    .expect("pipeline");
    let runner = db
        .register_runner("r", &hash_token("fnr_x"), &[], true, now)
        .await
        .expect("runner");
    let claimed = db
        .claim_job(&runner, 600, now)
        .await
        .expect("claim")
        .expect("job");
    let token = format!("fnj_{}", random_hex(32).expect("random"));
    db.create_job_token(claimed.attempt.id, repo.id, &hash_token(&token), now + 3600)
        .await
        .expect("job token");
    Setup {
        h,
        token,
        attempt_id: claimed.attempt.id,
        runner_id: runner.id,
    }
}

fn basic(user: &str, token: &str) -> String {
    let raw = format!("{user}:{token}");
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(raw)
    )
}

async fn extract(
    state: &Arc<AppState>,
    method: &str,
    uri: &str,
    auth: &str,
) -> Result<MaybeUser, ApiError> {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, auth)
        .body(())
        .expect("request");
    let (mut parts, ()) = req.into_parts();
    MaybeUser::from_request_parts(&mut parts, state).await
}

async fn http(h: &Harness, method: &str, uri: &str, auth: &str) -> StatusCode {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, auth)
        .body(Body::empty())
        .expect("request");
    h.app.clone().oneshot(req).await.expect("response").status()
}

const FETCH_REFS: &str = "/alice/demo.git/info/refs?service=git-upload-pack";

#[tokio::test(flavor = "multi_thread")]
async fn job_token_reads_its_repository_only() {
    let s = setup().await;
    let state = &s.h.state;
    let auth = basic(JOB_TOKEN_USER, &s.token);

    let m = extract(state, "GET", FETCH_REFS, &auth)
        .await
        .expect("job token accepted for info/refs upload-pack");
    let job = *m.job().expect("job principal");
    assert_eq!(job.attempt_id, s.attempt_id);
    assert!(m.as_ref().is_none(), "a job is never a user");

    let (repo, perm) = authorize_repo(state, "alice", "demo", &m, Permission::Read)
        .await
        .expect("read own repo");
    assert_eq!(repo.name, "demo");
    assert_eq!(perm, Permission::Read);
    assert!(matches!(
        authorize_repo(state, "alice", "demo", &m, Permission::Write).await,
        Err(ApiError::Forbidden)
    ));
    for other in ["other", "open", "missing"] {
        assert!(
            matches!(
                authorize_repo(state, "alice", other, &m, Permission::Read).await,
                Err(ApiError::NotFound)
            ),
            "{other}"
        );
    }

    let m = extract(state, "POST", "/alice/demo.git/git-upload-pack", &auth)
        .await
        .expect("job token accepted for git-upload-pack");
    assert!(m.job().is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn job_token_rejected_outside_git_fetch() {
    let s = setup().await;
    let h = &s.h;
    let auth = basic(JOB_TOKEN_USER, &s.token);

    // Push is never allowed.
    assert_eq!(
        http(
            h,
            "GET",
            "/alice/demo.git/info/refs?service=git-receive-pack",
            &auth
        )
        .await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        http(h, "POST", "/alice/demo.git/git-receive-pack", &auth).await,
        StatusCode::UNAUTHORIZED
    );
    // No REST API access.
    for uri in [
        "/api/v1/user",
        "/api/v1/repos/alice/demo",
        "/api/v1/repos/alice/demo/branches",
    ] {
        assert_eq!(
            http(h, "GET", uri, &auth).await,
            StatusCode::UNAUTHORIZED,
            "{uri}"
        );
    }
    // Only as HTTP Basic user `fornax-job`.
    for bad in [
        basic("alice", &s.token),
        format!("Bearer {}", s.token),
        basic(JOB_TOKEN_USER, &format!("fnj_{}", "0".repeat(64))),
        basic(JOB_TOKEN_USER, "fnj_short"),
    ] {
        assert!(
            matches!(
                extract(&h.state, "GET", FETCH_REFS, &bad).await,
                Err(ApiError::Unauthorized)
            ),
            "{bad}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn job_token_invalid_after_attempt_finishes() {
    let s = setup().await;
    let state = &s.h.state;
    let auth = basic(JOB_TOKEN_USER, &s.token);
    assert!(extract(state, "GET", FETCH_REFS, &auth).await.is_ok());
    state
        .db
        .finish_attempt(
            s.attempt_id,
            s.runner_id,
            &Outcome {
                success: true,
                ..Outcome::default()
            },
            now_unix(),
        )
        .await
        .expect("finish");
    // Even before the token row is revoked, a finished attempt's token is dead.
    assert!(matches!(
        extract(state, "GET", FETCH_REFS, &auth).await,
        Err(ApiError::Unauthorized)
    ));
    assert_eq!(
        http(&s.h, "GET", FETCH_REFS, &auth).await,
        StatusCode::UNAUTHORIZED
    );
    assert!(
        state
            .db
            .revoke_job_token(s.attempt_id)
            .await
            .expect("revoke")
    );
}

/// Real `git` with the agent's credential style (`http.extraHeader`), end to end through
/// `githttp`. Needs `githttp` to pass the whole `MaybeUser` to `authorize_repo` (`&user`
/// instead of `user.as_ref()` in `info_refs` and `upload_pack`).
#[tokio::test(flavor = "multi_thread")]
async fn git_fetch_with_job_token() {
    let s = setup().await;
    let h = &s.h;
    let alice = h
        .state
        .db
        .user_by_name("alice")
        .await
        .expect("db")
        .expect("alice");
    let (pat, hash) = fornax_api::auth::generate_token().expect("token");
    h.state
        .db
        .create_token(alice.id, "push", &hash)
        .await
        .expect("token");
    let src = common::local_repo(&h.work, "src", &[("README.md", b"hi\n")]);
    let push_url = format!("{}/alice/demo.git", h.authed("alice", &pat));
    assert!(common::git(&src, &["push", "-q", &push_url, "HEAD:main"]));

    let header = format!(
        "http.extraHeader=Authorization: {}",
        basic(JOB_TOKEN_USER, &s.token)
    );
    let url = |repo: &str| format!("{}/alice/{repo}.git", h.base);
    let out = common::git_out(&h.work, &["-c", &header, "ls-remote", &url("demo")], &[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("refs/heads/main"));
    let clone = h.work.join("clone");
    let out = common::git_out(
        &h.work,
        &[
            "-c",
            &header,
            "clone",
            "-q",
            &url("demo"),
            clone.to_str().expect("utf8"),
        ],
        &[],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(clone.join("README.md").exists());
    // Other repositories and pushes stay closed.
    let out = common::git_out(&h.work, &["-c", &header, "ls-remote", &url("other")], &[]);
    assert!(!out.status.success());
    let out = common::git_out(
        &clone,
        &[
            "-c",
            &header,
            "push",
            "-q",
            &url("demo"),
            "HEAD:refs/heads/evil",
        ],
        &[],
    );
    assert!(!out.status.success());
}
