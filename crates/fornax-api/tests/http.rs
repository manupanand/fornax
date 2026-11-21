// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! Repositories end to end: REST API, `git clone`/`push` over HTTP, settings (H-011).

mod common;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use common::{Harness, git, local_repo};
use serde_json::json;
use tower::ServiceExt;

#[tokio::test(flavor = "multi_thread")]
async fn create_push_clone_and_browse() {
    let h = Harness::start().await;
    let (_, alice) = h.user("alice").await;
    let (_, bob) = h.user("bob").await;

    assert_eq!(
        h.status("GET", "/api/v1/health", None, None).await,
        StatusCode::OK
    );
    assert_eq!(
        h.status("GET", "/api/v1/user", None, None).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        h.status("GET", "/api/v1/user", Some("fnx_bogus"), None)
            .await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        h.status("GET", "/api/v1/nope", None, None).await,
        StatusCode::NOT_FOUND
    );
    let (status, me) = h.call("GET", "/api/v1/user", Some(&alice), None).await;
    assert_eq!(
        (status, me["username"].as_str()),
        (StatusCode::OK, Some("alice"))
    );

    let (status, repo) = h
        .call(
            "POST",
            "/api/v1/user/repos",
            Some(&alice),
            Some(json!({ "name": "demo" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{repo}");
    assert_eq!(repo["clone_url"], format!("{}/alice/demo.git", h.base));
    assert_eq!(repo["permission"], "admin");
    let secret = json!({ "name": "secret", "private": true });
    assert_eq!(
        h.status("POST", "/api/v1/user/repos", Some(&alice), Some(secret))
            .await,
        StatusCode::CREATED
    );
    for (body, want) in [
        (json!({ "name": "DEMO" }), StatusCode::CONFLICT),
        (json!({ "name": "bad/name" }), StatusCode::BAD_REQUEST),
        (
            json!({ "name": "x", "unknown": 1 }),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
    ] {
        assert_eq!(
            h.status("POST", "/api/v1/user/repos", Some(&alice), Some(body))
                .await,
            want
        );
    }
    assert_eq!(
        h.status(
            "POST",
            "/api/v1/user/repos",
            None,
            Some(json!({ "name": "x" }))
        )
        .await,
        StatusCode::UNAUTHORIZED
    );

    // Visibility.
    let (_, listing) = h.call("GET", "/api/v1/users/alice/repos", None, None).await;
    assert_eq!(listing.as_array().map(Vec::len), Some(1));
    let (_, listing) = h
        .call("GET", "/api/v1/users/alice/repos", Some(&alice), None)
        .await;
    assert_eq!(listing.as_array().map(Vec::len), Some(2));
    assert_eq!(
        h.status("GET", "/api/v1/repos/alice/secret", None, None)
            .await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        h.status("GET", "/api/v1/repos/alice/secret", Some(&bob), None)
            .await,
        StatusCode::NOT_FOUND
    );
    let (_, pub_view) = h
        .call("GET", "/api/v1/repos/alice/demo", Some(&bob), None)
        .await;
    assert_eq!(pub_view["permission"], "read");

    // Push over smart HTTP with Basic auth (username + token).
    let src = local_repo(
        &h.work,
        "src",
        &[
            ("README.md", b"# demo\n"),
            (".fornax/ci.yaml", b"kind: Pipeline\nversion: 1\n"),
        ],
    );
    let anon = format!("{}/alice/demo.git", h.base);
    let bob_push = format!("{}/alice/demo.git", h.authed("bob", &bob));
    let alice_url = h.authed("alice", &alice);
    assert!(
        !git(&src, &["push", "-q", &anon, "main"]),
        "anonymous push must fail"
    );
    assert!(
        !git(&src, &["push", "-q", &bob_push, "main"]),
        "non-owner push must fail"
    );
    assert!(git(
        &src,
        &["push", "-q", &format!("{alice_url}/alice/demo.git"), "main"]
    ));

    assert!(git(&h.work, &["clone", "-q", &anon, "clone"]));
    assert_eq!(
        std::fs::read_to_string(h.work.join("clone/README.md")).expect("read"),
        "# demo\n"
    );
    assert!(!git(
        &h.work,
        &[
            "clone",
            "-q",
            &format!("{}/alice/secret.git", h.base),
            "nope"
        ]
    ));
    assert!(git(
        &h.work,
        &[
            "clone",
            "-q",
            &format!("{alice_url}/alice/secret.git"),
            "secret"
        ]
    ));

    // Browse.
    let (_, branches) = h
        .call("GET", "/api/v1/repos/alice/demo/branches", None, None)
        .await;
    assert_eq!(branches[0]["name"], "main");
    let (_, commits) = h
        .call("GET", "/api/v1/repos/alice/demo/commits", None, None)
        .await;
    assert_eq!(commits[0]["summary"], "initial import");
    let (_, tree) = h
        .call("GET", "/api/v1/repos/alice/demo/tree", None, None)
        .await;
    let names: Vec<_> = tree
        .as_array()
        .expect("array")
        .iter()
        .filter_map(|e| e["name"].as_str())
        .collect();
    assert_eq!(names, [".fornax", "README.md"]);
    let (_, tree) = h
        .call(
            "GET",
            "/api/v1/repos/alice/demo/tree?path=.fornax",
            None,
            None,
        )
        .await;
    assert_eq!(tree[0]["name"], "ci.yaml");

    let raw = h
        .app
        .clone()
        .oneshot(
            Request::get("/api/v1/repos/alice/demo/raw?path=README.md")
                .body(Body::empty())
                .expect("req"),
        )
        .await
        .expect("res");
    assert_eq!(
        raw.headers()[header::CONTENT_TYPE],
        "application/octet-stream"
    );
    assert_eq!(
        &to_bytes(raw.into_body(), usize::MAX).await.expect("body")[..],
        b"# demo\n"
    );
    assert_eq!(
        h.status("GET", "/api/v1/repos/alice/demo/raw?path=nope", None, None)
            .await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        h.status(
            "GET",
            "/api/v1/repos/alice/demo/commits?ref=--all",
            None,
            None
        )
        .await,
        StatusCode::NOT_FOUND
    );

    // Pushes are audited.
    let audit = h.state.db.recent_audit(5).await.expect("audit");
    assert!(
        audit
            .iter()
            .any(|e| e.action == "repo.push" && e.target == "alice/demo")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn update_rename_and_delete() {
    let h = Harness::start().await;
    let (_, alice) = h.user("alice").await;
    let (_, bob) = h.user("bob").await;
    h.call(
        "POST",
        "/api/v1/user/repos",
        Some(&alice),
        Some(json!({ "name": "app" })),
    )
    .await;
    let src = local_repo(&h.work, "src", &[("a.txt", b"a\n")]);
    let alice_url = h.authed("alice", &alice);
    assert!(git(
        &src,
        &["push", "-q", &format!("{alice_url}/alice/app.git"), "main"]
    ));
    assert!(git(
        &src,
        &[
            "push",
            "-q",
            &format!("{alice_url}/alice/app.git"),
            "main:develop"
        ]
    ));

    // Only admins of the repository may change settings.
    let patch = json!({ "description": "x" });
    assert_eq!(
        h.status(
            "PATCH",
            "/api/v1/repos/alice/app",
            Some(&bob),
            Some(patch.clone())
        )
        .await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        h.status("PATCH", "/api/v1/repos/alice/app", None, Some(patch))
            .await,
        StatusCode::UNAUTHORIZED
    );

    let (status, repo) = h
        .call(
            "PATCH",
            "/api/v1/repos/alice/app",
            Some(&alice),
            Some(json!({ "name": "service", "description": "API", "private": true, "default_branch": "develop" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{repo}");
    assert_eq!(
        (repo["name"].as_str(), repo["private"].as_bool()),
        (Some("service"), Some(true))
    );
    assert_eq!(repo["default_branch"], "develop");
    assert_eq!(
        h.status("GET", "/api/v1/repos/alice/app", Some(&alice), None)
            .await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        h.status("GET", "/api/v1/repos/alice/service", Some(&bob), None)
            .await,
        StatusCode::NOT_FOUND
    );

    // The rename moved the repository on disk and HEAD follows the new default branch.
    assert!(git(
        &h.work,
        &[
            "clone",
            "-q",
            &format!("{alice_url}/alice/service.git"),
            "c"
        ]
    ));
    let head = common::git_out(
        &h.work.join("c"),
        &["rev-parse", "--abbrev-ref", "HEAD"],
        &[],
    );
    assert_eq!(String::from_utf8_lossy(&head.stdout).trim(), "develop");

    for body in [
        json!({ "default_branch": "nope" }),
        json!({ "name": "../x" }),
    ] {
        assert_eq!(
            h.status(
                "PATCH",
                "/api/v1/repos/alice/service",
                Some(&alice),
                Some(body)
            )
            .await,
            StatusCode::BAD_REQUEST
        );
    }
    h.call(
        "POST",
        "/api/v1/user/repos",
        Some(&alice),
        Some(json!({ "name": "other" })),
    )
    .await;
    assert_eq!(
        h.status(
            "PATCH",
            "/api/v1/repos/alice/service",
            Some(&alice),
            Some(json!({ "name": "OTHER" }))
        )
        .await,
        StatusCode::BAD_REQUEST,
        "rename onto an existing repository must fail"
    );

    assert_eq!(
        h.status("DELETE", "/api/v1/repos/alice/service", Some(&bob), None)
            .await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        h.status("DELETE", "/api/v1/repos/alice/service", Some(&alice), None)
            .await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        h.status("GET", "/api/v1/repos/alice/service", Some(&alice), None)
            .await,
        StatusCode::NOT_FOUND
    );
    assert!(!h.state.repos.repo_path("alice", "service").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn collaborators_grant_access() {
    let h = Harness::start().await;
    let (_, alice) = h.user("alice").await;
    let (_, bob) = h.user("bob").await;
    h.call(
        "POST",
        "/api/v1/user/repos",
        Some(&alice),
        Some(json!({ "name": "p", "private": true })),
    )
    .await;
    assert_eq!(
        h.status("GET", "/api/v1/repos/alice/p", Some(&bob), None)
            .await,
        StatusCode::NOT_FOUND
    );

    let put = |perm: &str| json!({ "permission": perm });
    assert_eq!(
        h.status(
            "PUT",
            "/api/v1/repos/alice/p/collaborators/bob",
            Some(&bob),
            Some(put("admin"))
        )
        .await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        h.status(
            "PUT",
            "/api/v1/repos/alice/p/collaborators/bob",
            Some(&alice),
            Some(put("owner"))
        )
        .await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        h.status(
            "PUT",
            "/api/v1/repos/alice/p/collaborators/bob",
            Some(&alice),
            Some(put("read"))
        )
        .await,
        StatusCode::NO_CONTENT
    );
    let (_, view) = h
        .call("GET", "/api/v1/repos/alice/p", Some(&bob), None)
        .await;
    assert_eq!(view["permission"], "read");

    let src = local_repo(&h.work, "src", &[("f", b"1")]);
    let bob_url = format!("{}/alice/p.git", h.authed("bob", &bob));
    assert!(
        !git(&src, &["push", "-q", &bob_url, "main"]),
        "read collaborator cannot push"
    );
    h.call(
        "PUT",
        "/api/v1/repos/alice/p/collaborators/bob",
        Some(&alice),
        Some(put("write")),
    )
    .await;
    assert!(
        git(&src, &["push", "-q", &bob_url, "main"]),
        "write collaborator can push"
    );

    let (_, list) = h
        .call(
            "GET",
            "/api/v1/repos/alice/p/collaborators",
            Some(&alice),
            None,
        )
        .await;
    assert_eq!(list[0]["username"], "bob");
    assert_eq!(
        h.status(
            "DELETE",
            "/api/v1/repos/alice/p/collaborators/bob",
            Some(&alice),
            None
        )
        .await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        h.status("GET", "/api/v1/repos/alice/p", Some(&bob), None)
            .await,
        StatusCode::NOT_FOUND
    );
}
