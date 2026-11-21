// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! REST handlers for repositories (`/api/v1`): create, read, update, delete, collaborators,
//! and browsing (branches, commits, tree, raw).

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use fornax_core::names::validate_repo_name;
use fornax_core::perm::{OrgRole, Permission};
use fornax_core::refname::validate_branch_name;
use fornax_db::{Collaborator, NewRepo, Repo, RepoUpdate, User};
use fornax_git::{Branch, Commit, TreeEntry};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::AppState;
use crate::auth::{MaybeUser, authorize_repo, deny};
use crate::error::{ApiError, ApiResult};

/// Largest file served by the raw endpoint.
const MAX_RAW_BYTES: u64 = 10 * 1024 * 1024;
const MAX_DESCRIPTION: usize = 1000;

#[derive(Debug, Serialize)]
pub struct RepoView {
    pub id: i64,
    pub owner: String,
    pub owner_kind: &'static str,
    pub name: String,
    pub full_name: String,
    pub description: String,
    pub private: bool,
    pub default_branch: String,
    pub clone_url: String,
    pub ssh_url: Option<String>,
    /// The caller's effective permission.
    pub permission: Permission,
    pub created_at: i64,
    pub updated_at: i64,
}

impl RepoView {
    pub(crate) fn new(repo: Repo, permission: Permission, state: &AppState) -> Self {
        let full_name = repo.full_name();
        Self {
            clone_url: format!(
                "{}/{full_name}.git",
                state.config.public_url.trim_end_matches('/')
            ),
            ssh_url: state
                .config
                .ssh_url_base
                .as_ref()
                .map(|base| format!("{base}{full_name}.git")),
            id: repo.id,
            owner: repo.owner,
            owner_kind: if repo.owner_is_org { "org" } else { "user" },
            name: repo.name,
            full_name,
            description: repo.description,
            private: repo.is_private,
            default_branch: repo.default_branch,
            permission,
            created_at: repo.created_at,
            updated_at: repo.updated_at,
        }
    }
}

pub async fn health() -> Json<serde_json::Value> {
    Json(json!({ "status": "ok", "version": env!("CARGO_PKG_VERSION") }))
}

pub async fn current_user(user: MaybeUser) -> ApiResult<Json<User>> {
    Ok(Json(user.require()?))
}

// ---- create ------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateRepo {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub private: bool,
    pub default_branch: Option<String>,
}

/// `POST /api/v1/user/repos` — repository owned by the caller.
pub async fn create_user_repo(
    State(state): State<Arc<AppState>>,
    user: MaybeUser,
    Json(body): Json<CreateRepo>,
) -> ApiResult<(StatusCode, Json<RepoView>)> {
    let user = user.require()?;
    create_repo_for(&state, &user, &user, body).await
}

/// `POST /api/v1/orgs/{org}/repos` — repository owned by an organisation (org owners).
pub async fn create_org_repo(
    State(state): State<Arc<AppState>>,
    user: MaybeUser,
    Path(org): Path<String>,
    Json(body): Json<CreateRepo>,
) -> ApiResult<(StatusCode, Json<RepoView>)> {
    let user = user.require()?;
    let org = state
        .db
        .user_by_name(&org)
        .await?
        .filter(|o| o.is_org)
        .ok_or(ApiError::NotFound)?;
    let role = state.db.org_role(org.id, user.id).await?;
    if !user.is_admin && role != Some(OrgRole::Owner) {
        return Err(if role.is_some() {
            ApiError::Forbidden
        } else {
            ApiError::NotFound
        });
    }
    create_repo_for(&state, &user, &org, body).await
}

