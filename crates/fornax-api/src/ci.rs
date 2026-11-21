// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! Read API for CI results (`/api/v1/repos/{owner}/{repo}/pipelines…`): list pipelines,
//! one pipeline with its jobs and attempts, raw job logs, and cancellation.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use fornax_core::perm::Permission;
use fornax_db::{Attempt, Job, Pipeline};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::AppState;
use crate::auth::{MaybeUser, authorize_repo};
use crate::error::{ApiError, ApiResult};

const MAX_LOG_READ: usize = 1024 * 1024;

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub limit: Option<i64>,
}

/// `GET /api/v1/repos/{owner}/{repo}/pipelines` — newest first.
pub async fn list_pipelines(
    State(state): State<Arc<AppState>>,
    user: MaybeUser,
    Path((owner, name)): Path<(String, String)>,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<Vec<Pipeline>>> {
    let (repo, _) = authorize_repo(&state, &owner, &name, user.as_ref(), Permission::Read).await?;
    let limit = q.limit.unwrap_or(30).clamp(1, 200);
    Ok(Json(state.db.pipelines_for_repo(repo.id, limit).await?))
}

#[derive(Debug, Serialize)]
pub struct JobView {
    #[serde(flatten)]
    pub job: Job,
    pub attempts: Vec<Attempt>,
}

#[derive(Debug, Serialize)]
pub struct PipelineView {
    #[serde(flatten)]
    pub pipeline: Pipeline,
    pub jobs: Vec<JobView>,
    pub needs: Vec<(i64, i64)>,
}

async fn load_pipeline(
    state: &AppState,
    owner: &str,
    name: &str,
    number: i64,
    user: Option<&fornax_db::User>,
    needed: Permission,
) -> ApiResult<Pipeline> {
    let (repo, _) = authorize_repo(state, owner, name, user, needed).await?;
    state
        .db
        .pipeline_by_number(repo.id, number)
        .await?
        .ok_or(ApiError::NotFound)
}

/// `GET /api/v1/repos/{owner}/{repo}/pipelines/{number}` — pipeline, jobs, attempts.
pub async fn get_pipeline(
    State(state): State<Arc<AppState>>,
    user: MaybeUser,
    Path((owner, name, number)): Path<(String, String, i64)>,
) -> ApiResult<Json<PipelineView>> {
    let pipeline = load_pipeline(
        &state,
        &owner,
        &name,
        number,
        user.as_ref(),
        Permission::Read,
    )
    .await?;
    let mut jobs = Vec::new();
    for mut job in state.db.jobs_for_pipeline(pipeline.id).await? {
        let attempts = state.db.attempts_for_job(job.id).await?;
        // The payload is internal (steps, env); it is not part of the public view.
        job.payload = String::new();
        jobs.push(JobView { job, attempts });
    }
    let needs = state.db.job_needs(pipeline.id).await?;
    Ok(Json(PipelineView {
        pipeline,
        jobs,
        needs,
    }))
}

#[derive(Debug, Deserialize)]
pub struct LogQuery {
    /// Attempt number (1-based); defaults to the latest attempt.
    pub attempt: Option<i64>,
    /// Byte offset to read from (for incremental tailing).
    pub from: Option<u64>,
}

/// `GET /api/v1/repos/{owner}/{repo}/pipelines/{number}/jobs/{job}/log` — raw log bytes
/// (already masked by the runner). `X-Fornax-Next-Offset` tells the client where to resume.
pub async fn get_job_log(
    State(state): State<Arc<AppState>>,
    user: MaybeUser,
    Path((owner, name, number, job_id)): Path<(String, String, i64, i64)>,
    Query(q): Query<LogQuery>,
) -> ApiResult<Response> {
    let pipeline = load_pipeline(
        &state,
        &owner,
        &name,
        number,
        user.as_ref(),
        Permission::Read,
    )
    .await?;
    let job = state
        .db
        .job(job_id)
        .await?
        .filter(|j| j.pipeline_id == pipeline.id)
        .ok_or(ApiError::NotFound)?;
    let attempts = state.db.attempts_for_job(job.id).await?;
    let attempt = match q.attempt {
        Some(n) => attempts.into_iter().find(|a| a.number == n),
        None => attempts.into_iter().max_by_key(|a| a.number),
    };
    let (data, next) = match attempt {
        Some(a) => {
            state
                .db
                .read_log(a.id, q.from.unwrap_or(0), MAX_LOG_READ)
                .await?
        }
        None => (Vec::new(), 0),
    };
    let next =
        HeaderValue::from_str(&next.to_string()).map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok((
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/plain; charset=utf-8"),
            ),
            (
                header::X_CONTENT_TYPE_OPTIONS,
                HeaderValue::from_static("nosniff"),
            ),
            (HeaderName::from_static("x-fornax-next-offset"), next),
        ],
        data,
    )
        .into_response())
}

/// `POST /api/v1/repos/{owner}/{repo}/pipelines/{number}/cancel` (write permission).
///
/// Takes a JSON body (`{}`), so a cross-site form or a `fetch` without CORS preflight cannot
/// trigger it with the user's session cookie (CSRF model of D-031).
pub async fn cancel_pipeline(
    State(state): State<Arc<AppState>>,
    user: MaybeUser,
    Path((owner, name, number)): Path<(String, String, i64)>,
    Json(_body): Json<serde::de::IgnoredAny>,
) -> ApiResult<(StatusCode, Json<serde_json::Value>)> {
    let user = user.require()?;
    let pipeline = load_pipeline(
        &state,
        &owner,
        &name,
        number,
        Some(&user),
        Permission::Write,
    )
    .await?;
    let outcome = state
        .db
        .cancel_pipeline(pipeline.id, fornax_core::now_unix())
        .await?;
    state
        .db
        .audit(
            Some(user.id),
            "pipeline.cancel",
            &format!("{owner}/{name}#{number}"),
            &json!({ "canceled_jobs": outcome.canceled_jobs, "running_jobs": outcome.running_jobs })
                .to_string(),
        )
        .await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(
            json!({ "canceled_jobs": outcome.canceled_jobs, "running_jobs": outcome.running_jobs }),
        ),
    ))
}
