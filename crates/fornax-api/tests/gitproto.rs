// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! Git transport details: streamed large pushes and the size cap (H-009), gzip request
//! bodies, and protocol v2 (H-010).

mod common;

use std::io::Write;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use common::{Harness, git, git_out, local_repo};
use fornax_api::ApiConfig;
use serde_json::json;
use tower::ServiceExt;

/// Incompressible bytes (xorshift), so pack size ≈ file size.
fn noise(len: usize, mut seed: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(len + 8);
    while out.len() < len {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        out.extend_from_slice(&seed.to_le_bytes());
    }
    out.truncate(len);
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn large_push_streams_through() {
    let h = Harness::start().await;
    let (_, alice) = h.user("alice").await;
    h.call(
        "POST",
        "/api/v1/user/repos",
        Some(&alice),
        Some(json!({ "name": "big" })),
    )
    .await;
    let data = noise(40 * 1024 * 1024, 0x9e37_79b9_7f4a_7c15);
    let src = local_repo(&h.work, "src", &[("blob.bin", &data)]);
    let url = format!("{}/alice/big.git", h.authed("alice", &alice));
    let out = git_out(&src, &["push", "-q", &url, "main"], &[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(git(&h.work, &["clone", "-q", &url, "back"]));
    assert_eq!(
        std::fs::read(h.work.join("back/blob.bin"))
            .expect("read")
            .len(),
        data.len()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn request_size_cap_is_enforced() {
    let config = ApiConfig {
        max_git_request_bytes: 1024 * 1024,
        ..ApiConfig::default()
    };
    let h = Harness::start_with(None, config).await;
    let (_, alice) = h.user("alice").await;
    h.call(
        "POST",
        "/api/v1/user/repos",
        Some(&alice),
        Some(json!({ "name": "capped" })),
    )
    .await;
    let src = local_repo(&h.work, "src", &[("blob.bin", &noise(4 * 1024 * 1024, 7))]);
    let url = format!("{}/alice/capped.git", h.authed("alice", &alice));
    assert!(
        !git(&src, &["push", "-q", &url, "main"]),
        "push over the cap must fail"
    );
    let (_, branches) = h
        .call(
            "GET",
            "/api/v1/repos/alice/capped/branches",
            Some(&alice),
            None,
        )
        .await;
    assert_eq!(branches, json!([]), "nothing may be written");
}

#[tokio::test(flavor = "multi_thread")]
async fn protocol_v2_and_gzip_requests() {
    let h = Harness::start().await;
    let (_, alice) = h.user("alice").await;
    h.call(
        "POST",
        "/api/v1/user/repos",
        Some(&alice),
        Some(json!({ "name": "v2" })),
    )
    .await;
    let src = local_repo(&h.work, "src", &[("a.txt", b"hello v2\n")]);
    let url = format!("{}/alice/v2.git", h.authed("alice", &alice));
    assert!(git(&src, &["push", "-q", &url, "main"]));

    // Clone with protocol v2 and check the server actually spoke v2.
    let anon = format!("{}/alice/v2.git", h.base);
    let out = git_out(
        &h.work,
        &["-c", "protocol.version=2", "clone", &anon, "v2clone"],
        &[("GIT_TRACE_PACKET", "1")],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let trace = String::from_utf8_lossy(&out.stderr);
    assert!(
        trace.contains("version 2"),
        "expected a v2 capability advertisement"
    );
    assert!(git(
        &h.work,
        &["-c", "protocol.version=0", "clone", "-q", &anon, "v0clone"]
    ));

    // A hand-made, gzip-compressed v0 upload-pack request.
    let head = String::from_utf8(git_out(&src, &["rev-parse", "HEAD"], &[]).stdout).expect("utf8");
    let want = format!("want {}\n", head.trim());
    let request = format!("{:04x}{want}00000009done\n", want.len() + 4);
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(request.as_bytes()).expect("gzip");
    let res = h
        .app
        .clone()
        .oneshot(
            Request::post("/alice/v2.git/git-upload-pack")
                .header(
                    header::CONTENT_TYPE,
                    "application/x-git-upload-pack-request",
                )
                .header(header::CONTENT_ENCODING, "gzip")
                .body(Body::from(gz.finish().expect("gzip")))
                .expect("req"),
        )
        .await
        .expect("res");
    assert_eq!(res.status(), StatusCode::OK);
    let body = to_bytes(res.into_body(), usize::MAX).await.expect("body");
    assert!(
        body.starts_with(b"0008NAK\n"),
        "{:?}",
        &body[..body.len().min(16)]
    );
    assert!(
        body.windows(4).any(|w| w == b"PACK"),
        "response carries a pack"
    );

    // Wrong content type and the dumb protocol are rejected.
    let bad = Request::post("/alice/v2.git/git-upload-pack")
        .body(Body::empty())
        .expect("req");
    assert_eq!(
        h.app.clone().oneshot(bad).await.expect("res").status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        h.status("GET", "/alice/v2.git/info/refs", None, None).await,
        StatusCode::BAD_REQUEST
    );
}
