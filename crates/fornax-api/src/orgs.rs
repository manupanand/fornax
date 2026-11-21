// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! Organisations, members and teams (`/api/v1/orgs`).
//!
//! Owners manage everything; members can see the organisation, its members and teams, and
//! read every organisation repository. Teams grant read/write/admin on chosen repositories.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use fornax_core::names::{validate_repo_name, validate_username};
use fornax_core::perm::{OrgRole, Permission};
use fornax_db::{Member, Team, User};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::AppState;
use crate::auth::MaybeUser;
use crate::error::{ApiError, ApiResult};
use crate::repos::{load_org, require_role};

#[derive(Debug, Serialize)]
pub struct OrgView {
    pub id: i64,
    pub name: String,
    pub created_at: i64,
    /// Caller's role, if a member.
    pub role: Option<OrgRole>,
}

fn view(org: User, role: Option<OrgRole>) -> OrgView {
    OrgView {
        id: org.id,
        name: org.username,
        created_at: org.created_at,
        role,
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateOrg {
    pub name: String,
}

/// `POST /api/v1/orgs` — the caller becomes the first owner.
pub async fn create_org(
    State(state): State<Arc<AppState>>,
    user: MaybeUser,
    Json(body): Json<CreateOrg>,
) -> ApiResult<(StatusCode, Json<OrgView>)> {
    let user = user.require()?;
    validate_username(&body.name)?;
    let org = state.db.create_org(&body.name, user.id).await?;
    state
        .db
        .audit(Some(user.id), "org.create", &org.username, "{}")
        .await?;
    Ok((StatusCode::CREATED, Json(view(org, Some(OrgRole::Owner)))))
}

/// `GET /api/v1/orgs/{org}` — organisations are publicly listed (like GitHub).
pub async fn get_org(
    State(state): State<Arc<AppState>>,
    user: MaybeUser,
    Path(org): Path<String>,
) -> ApiResult<Json<OrgView>> {
    let (org, role) = load_org(&state, &org, user.as_ref()).await?;
    Ok(Json(view(org, role)))
}

// ---- members -----------------------------------------------------------------------------

pub async fn list_members(
    State(state): State<Arc<AppState>>,
    user: MaybeUser,
    Path(org): Path<String>,
) -> ApiResult<Json<Vec<Member>>> {
    let (org, role) = load_org(&state, &org, user.as_ref()).await?;
    require_role(user.as_ref(), role, OrgRole::Member)?;
    Ok(Json(state.db.org_members(org.id).await?))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetRole {
    pub role: String,
}

/// `PUT /api/v1/orgs/{org}/members/{user}` `{"role": "member"|"owner"}`
pub async fn put_member(
    State(state): State<Arc<AppState>>,
    user: MaybeUser,
    Path((org, target)): Path<(String, String)>,
    Json(body): Json<SetRole>,
) -> ApiResult<StatusCode> {
    let actor = user.require()?;
    let (org, role) = load_org(&state, &org, Some(&actor)).await?;
    require_role(Some(&actor), role, OrgRole::Owner)?;
    let new_role: OrgRole = body.role.parse()?;
    let target = find_user(&state, &target).await?;
    if new_role == OrgRole::Member
        && state.db.org_role(org.id, target.id).await? == Some(OrgRole::Owner)
        && state.db.org_owner_count(org.id).await? <= 1
    {
        return Err(ApiError::Conflict(
            "an organisation needs at least one owner".into(),
        ));
    }
    state.db.set_org_member(org.id, target.id, new_role).await?;
    state
        .db
        .audit(
            Some(actor.id),
            "org.member.set",
            &org.username,
            &json!({ "user": target.username, "role": new_role.as_str() }).to_string(),
        )
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn delete_member(
    State(state): State<Arc<AppState>>,
    user: MaybeUser,
    Path((org, target)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    let actor = user.require()?;
    let (org, role) = load_org(&state, &org, Some(&actor)).await?;
    require_role(Some(&actor), role, OrgRole::Owner)?;
    let target = find_user(&state, &target).await?;
    if state.db.org_role(org.id, target.id).await? == Some(OrgRole::Owner)
        && state.db.org_owner_count(org.id).await? <= 1
    {
        return Err(ApiError::Conflict("cannot remove the last owner".into()));
    }
    if !state.db.remove_org_member(org.id, target.id).await? {
        return Err(ApiError::NotFound);
    }
    state
        .db
        .audit(
            Some(actor.id),
            "org.member.remove",
            &org.username,
            &json!({ "user": target.username }).to_string(),
        )
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn find_user(state: &AppState, name: &str) -> ApiResult<User> {
    state
        .db
        .user_by_name(name)
        .await?
        .filter(|u| !u.is_org)
        .ok_or(ApiError::NotFound)
}

// ---- teams -------------------------------------------------------------------------------

pub async fn list_teams(
    State(state): State<Arc<AppState>>,
    user: MaybeUser,
    Path(org): Path<String>,
) -> ApiResult<Json<Vec<Team>>> {
    let (org, role) = load_org(&state, &org, user.as_ref()).await?;
    require_role(user.as_ref(), role, OrgRole::Member)?;
    Ok(Json(state.db.teams(org.id).await?))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateTeam {
    pub name: String,
    pub permission: String,
}

pub async fn create_team(
    State(state): State<Arc<AppState>>,
    user: MaybeUser,
    Path(org): Path<String>,
    Json(body): Json<CreateTeam>,
) -> ApiResult<(StatusCode, Json<Team>)> {
    let actor = user.require()?;
    let (org, role) = load_org(&state, &org, Some(&actor)).await?;
    require_role(Some(&actor), role, OrgRole::Owner)?;
    validate_repo_name(&body.name)?;
    let permission = Permission::parse_grant(&body.permission)?;
    let team = state.db.create_team(org.id, &body.name, permission).await?;
    state
        .db
        .audit(
            Some(actor.id),
            "org.team.create",
            &format!("{}/{}", org.username, team.name),
            &json!({ "permission": permission.as_str() }).to_string(),
        )
        .await?;
    Ok((StatusCode::CREATED, Json(team)))
}

async fn load_team(
    state: &AppState,
    org: &str,
    team: &str,
    user: Option<&User>,
    needed: OrgRole,
) -> ApiResult<(User, Team)> {
    let (org, role) = load_org(state, org, user).await?;
    require_role(user, role, needed)?;
    let team = state
        .db
        .team_by_name(org.id, team)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok((org, team))
}

pub async fn delete_team(
    State(state): State<Arc<AppState>>,
    user: MaybeUser,
    Path((org, team)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    let actor = user.require()?;
    let (org, team) = load_team(&state, &org, &team, Some(&actor), OrgRole::Owner).await?;
    state.db.delete_team(team.id).await?;
    state
        .db
        .audit(
            Some(actor.id),
            "org.team.delete",
            &format!("{}/{}", org.username, team.name),
            "{}",
        )
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn list_team_members(
    State(state): State<Arc<AppState>>,
    user: MaybeUser,
    Path((org, team)): Path<(String, String)>,
) -> ApiResult<Json<Vec<String>>> {
    let (_, team) = load_team(&state, &org, &team, user.as_ref(), OrgRole::Member).await?;
    Ok(Json(state.db.team_members(team.id).await?))
}

/// `PUT /api/v1/orgs/{org}/teams/{team}/members/{user}` — the user must be an org member.
pub async fn put_team_member(
    State(state): State<Arc<AppState>>,
    user: MaybeUser,
    Path((org, team, target)): Path<(String, String, String)>,
) -> ApiResult<StatusCode> {
    let actor = user.require()?;
    let (org, team) = load_team(&state, &org, &team, Some(&actor), OrgRole::Owner).await?;
    let target = find_user(&state, &target).await?;
    if state.db.org_role(org.id, target.id).await?.is_none() {
        return Err(ApiError::BadRequest(format!(
            "{} is not a member of {}",
            target.username, org.username
        )));
    }
    state.db.add_team_member(team.id, target.id).await?;
    state
        .db
        .audit(
            Some(actor.id),
            "org.team.member.add",
            &format!("{}/{}", org.username, team.name),
            &json!({ "user": target.username }).to_string(),
        )
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn delete_team_member(
    State(state): State<Arc<AppState>>,
    user: MaybeUser,
    Path((org, team, target)): Path<(String, String, String)>,
) -> ApiResult<StatusCode> {
    let actor = user.require()?;
    let (org, team) = load_team(&state, &org, &team, Some(&actor), OrgRole::Owner).await?;
    let target = find_user(&state, &target).await?;
    state.db.remove_team_member(team.id, target.id).await?;
    state
        .db
        .audit(
            Some(actor.id),
            "org.team.member.remove",
            &format!("{}/{}", org.username, team.name),
            &json!({ "user": target.username }).to_string(),
        )
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `PUT /api/v1/orgs/{org}/teams/{team}/repos/{repo}` — grant the team its permission on an
/// organisation repository.
pub async fn put_team_repo(
    State(state): State<Arc<AppState>>,
    user: MaybeUser,
    Path((org, team, repo)): Path<(String, String, String)>,
) -> ApiResult<StatusCode> {
    let actor = user.require()?;
    let (org, team) = load_team(&state, &org, &team, Some(&actor), OrgRole::Owner).await?;
    let repo = state
        .db
        .repo_by_name(&org.username, &repo)
        .await?
        .ok_or(ApiError::NotFound)?;
    state.db.add_team_repo(team.id, repo.id).await?;
    state
        .db
        .audit(
            Some(actor.id),
            "org.team.repo.add",
            &format!("{}/{}", org.username, team.name),
            &json!({ "repo": repo.full_name() }).to_string(),
        )
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn delete_team_repo(
    State(state): State<Arc<AppState>>,
    user: MaybeUser,
    Path((org, team, repo)): Path<(String, String, String)>,
) -> ApiResult<StatusCode> {
    let actor = user.require()?;
    let (org, team) = load_team(&state, &org, &team, Some(&actor), OrgRole::Owner).await?;
    let repo = state
        .db
        .repo_by_name(&org.username, &repo)
        .await?
        .ok_or(ApiError::NotFound)?;
    state.db.remove_team_repo(team.id, repo.id).await?;
    state
        .db
        .audit(
            Some(actor.id),
            "org.team.repo.remove",
            &format!("{}/{}", org.username, team.name),
            &json!({ "repo": repo.full_name() }).to_string(),
        )
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