async fn create_repo_for(
    state: &AppState,
    actor: &User,
    owner: &User,
    body: CreateRepo,
) -> ApiResult<(StatusCode, Json<RepoView>)> {
    validate_repo_name(&body.name)?;
    let default_branch = body.default_branch.as_deref().unwrap_or("main");
    validate_branch_name(default_branch)?;
    if body.description.len() > MAX_DESCRIPTION {
        return Err(ApiError::BadRequest(
            "description: must be at most 1000 bytes".into(),
        ));
    }

    // Row first (enforces uniqueness), then disk; roll the row back if git fails.
    let repo = state
        .db
        .create_repo(&NewRepo {
            owner_id: owner.id,
            name: &body.name,
            description: &body.description,
            is_private: body.private,
            default_branch,
        })
        .await?;
    let (owner_key, name_key) = repo.storage_key();
    if let Err(e) = state
        .repos
        .create(&owner_key, &name_key, default_branch)
        .await
    {
        state.db.delete_repo(repo.id).await?;
        return Err(e.into());
    }
    state
        .db
        .audit(
            Some(actor.id),
            "repo.create",
            &repo.full_name(),
            &json!({ "private": repo.is_private }).to_string(),
        )
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(RepoView::new(repo, Permission::Admin, state)),
    ))
}

// ---- read / list -------------------------------------------------------------------------

/// `GET /api/v1/users/{owner}/repos` and `/orgs/{org}/repos` — repositories the caller
/// can see.
pub async fn list_owner_repos(
    State(state): State<Arc<AppState>>,
    user: MaybeUser,
    Path(owner): Path<String>,
) -> ApiResult<Json<Vec<RepoView>>> {
    let owner = state
        .db
        .user_by_name(&owner)
        .await?
        .ok_or(ApiError::NotFound)?;
    let mut out = Vec::new();
    for repo in state.db.repos_by_owner(owner.id).await? {
        let perm = state.db.permission(user.as_ref(), &repo).await?;
        if perm >= Permission::Read {
            out.push(RepoView::new(repo, perm, &state));
        }
    }
    Ok(Json(out))
}

pub async fn get_repo(
    State(state): State<Arc<AppState>>,
    user: MaybeUser,
    Path((owner, name)): Path<(String, String)>,
) -> ApiResult<Json<RepoView>> {
    let (repo, perm) =
        authorize_repo(&state, &owner, &name, user.as_ref(), Permission::Read).await?;
    Ok(Json(RepoView::new(repo, perm, &state)))
}

// ---- update / delete ---------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateRepo {
    pub name: Option<String>,
    pub description: Option<String>,
    pub private: Option<bool>,
    pub default_branch: Option<String>,
}

/// `PATCH /api/v1/repos/{owner}/{repo}` — rename, describe, change visibility or default
/// branch (admin permission).
pub async fn update_repo(
    State(state): State<Arc<AppState>>,
    user: MaybeUser,
    Path((owner, name)): Path<(String, String)>,
    Json(body): Json<UpdateRepo>,
) -> ApiResult<Json<RepoView>> {
    let user = user.require()?;
    let (repo, perm) =
        authorize_repo(&state, &owner, &name, Some(&user), Permission::Admin).await?;
    if let Some(n) = &body.name {
        validate_repo_name(n)?;
    }
    if let Some(d) = &body.description
        && d.len() > MAX_DESCRIPTION
    {
        return Err(ApiError::BadRequest(
            "description: must be at most 1000 bytes".into(),
        ));
    }
    let (owner_key, old_key) = repo.storage_key();
    let mut path = state.repos.repo_path(&owner_key, &old_key);
    if let Some(b) = &body.default_branch {
        validate_branch_name(b)?;
        let branches = state.repos.branches(&path).await?;
        if !branches.is_empty() && !branches.iter().any(|x| &x.name == b) {
            return Err(ApiError::BadRequest(format!(
                "default_branch: branch {b} does not exist"
            )));
        }
    }

    // Disk rename first (fails cleanly if the target exists), DB second, undo on DB error.
    let renaming = body
        .name
        .as_ref()
        .is_some_and(|n| !n.eq_ignore_ascii_case(&repo.name));
    if renaming && let Some(n) = &body.name {
        path = state.repos.rename(&owner_key, &old_key, n).await?;
    }
    let updated = state
        .db
        .update_repo(
            repo.id,
            &RepoUpdate {
                name: body.name.as_deref(),
                description: body.description.as_deref(),
                is_private: body.private,
                default_branch: body.default_branch.as_deref(),
            },
        )
        .await;
    let updated = match updated {
        Ok(r) => r,
        Err(e) => {
            if renaming && let Some(n) = &body.name {
                let _ = state.repos.rename(&owner_key, n, &old_key).await;
            }
            return Err(e.into());
        }
    };
    if let Some(b) = &body.default_branch {
        state.repos.set_head(&path, b).await?;
    }
    state
        .db
        .audit(
            Some(user.id),
            "repo.update",
            &updated.full_name(),
            &json!({
                "from": repo.full_name(),
                "private": updated.is_private,
                "default_branch": updated.default_branch,
            })
            .to_string(),
        )
        .await?;
    Ok(Json(RepoView::new(updated, perm, &state)))
}

