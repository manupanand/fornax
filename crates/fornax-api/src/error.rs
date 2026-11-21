// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

use axum::Json;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use fornax_db::DbError;
use fornax_git::GitError;
use serde_json::json;

/// Errors returned by handlers, rendered as `{"error": {"code", "message"}}`.
#[derive(Debug)]
pub enum ApiError {
    /// No or invalid credentials. Carries `WWW-Authenticate` so `git` prompts for them.
    Unauthorized,
    Forbidden,
    NotFound,
    BadRequest(String),
    Conflict(String),
    Internal(String),
}

impl ApiError {
    fn parts(&self) -> (StatusCode, &'static str, String) {
        match self {
            Self::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "authentication required".into(),
            ),
            Self::Forbidden => (
                StatusCode::FORBIDDEN,
                "forbidden",
                "permission denied".into(),
            ),
            Self::NotFound => (StatusCode::NOT_FOUND, "not_found", "not found".into()),
            Self::BadRequest(m) => (StatusCode::BAD_REQUEST, "bad_request", m.clone()),
            Self::Conflict(m) => (StatusCode::CONFLICT, "conflict", m.clone()),
            Self::Internal(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "internal error".into(),
            ),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        if let Self::Internal(detail) = &self {
            tracing::error!(%detail, "internal error");
        }
        let (status, code, message) = self.parts();
        let mut res = (
            status,
            Json(json!({ "error": { "code": code, "message": message } })),
        )
            .into_response();
        if status == StatusCode::UNAUTHORIZED {
            res.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                HeaderValue::from_static("Basic realm=\"fornax\""),
            );
        }
        res
    }
}

impl From<DbError> for ApiError {
    fn from(e: DbError) -> Self {
        match e {
            DbError::Conflict(what) => Self::Conflict(format!("{what} already exists")),
            DbError::NotFound(_) => Self::NotFound,
            other => Self::Internal(other.to_string()),
        }
    }
}

impl From<GitError> for ApiError {
    fn from(e: GitError) -> Self {
        match e {
            GitError::NotFound(_) => Self::NotFound,
            GitError::InvalidInput(m) => Self::BadRequest(m),
            other => Self::Internal(other.to_string()),
        }
    }
}

impl From<fornax_core::ValidationError> for ApiError {
    fn from(e: fornax_core::ValidationError) -> Self {
        Self::BadRequest(e.to_string())
    }
}

pub type ApiResult<T> = Result<T, ApiError>;
