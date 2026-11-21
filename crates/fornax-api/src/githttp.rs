// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! Git smart HTTP protocol (`/{owner}/{repo}.git/...`).
//!
//! Wire handling is delegated to `git upload-pack` / `git receive-pack --stateless-rpc`
//! (D-019); this module does authentication, authorisation, framing and streaming:
//!
//! - request bodies are **streamed** into git (optionally gunzipped on the fly) and capped at
//!   `max_git_request_bytes` — nothing is buffered in memory (H-009);
//! - the `Git-Protocol` header is forwarded as `GIT_PROTOCOL`, enabling protocol v2 (H-010).

use std::process::Stdio;
use std::sync::Arc;

use async_compression::tokio::bufread::GzipDecoder;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, header};
use axum::response::{IntoResponse, Response};
use fornax_core::perm::Permission;
use fornax_db::{Db, Repo};
use fornax_git::GitService;
use futures_util::TryStreamExt;
use serde::Deserialize;
use serde_json::json;
use tokio::io::{AsyncRead, AsyncReadExt, BufReader};
use tokio_util::io::{ReaderStream, StreamReader};

use crate::AppState;
use crate::auth::{MaybeUser, authorize_repo};
use crate::error::{ApiError, ApiResult};

fn strip_git_suffix(repo: &str) -> &str {
    repo.strip_suffix(".git").unwrap_or(repo)
}

fn needed(service: GitService) -> Permission {
    match service {
        GitService::UploadPack => Permission::Read,
        GitService::ReceivePack => Permission::Write,
    }
}

/// One pkt-line: 4 hex digits of total length, then the payload.
fn pkt_line(payload: &str) -> String {
    format!("{:04x}{payload}", payload.len() + 4)
}

/// The client's `Git-Protocol` header, if well-formed (e.g. `version=2`).
fn git_protocol(headers: &HeaderMap) -> Option<String> {
    let v = headers.get("git-protocol")?.to_str().ok()?;
    let ok = !v.is_empty()
        && v.len() <= 256
        && v.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'=' | b':' | b'.' | b',' | b'-' | b'_')
        });
    ok.then(|| v.to_owned())
}

fn is_v2(protocol: Option<&str>) -> bool {
    protocol.is_some_and(|p| p.split(':').any(|kv| kv == "version=2"))
}

