// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! The embedded Svelte UI (H-012): SPA fallback, asset caching, security headers.

mod common;

use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode, header};
use common::Harness;
use tower::ServiceExt;

async fn get(
    h: &Harness,
    method: Method,
    uri: &str,
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let res = h
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .body(Body::empty())
                .expect("req"),
        )
        .await
        .expect("res");
    let (status, headers) = (res.status(), res.headers().clone());
    (
        status,
        headers,
        to_bytes(res.into_body(), usize::MAX)
            .await
            .expect("body")
            .to_vec(),
    )
}

#[tokio::test]
async fn serves_the_single_page_app() {
    let h = Harness::start().await;
    for path in ["/", "/alice", "/alice/demo/tree/main/src", "/settings"] {
        let (status, headers, body) = get(&h, Method::GET, path).await;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert_eq!(headers[header::CONTENT_TYPE], "text/html; charset=utf-8");
        assert!(
            headers[header::CONTENT_SECURITY_POLICY]
                .to_str()
                .expect("csp")
                .contains("default-src 'self'")
        );
        assert_eq!(headers[header::X_FRAME_OPTIONS], "DENY");
        let html = String::from_utf8(body).expect("utf8");
        if fornax_api::ui::available() {
            assert!(html.contains("<div id=\"app\">"), "{path}: {html}");
        } else {
            assert!(html.contains("web UI has not been built"));
        }
    }

    if fornax_api::ui::available() {
        let (_, _, index) = get(&h, Method::GET, "/").await;
        let index = String::from_utf8(index).expect("utf8");
        let asset = index
            .split('"')
            .find(|s| s.starts_with("/assets/") && s.ends_with(".js"))
            .expect("script asset");
        let (status, headers, body) = get(&h, Method::GET, asset).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            headers[header::CONTENT_TYPE]
                .to_str()
                .expect("ct")
                .contains("javascript")
        );
        assert!(
            headers[header::CACHE_CONTROL]
                .to_str()
                .expect("cc")
                .contains("immutable")
        );
        assert!(!body.is_empty());
        let (status, headers, _) = get(&h, Method::GET, "/favicon.svg").await;
        assert_eq!(
            (status, headers[header::CONTENT_TYPE].to_str().expect("ct")),
            (StatusCode::OK, "image/svg+xml")
        );
    }

    // API and non-GET requests never fall back to HTML.
    let (status, headers, _) = get(&h, Method::GET, "/api/v1/does-not-exist").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(headers[header::CONTENT_TYPE], "application/json");
    assert_eq!(
        get(&h, Method::POST, "/alice").await.0,
        StatusCode::NOT_FOUND
    );
}
