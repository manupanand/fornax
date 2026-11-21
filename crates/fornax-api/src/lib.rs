// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! Fornax HTTP layer: REST API (`/api/v1`), Git smart HTTP (`/{owner}/{repo}.git`) and the
//! embedded web UI (everything else).

pub mod auth;
pub mod ci;
pub mod error;
pub mod githttp;
pub mod keys;
pub mod oidc;
pub mod orgs;
pub mod repos;
pub mod ui;

use std::sync::Arc;

use axum::Router;
use axum::routing::{delete, get, post, put};
use fornax_db::Db;
use fornax_git::RepoStore;

pub use oidc::{Oidc, OidcConfig};

#[derive(Debug, Clone)]
pub struct ApiConfig {
    /// Base URL clients use to reach this server, e.g. `https://git.example.com`.
    pub public_url: String,
    /// Prefix for SSH clone URLs, e.g. `ssh://git@git.example.com:2222/`, if SSH is enabled.
    pub ssh_url_base: Option<String>,
    /// Upper bound for a (decompressed) git push/fetch request body.
    pub max_git_request_bytes: usize,
    /// Lifetime of browser sessions.
    pub session_ttl_secs: i64,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            public_url: "http://127.0.0.1:3000".into(),
            ssh_url_base: None,
            max_git_request_bytes: 2 * 1024 * 1024 * 1024,
            session_ttl_secs: 7 * 24 * 3600,
        }
    }
}

#[derive(Debug)]
pub struct AppState {
    pub db: Db,
    pub repos: RepoStore,
    pub config: ApiConfig,
    pub oidc: Option<Oidc>,
    /// Wakes runners after a push queued new jobs (the runner server's notifier).
    pub ci_wakeup: fornax_ci::Wakeup,
}

/// Build the full HTTP router.
pub fn router(state: Arc<AppState>) -> Router {
    let api = Router::new()
        .route("/health", get(repos::health))
        // auth
        .route("/auth/config", get(oidc::auth_config))
        .route("/auth/oidc/login", get(oidc::login))
        .route("/auth/oidc/callback", get(oidc::callback))
        .route("/auth/logout", post(oidc::logout))
        // current user
        .route("/user", get(repos::current_user))
        .route("/user/repos", post(repos::create_user_repo))
        .route("/user/keys", get(keys::list_keys).post(keys::add_key))
        .route("/user/keys/{id}", delete(keys::delete_key))
        .route("/users/{owner}/repos", get(repos::list_owner_repos))
        // organisations
        .route("/orgs", post(orgs::create_org))
        .route("/orgs/{org}", get(orgs::get_org))
        .route(
            "/orgs/{org}/repos",
            get(repos::list_owner_repos).post(repos::create_org_repo),
        )
        .route("/orgs/{org}/members", get(orgs::list_members))
        .route(
            "/orgs/{org}/members/{user}",
            put(orgs::put_member).delete(orgs::delete_member),
        )
        .route(
            "/orgs/{org}/teams",
            get(orgs::list_teams).post(orgs::create_team),
        )
        .route("/orgs/{org}/teams/{team}", delete(orgs::delete_team))
        .route(
            "/orgs/{org}/teams/{team}/members",
            get(orgs::list_team_members),
        )
        .route(
            "/orgs/{org}/teams/{team}/members/{user}",
            put(orgs::put_team_member).delete(orgs::delete_team_member),
        )
        .route(
            "/orgs/{org}/teams/{team}/repos/{repo}",
            put(orgs::put_team_repo).delete(orgs::delete_team_repo),
        )
        // repositories
        .route(
            "/repos/{owner}/{repo}",
            get(repos::get_repo)
                .patch(repos::update_repo)
                .delete(repos::delete_repo),
        )
        .route(
            "/repos/{owner}/{repo}/collaborators",
            get(repos::list_collaborators),
        )
        .route(
            "/repos/{owner}/{repo}/collaborators/{user}",
            put(repos::put_collaborator).delete(repos::delete_collaborator),
        )
        .route("/repos/{owner}/{repo}/branches", get(repos::list_branches))
        .route("/repos/{owner}/{repo}/commits", get(repos::list_commits))
        .route("/repos/{owner}/{repo}/tree", get(repos::get_tree))
        .route("/repos/{owner}/{repo}/raw", get(repos::get_raw))
        // CI
        .route("/repos/{owner}/{repo}/pipelines", get(ci::list_pipelines))
        .route(
            "/repos/{owner}/{repo}/pipelines/{number}",
            get(ci::get_pipeline),
        )
        .route(
            "/repos/{owner}/{repo}/pipelines/{number}/cancel",
            post(ci::cancel_pipeline),
        )
        .route(
            "/repos/{owner}/{repo}/pipelines/{number}/jobs/{job}/log",
            get(ci::get_job_log),
        )
        .fallback(|| async { error::ApiError::NotFound });

    let git = Router::new()
        .route("/{owner}/{repo}/info/refs", get(githttp::info_refs))
        .route(
            "/{owner}/{repo}/git-upload-pack",
            post(githttp::upload_pack),
        )
        .route(
            "/{owner}/{repo}/git-receive-pack",
            post(githttp::receive_pack),
        );

    Router::new()
        .nest("/api/v1", api)
        .merge(git)
        .fallback(ui::serve)
        .with_state(state)
}