fn no_cache() -> [(header::HeaderName, &'static str); 2] {
    [
        (
            header::CACHE_CONTROL,
            "no-cache, max-age=0, must-revalidate",
        ),
        (header::PRAGMA, "no-cache"),
    ]
}

#[derive(Debug, Deserialize)]
pub struct InfoRefsQuery {
    pub service: Option<String>,
}

/// `GET /{owner}/{repo}.git/info/refs?service=git-upload-pack|git-receive-pack`
pub async fn info_refs(
    State(state): State<Arc<AppState>>,
    user: MaybeUser,
    Path((owner, repo)): Path<(String, String)>,
    Query(q): Query<InfoRefsQuery>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let service = q
        .service
        .as_deref()
        .and_then(GitService::from_name)
        .ok_or_else(|| ApiError::BadRequest("only the smart HTTP protocol is supported".into()))?;
    let (repo, _) = authorize_repo(
        &state,
        &owner,
        strip_git_suffix(&repo),
        &user,
        needed(service),
    )
    .await?;
    let protocol = git_protocol(&headers);
    let (o, n) = repo.storage_key();
    let mut cmd = state
        .repos
        .service_command(&state.repos.repo_path(&o, &n), service, true);
    if let Some(p) = &protocol {
        cmd.env("GIT_PROTOCOL", p);
    }
    let out = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    if !out.status.success() {
        return Err(ApiError::Internal(
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ));
    }
    // Protocol v2 starts directly with the capability advertisement (as git http-backend).
    let mut body = if is_v2(protocol.as_deref()) {
        Vec::new()
    } else {
        format!(
            "{}0000",
            pkt_line(&format!("# service={}\n", service.name()))
        )
        .into_bytes()
    };
    body.extend_from_slice(&out.stdout);
    let content_type = format!("application/x-{}-advertisement", service.name());
    Ok(([(header::CONTENT_TYPE, content_type)], no_cache(), body).into_response())
}

/// `POST /{owner}/{repo}.git/git-upload-pack`
pub async fn upload_pack(
    state: State<Arc<AppState>>,
    user: MaybeUser,
    path: Path<(String, String)>,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<Response> {
    rpc(state, user, path, headers, body, GitService::UploadPack).await
}

/// `POST /{owner}/{repo}.git/git-receive-pack`
pub async fn receive_pack(
    state: State<Arc<AppState>>,
    user: MaybeUser,
    path: Path<(String, String)>,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<Response> {
    rpc(state, user, path, headers, body, GitService::ReceivePack).await
}

async fn rpc(
    State(state): State<Arc<AppState>>,
    user: MaybeUser,
    Path((owner, repo)): Path<(String, String)>,
    headers: HeaderMap,
    body: Body,
    service: GitService,
) -> ApiResult<Response> {
    let (repo, _) = authorize_repo(
        &state,
        &owner,
        strip_git_suffix(&repo),
        &user,
        needed(service),
    )
    .await?;
    let expected = format!("application/x-{}-request", service.name());
    if headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        != Some(expected.as_str())
    {
        return Err(ApiError::BadRequest(format!(
            "expected Content-Type {expected}"
        )));
    }
    let gzipped = headers
        .get(header::CONTENT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("gzip") || v.eq_ignore_ascii_case("x-gzip"));

    // Request body → (gunzip) → byte cap → git stdin, all streaming.
    let raw = StreamReader::new(body.into_data_stream().map_err(std::io::Error::other));
    let input: Box<dyn AsyncRead + Send + Unpin> = if gzipped {
        Box::new(GzipDecoder::new(BufReader::new(raw)))
    } else {
        Box::new(raw)
    };
    let limit = state.config.max_git_request_bytes as u64;

    // Ref heads before the push, to find out afterwards what it updated (CI triggers).
    let before = if service == GitService::ReceivePack {
        match fornax_ci::before_push(&state.repos, &repo).await {
            Ok(refs) => Some(refs),
            Err(e) => {
                tracing::error!(error = %e, repo = %repo.full_name(), "ref snapshot failed; CI will not run for this push");
                None
            }
        }
    } else {
        None
    };

    let (o, n) = repo.storage_key();
    let mut cmd = state
        .repos
        .service_command(&state.repos.repo_path(&o, &n), service, false);
    if let Some(p) = git_protocol(&headers) {
        cmd.env("GIT_PROTOCOL", p);
    }
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let (Some(mut stdin), Some(stdout), Some(mut stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        return Err(ApiError::Internal("git pipes unavailable".into()));
    };

    tokio::spawn(async move {
        let mut capped = input.take(limit + 1);
        match tokio::io::copy(&mut capped, &mut stdin).await {
            // Closing stdin early makes git abort on the truncated request.
            Ok(n) if n > limit => tracing::warn!(limit, "git request body exceeds limit"),
            Ok(_) => {}
            Err(e) => tracing::debug!(error = %e, "git request stream ended early"),
        }
    });

    let actor = user.as_ref().map(|u| u.id);
    let state = state.clone();
    tokio::spawn(async move {
        let mut err = Vec::new();
        let _ = stderr.read_to_end(&mut err).await;
        match child.wait().await {
            Ok(status) if status.success() => {
                if service == GitService::ReceivePack {
                    record_push(&state.db, actor, &repo).await;
                    if let Some(before) = before {
                        trigger_ci(&state, actor, &repo, &before).await;
                    }
                }
            }
            Ok(status) => tracing::warn!(
                %status,
                stderr = %String::from_utf8_lossy(&err),
                service = service.name(),
                "git rpc failed"
            ),
            Err(e) => tracing::error!(error = %e, "failed to wait for git"),
        }
    });

    let content_type = format!("application/x-{}-result", service.name());
    Ok((
        [(header::CONTENT_TYPE, content_type)],
        no_cache(),
        Body::from_stream(ReaderStream::new(stdout)),
    )
        .into_response())
}

/// Create pipelines for the refs a push updated and wake runners. Never fails the push:
/// spec problems and errors are logged.
async fn trigger_ci(
    state: &AppState,
    actor: Option<i64>,
    repo: &Repo,
    before: &std::collections::BTreeMap<String, String>,
) {
    let now = fornax_core::now_unix();
    match fornax_ci::after_push(
        &state.db,
        &state.repos,
        repo,
        actor,
        before,
        &state.ci_wakeup,
        now,
    )
    .await
    {
        Ok(outcome) => {
            for p in &outcome.created {
                tracing::info!(repo = %repo.full_name(), pipeline = p.number, name = %p.name, "pipeline created");
            }
            for (git_ref, diags) in &outcome.errors {
                for d in diags {
                    tracing::warn!(repo = %repo.full_name(), %git_ref, diagnostic = %d, ".fornax spec error");
                }
            }
        }
        Err(e) => {
            tracing::error!(error = %e, repo = %repo.full_name(), "creating pipelines failed")
        }
    }
}

/// Bump `updated_at` and audit a successful push (shared with the SSH transport).
pub async fn record_push(db: &Db, actor: Option<i64>, repo: &Repo) {
    let result = async {
        db.touch_repo(repo.id).await?;
        db.audit(
            actor,
            "repo.push",
            &repo.full_name(),
            &json!({}).to_string(),
        )
        .await
    }
    .await;
    if let Err(e) = result {
        tracing::error!(error = %e, repo = %repo.full_name(), "failed to record push");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkt_lines() {
        assert_eq!(
            pkt_line("# service=git-upload-pack\n"),
            "001e# service=git-upload-pack\n"
        );
    }

    #[test]
    fn strips_suffix() {
        assert_eq!(strip_git_suffix("repo.git"), "repo");
        assert_eq!(strip_git_suffix("repo"), "repo");
    }

    #[test]
    fn protocol_header() {
        let mut h = HeaderMap::new();
        assert_eq!(git_protocol(&h), None);
        h.insert("git-protocol", "version=2".parse().expect("hv"));
        assert_eq!(git_protocol(&h).as_deref(), Some("version=2"));
        assert!(is_v2(git_protocol(&h).as_deref()));
        h.insert("git-protocol", "version=2;rm -rf".parse().expect("hv"));
        assert_eq!(git_protocol(&h), None);
        assert!(!is_v2(Some("version=1")));
    }
}
