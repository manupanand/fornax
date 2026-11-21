// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! The Svelte web UI (`web/`), embedded into the binary at build time (H-012).
//!
//! Static files come from `web/dist`. Any other path that isn't API or Git returns
//! `index.html` so the client-side router can handle it. If the UI was not built, a small
//! placeholder page is served instead (the API keeps working).

use axum::http::{HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use rust_embed::Embed;

#[derive(Embed)]
#[folder = "$CARGO_MANIFEST_DIR/../../web/dist"]
#[allow_missing = true]
struct Assets;

const CSP: &str = "default-src 'self'; img-src 'self' data:; style-src 'self' 'unsafe-inline'; \
                   object-src 'none'; base-uri 'none'; frame-ancestors 'none'";

const PLACEHOLDER: &str = "<!doctype html><meta charset=utf-8><title>Fornax</title>\
<h1>Fornax</h1><p>The web UI has not been built. Run <code>npm ci &amp;&amp; npm run build</code> \
in <code>web/</code> and rebuild the binary. The API is available at \
<a href=\"/api/v1/health\">/api/v1/health</a>.</p>";

fn secure(mut res: Response) -> Response {
    let h = res.headers_mut();
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("same-origin"),
    );
    res
}

/// Whether an embedded UI is present.
pub fn available() -> bool {
    Assets::get("index.html").is_some()
}

pub async fn serve(method: Method, uri: Uri) -> Response {
    if method != Method::GET && method != Method::HEAD {
        return StatusCode::NOT_FOUND.into_response();
    }
    let path = uri.path().trim_start_matches('/');
    if let Some(file) = Assets::get(path).filter(|_| !path.is_empty()) {
        let mime = file.metadata.mimetype().to_owned();
        // Vite fingerprints everything under assets/, so it can be cached forever.
        let cache = if path.starts_with("assets/") {
            "public, max-age=31536000, immutable"
        } else {
            "no-cache"
        };
        return secure(
            (
                [
                    (header::CONTENT_TYPE, mime.as_str()),
                    (header::CACHE_CONTROL, cache),
                ],
                file.data.into_owned(),
            )
                .into_response(),
        );
    }
    let index = Assets::get("index.html").map(|f| f.data.into_owned());
    let body = index.unwrap_or_else(|| PLACEHOLDER.as_bytes().to_vec());
    secure(
        (
            [
                (header::CONTENT_TYPE, "text/html; charset=utf-8"),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            body,
        )
            .into_response(),
    )
}
