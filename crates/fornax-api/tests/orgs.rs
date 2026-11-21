// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! Organisations, members, teams and their effect on repository access (H-016).

mod common;

use axum::http::StatusCode;
use common::{Harness, git, local_repo};
use serde_json::json;

#[tokio::test(flavor = "multi_thread")]
async fn organisations_teams_and_roles() {
    let h = Harness::start().await;
    let (_, owner) = h.user("owner").await;
    let (_, dev) = h.user("dev").await;
    let (_, viewer) = h.user("viewer").await;
    let (_, outsider) = h.user("outsider").await;

    // Create an organisation; names are shared with users.
    let (status, org) = h
        .call(
            "POST",
            "/api/v1/orgs",
            Some(&owner),
            Some(json!({ "name": "acme" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{org}");
    assert_eq!(org["role"], "owner");
    assert_eq!(
        h.status(
            "POST",
            "/api/v1/orgs",
            Some(&dev),
            Some(json!({ "name": "ACME" }))
        )
        .await,
        StatusCode::CONFLICT
    );
    assert_eq!(
        h.status(
            "POST",
            "/api/v1/orgs",
            Some(&dev),
            Some(json!({ "name": "owner" }))
        )
        .await,
        StatusCode::CONFLICT
    );
    assert_eq!(
        h.status("GET", "/api/v1/orgs/acme", None, None).await,
        StatusCode::OK
    );
    assert_eq!(
        h.status("GET", "/api/v1/orgs/owner", None, None).await,
        StatusCode::NOT_FOUND,
        "users are not orgs"
    );

    // Organisation repositories: only owners create them.
    let new_repo = |name: &str, private: bool| json!({ "name": name, "private": private });
    assert_eq!(
        h.status(
            "POST",
            "/api/v1/orgs/acme/repos",
            Some(&dev),
            Some(new_repo("core", true))
        )
        .await,
        StatusCode::NOT_FOUND
    );
    let (status, repo) = h
        .call(
            "POST",
            "/api/v1/orgs/acme/repos",
            Some(&owner),
            Some(new_repo("core", true)),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{repo}");
    assert_eq!(
        (repo["owner"].as_str(), repo["owner_kind"].as_str()),
        (Some("acme"), Some("org"))
    );
    h.call(
        "POST",
        "/api/v1/orgs/acme/repos",
        Some(&owner),
        Some(new_repo("site", false)),
    )
    .await;

    // Members.
    let role = |r: &str| json!({ "role": r });
    for who in ["dev", "viewer"] {
        let uri = format!("/api/v1/orgs/acme/members/{who}");
        assert_eq!(
            h.status("PUT", &uri, Some(&owner), Some(role("member")))
                .await,
            StatusCode::NO_CONTENT
        );
    }
    assert_eq!(
        h.status(
            "PUT",
            "/api/v1/orgs/acme/members/outsider",
            Some(&dev),
            Some(role("member"))
        )
        .await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        h.status(
            "PUT",
            "/api/v1/orgs/acme/members/nobody",
            Some(&owner),
            Some(role("member"))
        )
        .await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        h.status(
            "PUT",
            "/api/v1/orgs/acme/members/owner",
            Some(&owner),
            Some(role("member"))
        )
        .await,
        StatusCode::CONFLICT,
        "last owner"
    );
    assert_eq!(
        h.status(
            "DELETE",
            "/api/v1/orgs/acme/members/owner",
            Some(&owner),
            None
        )
        .await,
        StatusCode::CONFLICT
    );
    let (_, members) = h
        .call("GET", "/api/v1/orgs/acme/members", Some(&viewer), None)
        .await;
    assert_eq!(members.as_array().map(Vec::len), Some(3));
    assert_eq!(
        h.status("GET", "/api/v1/orgs/acme/members", Some(&outsider), None)
            .await,
        StatusCode::NOT_FOUND
    );

    // Members read every org repository; outsiders see only public ones.
    let perm = |token: String, repo: &'static str| {
        let h = &h;
        async move {
            let (s, v) = h
                .call(
                    "GET",
                    &format!("/api/v1/repos/acme/{repo}"),
                    Some(&token),
                    None,
                )
                .await;
            if s == StatusCode::OK {
                v["permission"].as_str().unwrap_or("?").to_owned()
            } else {
                s.as_u16().to_string()
            }
        }
    };
    assert_eq!(perm(viewer.clone(), "core").await, "read");
    assert_eq!(perm(outsider.clone(), "core").await, "404");
    assert_eq!(perm(outsider.clone(), "site").await, "read");
    let (_, listing) = h
        .call("GET", "/api/v1/orgs/acme/repos", Some(&outsider), None)
        .await;
    assert_eq!(listing.as_array().map(Vec::len), Some(1));

    // Teams grant write on chosen repositories.
    let team = json!({ "name": "devs", "permission": "write" });
    assert_eq!(
        h.status(
            "POST",
            "/api/v1/orgs/acme/teams",
            Some(&dev),
            Some(team.clone())
        )
        .await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        h.status("POST", "/api/v1/orgs/acme/teams", Some(&owner), Some(team))
            .await,
        StatusCode::CREATED
    );
    assert_eq!(
        h.status(
            "POST",
            "/api/v1/orgs/acme/teams",
            Some(&owner),
            Some(json!({ "name": "x", "permission": "none" }))
        )
        .await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        h.status(
            "PUT",
            "/api/v1/orgs/acme/teams/devs/members/outsider",
            Some(&owner),
            None
        )
        .await,
        StatusCode::BAD_REQUEST,
        "must be a member"
    );
    assert_eq!(
        h.status(
            "PUT",
            "/api/v1/orgs/acme/teams/devs/members/dev",
            Some(&owner),
            None
        )
        .await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        h.status(
            "PUT",
            "/api/v1/orgs/acme/teams/devs/repos/core",
            Some(&owner),
            None
        )
        .await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        h.status(
            "PUT",
            "/api/v1/orgs/acme/teams/devs/repos/missing",
            Some(&owner),
            None
        )
        .await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(perm(dev.clone(), "core").await, "write");
    assert_eq!(perm(viewer.clone(), "core").await, "read");
    let (_, teams) = h
        .call("GET", "/api/v1/orgs/acme/teams", Some(&viewer), None)
        .await;
    assert_eq!(teams[0]["name"], "devs");
    let (_, tm) = h
        .call(
            "GET",
            "/api/v1/orgs/acme/teams/devs/members",
            Some(&viewer),
            None,
        )
        .await;
    assert_eq!(tm, json!(["dev"]));

    // Write via team means push works; read-only members can't push.
    let src = local_repo(&h.work, "src", &[("x", b"x")]);
    let url = |u: &str, t: &str| format!("{}/acme/core.git", h.authed(u, t));
    assert!(!git(&src, &["push", "-q", &url("viewer", &viewer), "main"]));
    assert!(git(&src, &["push", "-q", &url("dev", &dev), "main"]));
    assert!(git(
        &h.work,
        &["clone", "-q", &url("viewer", &viewer), "viewer-clone"]
    ));

    // Removing the team grant or the member drops access again.
    assert_eq!(
        h.status(
            "DELETE",
            "/api/v1/orgs/acme/teams/devs/repos/core",
            Some(&owner),
            None
        )
        .await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(perm(dev.clone(), "core").await, "read");
    assert_eq!(
        h.status(
            "DELETE",
            "/api/v1/orgs/acme/members/dev",
            Some(&owner),
            None
        )
        .await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(perm(dev.clone(), "core").await, "404");

    // Promote a second owner, then the first may step down.
    assert_eq!(
        h.status(
            "PUT",
            "/api/v1/orgs/acme/members/viewer",
            Some(&owner),
            Some(role("owner"))
        )
        .await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        h.status(
            "PUT",
            "/api/v1/orgs/acme/members/owner",
            Some(&owner),
            Some(role("member"))
        )
        .await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        h.status(
            "DELETE",
            "/api/v1/orgs/acme/teams/devs",
            Some(&viewer),
            None
        )
        .await,
        StatusCode::NO_CONTENT
    );
}