/// `DELETE /api/v1/repos/{owner}/{repo}` (admin permission).
pub async fn delete_repo(
    State(state): State<Arc<AppState>>,
    user: MaybeUser,
    Path((owner, name)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    let user = user.require()?;
    let (repo, _) = authorize_repo(&state, &owner, &name, Some(&user), Permission::Admin).await?;
    state.db.delete_repo(repo.id).await?;
    let (o, n) = repo.storage_key();
    if let Err(e) = state.repos.remove(&o, &n).await {
        tracing::error!(error = %e, repo = %repo.full_name(), "repository row deleted but files remain");
    }
    state
        .db
        .audit(Some(user.id), "repo.delete", &repo.full_name(), "{}")
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---- collaborators -----------------------------------------------------------------------

pub async fn list_collaborators(
    State(state): State<Arc<AppState>>,
    user: MaybeUser,
    Path((owner, name)): Path<(String, String)>,
) -> ApiResult<Json<Vec<Collaborator>>> {
    let (repo, _) = authorize_repo(&state, &owner, &name, user.as_ref(), Permission::Admin).await?;
    Ok(Json(state.db.collaborators(repo.id).await?))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Grant {
    pub permission: String,
}

/// `PUT /api/v1/repos/{owner}/{repo}/collaborators/{user}` `{"permission": "write"}`
pub async fn put_collaborator(
    State(state): State<Arc<AppState>>,
    user: MaybeUser,
    Path((owner, name, target)): Path<(String, String, String)>,
    Json(body): Json<Grant>,
) -> ApiResult<StatusCode> {
    let user = user.require()?;
    let (repo, _) = authorize_repo(&state, &owner, &name, Some(&user), Permission::Admin).await?;
    let permission = Permission::parse_grant(&body.permission)?;
    let target = state
        .db
        .user_by_name(&target)
        .await?
        .filter(|u| !u.is_org)
        .ok_or(ApiError::NotFound)?;
    state
        .db
        .set_collaborator(repo.id, target.id, permission)
        .await?;
    state
        .db
        .audit(
            Some(user.id),
            "repo.collaborator.set",
            &repo.full_name(),
            &json!({ "user": target.username, "permission": permission.as_str() }).to_string(),
        )
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn delete_collaborator(
    State(state): State<Arc<AppState>>,
    user: MaybeUser,
    Path((owner, name, target)): Path<(String, String, String)>,
) -> ApiResult<StatusCode> {
    let user = user.require()?;
    let (repo, _) = authorize_repo(&state, &owner, &name, Some(&user), Permission::Admin).await?;
    let target = state
        .db
        .user_by_name(&target)
        .await?
        .ok_or(ApiError::NotFound)?;
    if !state.db.remove_collaborator(repo.id, target.id).await? {
        return Err(ApiError::NotFound);
    }
    state
        .db
        .audit(
            Some(user.id),
            "repo.collaborator.remove",
            &repo.full_name(),
            &json!({ "user": target.username }).to_string(),
        )
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---- browse ------------------------------------------------------------------------------

pub async fn list_branches(
    State(state): State<Arc<AppState>>,
    user: MaybeUser,
    Path((owner, name)): Path<(String, String)>,
) -> ApiResult<Json<Vec<Branch>>> {
    let (repo, _) = authorize_repo(&state, &owner, &name, user.as_ref(), Permission::Read).await?;
    let (o, n) = repo.storage_key();
    Ok(Json(
        state.repos.branches(&state.repos.repo_path(&o, &n)).await?,
    ))
}

#[derive(Debug, Deserialize)]
pub struct CommitsQuery {
    #[serde(rename = "ref")]
    pub rev: Option<String>,
    pub limit: Option<usize>,
}

pub async fn list_commits(
    State(state): State<Arc<AppState>>,
    user: MaybeUser,
    Path((owner, name)): Path<(String, String)>,
    Query(q): Query<CommitsQuery>,
) -> ApiResult<Json<Vec<Commit>>> {
    let (repo, _) = authorize_repo(&state, &owner, &name, user.as_ref(), Permission::Read).await?;
    let rev = q.rev.unwrap_or_else(|| repo.default_branch.clone());
    let (o, n) = repo.storage_key();
    let commits = state
        .repos
        .commits(&state.repos.repo_path(&o, &n), &rev, q.limit.unwrap_or(50))
        .await?;
    Ok(Json(commits))
}

#[derive(Debug, Deserialize)]
pub struct PathQuery {
    #[serde(rename = "ref")]
    pub rev: Option<String>,
    #[serde(default)]
    pub path: String,
}

pub async fn get_tree(
    State(state): State<Arc<AppState>>,
    user: MaybeUser,
    Path((owner, name)): Path<(String, String)>,
    Query(q): Query<PathQuery>,
) -> ApiResult<Json<Vec<TreeEntry>>> {
    let (repo, _) = authorize_repo(&state, &owner, &name, user.as_ref(), Permission::Read).await?;
    let rev = q.rev.unwrap_or_else(|| repo.default_branch.clone());
    let (o, n) = repo.storage_key();
    Ok(Json(
        state
            .repos
            .tree(&state.repos.repo_path(&o, &n), &rev, &q.path)
            .await?,
    ))
}

pub async fn get_raw(
    State(state): State<Arc<AppState>>,
    user: MaybeUser,
    Path((owner, name)): Path<(String, String)>,
    Query(q): Query<PathQuery>,
) -> ApiResult<Response> {
    let (repo, _) = authorize_repo(&state, &owner, &name, user.as_ref(), Permission::Read).await?;
    let rev = q.rev.unwrap_or_else(|| repo.default_branch.clone());
    let (o, n) = repo.storage_key();
    let bytes = state
        .repos
        .blob(&state.repos.repo_path(&o, &n), &rev, &q.path, MAX_RAW_BYTES)
        .await?;
    // Always served as an opaque download type so repository content can't run as HTML.
    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'none'; sandbox",
            ),
        ],
        bytes,
    )
        .into_response())
}

/// Helper for handlers that need an organisation the caller belongs to.
pub(crate) async fn load_org(
    state: &AppState,
    name: &str,
    user: Option<&User>,
) -> ApiResult<(User, Option<OrgRole>)> {
    let org = state
        .db
        .user_by_name(name)
        .await?
        .filter(|o| o.is_org)
        .ok_or(ApiError::NotFound)?;
    let role = match user {
        Some(u) if u.is_admin => Some(OrgRole::Owner),
        Some(u) => state.db.org_role(org.id, u.id).await?,
        None => None,
    };
    Ok((org, role))
}

/// Require organisation membership (`Member`) or ownership (`Owner`).
pub(crate) fn require_role(
    user: Option<&User>,
    role: Option<OrgRole>,
    needed: OrgRole,
) -> ApiResult<()> {
    match (role, needed) {
        (Some(OrgRole::Owner), _) | (Some(OrgRole::Member), OrgRole::Member) => Ok(()),
        (Some(OrgRole::Member), OrgRole::Owner) => Err(ApiError::Forbidden),
        (None, _) => Err(deny(user, Permission::None)),
    }
}
