// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! Shared test harness: a live Fornax server on a random port plus helpers.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use fornax_api::auth::generate_token;
use fornax_api::{ApiConfig, AppState, Oidc, OidcConfig, router};
use fornax_db::{Db, User};
use fornax_git::RepoStore;
use serde_json::Value;
use tower::ServiceExt;

pub struct Harness {
    pub tmp: tempfile::TempDir,
    pub app: Router,
    pub state: Arc<AppState>,
    pub base: String,
    pub work: PathBuf,
}

impl Harness {
    pub async fn start() -> Self {
        Self::start_with(None, ApiConfig::default()).await
    }

    /// Start a server; `oidc` receives this server's base URL.
    pub async fn start_with(oidc: Option<OidcConfig>, config: ApiConfig) -> Self {
        Self::start_full(oidc, config, fornax_ci::Wakeup::noop()).await
    }

    /// Start a server with a custom runner wake-up callback.
    pub async fn start_full(
        oidc: Option<OidcConfig>,
        mut config: ApiConfig,
        ci_wakeup: fornax_ci::Wakeup,
    ) -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db = Db::open(&tmp.path().join("fornax.db")).await.expect("db");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        config.public_url = base.clone();
        let state = Arc::new(AppState {
            db,
            repos: RepoStore::new(tmp.path().join("repos")),
            config,
            oidc: oidc.map(Oidc::new),
            ci_wakeup,
        });
        let app = router(state.clone());
        let served = app.clone();
        tokio::spawn(async move { axum::serve(listener, served).await.expect("serve") });
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work).expect("mkdir");
        Self {
            tmp,
            app,
            state,
            base,
            work,
        }
    }

    /// Create a user and return `(user, token)`.
    pub async fn user(&self, name: &str) -> (User, String) {
        self.user_with(name, false).await
    }

    pub async fn user_with(&self, name: &str, admin: bool) -> (User, String) {
        let user = self.state.db.create_user(name, admin).await.expect("user");
        let (token, hash) = generate_token().expect("token");
        self.state
            .db
            .create_token(user.id, "test", &hash)
            .await
            .expect("store token");
        (user, token)
    }

    pub async fn call(
        &self,
        method: &str,
        uri: &str,
        token: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut req = Request::builder().method(method).uri(uri);
        if let Some(t) = token {
            req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        let req = match body {
            Some(b) => req
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(b.to_string())),
            None => req.body(Body::empty()),
        }
        .expect("request");
        let res = self.app.clone().oneshot(req).await.expect("response");
        let status = res.status();
        let bytes = to_bytes(res.into_body(), usize::MAX).await.expect("body");
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    pub async fn status(
        &self,
        method: &str,
        uri: &str,
        token: Option<&str>,
        body: Option<Value>,
    ) -> StatusCode {
        self.call(method, uri, token, body).await.0
    }

    /// `http://user:token@host` for git URLs.
    pub fn authed(&self, user: &str, token: &str) -> String {
        self.base
            .replacen("http://", &format!("http://{user}:{token}@"), 1)
    }
}

/// Run git isolated from the developer's config, never prompting.
pub fn git_out(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args);
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().expect("run git")
}

pub fn git(dir: &Path, args: &[&str]) -> bool {
    git_out(dir, args, &[]).status.success()
}

/// A local repository at `work/<name>` with one commit containing `files`.
pub fn local_repo(work: &Path, name: &str, files: &[(&str, &[u8])]) -> PathBuf {
    let dir = work.join(name);
    for (path, data) in files {
        let p = dir.join(path);
        std::fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
        std::fs::write(p, data).expect("write");
    }
    assert!(git(&dir, &["init", "-q"]));
    assert!(git(&dir, &["add", "."]));
    assert!(git(&dir, &["commit", "-q", "-m", "initial import"]));
    dir
}
