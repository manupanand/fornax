// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! Request authentication.
//!
//! - Personal access tokens `fnx_<64 hex>` (only the SHA-256 is stored), sent as
//!   `Authorization: Bearer <token>` (API, CLI) or HTTP Basic `<username>:<token>` (git).
//! - Browser sessions: an HttpOnly, SameSite=Lax cookie created by OIDC login. Cookie auth
//!   is CSRF-safe here because state-changing endpoints either take a JSON body (forces a
//!   CORS preflight) or use PUT/PATCH/DELETE, and the server never sends CORS headers.
//! - CI job tokens `fnj_<64 hex>` (D-018), sent by the runner agent as HTTP Basic
//!   `fornax-job:<token>` for the checkout. They are accepted **only** on the git fetch
//!   endpoints (`info/refs?service=git-upload-pack`, `git-upload-pack`) and give
//!   [`Permission::Read`] on exactly the job's repository while its attempt is running
//!   ([`fornax_db::Db::job_token_repo`]). A job is never a [`User`]: handlers that look at
//!   [`MaybeUser::as_ref`] see an anonymous caller; only [`authorize_repo`] called with the
//!   whole [`MaybeUser`] (or a [`Caller::Job`]) honours the job grant.

use std::sync::Arc;

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::{Method, header};
use base64::Engine;
use fornax_core::now_unix;
use fornax_core::perm::Permission;
use fornax_db::{Repo, User};
use sha2::{Digest, Sha256};

use crate::AppState;
use crate::error::ApiError;

pub const TOKEN_PREFIX: &str = "fnx_";
pub const SESSION_COOKIE: &str = "fornax_session";
/// Prefix of CI job tokens (`fnj_` followed by 64 lowercase hex characters).
pub const JOB_TOKEN_PREFIX: &str = "fnj_";
/// HTTP Basic user name that carries a job token.
pub const JOB_TOKEN_USER: &str = "fornax-job";

/// `n` random bytes, hex encoded.
pub fn random_hex(n: usize) -> Result<String, getrandom::Error> {
    let mut bytes = vec![0u8; n];
    getrandom::getrandom(&mut bytes)?;
    Ok(hex::encode(bytes))
}

/// A new random token: `(plain value to show once, hash to store)`.
pub fn generate_token() -> Result<(String, String), getrandom::Error> {
    let token = format!("{TOKEN_PREFIX}{}", random_hex(32)?);
    let hash = hash_token(&token);
    Ok((token, hash))
}

/// SHA-256 (hex) of a secret, used for tokens, session ids and OAuth state.
pub fn hash_token(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

/// A CI job authenticated by its job token: may fetch `repo_id` while `attempt_id` runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JobPrincipal {
    pub attempt_id: i64,
    pub repo_id: i64,
}

/// The authenticated user, if any. Invalid explicit credentials are rejected (401); a stale
/// session cookie just means "anonymous".
///
/// A request authenticated with a job token has no user (`.0` is `None`) and carries a
/// [`JobPrincipal`] instead ([`MaybeUser::job`]).
#[derive(Debug, Clone)]
pub struct MaybeUser(pub Option<User>, Option<JobPrincipal>);

impl MaybeUser {
    /// An anonymous caller.
    pub fn anonymous() -> Self {
        Self(None, None)
    }

    /// A signed-in user.
    pub fn user(user: User) -> Self {
        Self(Some(user), None)
    }

    /// A CI job (job token).
    pub fn job_principal(job: JobPrincipal) -> Self {
        Self(None, Some(job))
    }

    /// The user, or 401. Jobs are not users, so they get 401 here.
    pub fn require(self) -> Result<User, ApiError> {
        self.0.ok_or(ApiError::Unauthorized)
    }

    /// The user, if any (`None` for anonymous callers and jobs).
    pub fn as_ref(&self) -> Option<&User> {
        self.0.as_ref()
    }

    /// The job, if the request used a job token.
    pub fn job(&self) -> Option<&JobPrincipal> {
        self.1.as_ref()
    }
}

/// Who is asking, for [`authorize_repo`].
#[derive(Debug, Clone, Copy)]
pub enum Caller<'a> {
    Anonymous,
    User(&'a User),
    /// A CI job: read access to its own repository only.
    Job(&'a JobPrincipal),
}

impl<'a> Caller<'a> {
    /// The user behind the call (`None` for anonymous callers and jobs).
    pub fn user(self) -> Option<&'a User> {
        match self {
            Self::User(u) => Some(u),
            Self::Anonymous | Self::Job(_) => None,
        }
    }
}

impl<'a> From<Option<&'a User>> for Caller<'a> {
    fn from(user: Option<&'a User>) -> Self {
        user.map_or(Self::Anonymous, Self::User)
    }
}

impl<'a> From<&'a User> for Caller<'a> {
    fn from(user: &'a User) -> Self {
        Self::User(user)
    }
}

impl<'a> From<&'a MaybeUser> for Caller<'a> {
    fn from(m: &'a MaybeUser) -> Self {
        match (&m.0, &m.1) {
            (_, Some(job)) => Self::Job(job),
            (Some(user), None) => Self::User(user),
            (None, None) => Self::Anonymous,
        }
    }
}

fn cookie_value<'a>(parts: &'a Parts, name: &str) -> Option<&'a str> {
    parts
        .headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v)
}

/// `true` for the two git smart-HTTP *fetch* requests, the only ones a job token may make:
/// `GET …/info/refs?service=git-upload-pack` and `POST …/git-upload-pack`.
fn is_git_fetch(parts: &Parts) -> bool {
    let path = parts.uri.path();
    if parts.method == Method::GET && path.ends_with("/info/refs") {
        return parts
            .uri
            .query()
            .is_some_and(|q| q.split('&').any(|kv| kv == "service=git-upload-pack"));
    }
    parts.method == Method::POST && path.ends_with("/git-upload-pack")
}

/// `fnj_` followed by exactly 64 lowercase hex characters.
fn is_job_token_format(token: &str) -> bool {
    token.strip_prefix(JOB_TOKEN_PREFIX).is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

impl FromRequestParts<Arc<AppState>> for MaybeUser {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        let Some(value) = parts.headers.get(header::AUTHORIZATION) else {
            let Some(session) = cookie_value(parts, SESSION_COOKIE) else {
                return Ok(Self::anonymous());
            };
            return Ok(Self(
                state.db.user_by_session(&hash_token(session)).await?,
                None,
            ));
        };
        let value = value.to_str().map_err(|_| ApiError::Unauthorized)?;
        let (scheme, rest) = value.split_once(' ').ok_or(ApiError::Unauthorized)?;
        let (username, token) = match scheme.to_ascii_lowercase().as_str() {
            "bearer" | "token" => (None, rest.trim().to_owned()),
            "basic" => {
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(rest.trim())
                    .map_err(|_| ApiError::Unauthorized)?;
                let decoded = String::from_utf8(decoded).map_err(|_| ApiError::Unauthorized)?;
                let (user, token) = decoded.split_once(':').ok_or(ApiError::Unauthorized)?;
                (Some(user.to_owned()), token.to_owned())
            }
            _ => return Err(ApiError::Unauthorized),
        };
        if token.starts_with(JOB_TOKEN_PREFIX) {
            // Job tokens: HTTP Basic `fornax-job:<token>`, git fetch requests only.
            if username.as_deref() != Some(JOB_TOKEN_USER)
                || !is_job_token_format(&token)
                || !is_git_fetch(parts)
            {
                return Err(ApiError::Unauthorized);
            }
            let (attempt_id, repo_id) = state
                .db
                .job_token_repo(&hash_token(&token), now_unix())
                .await?
                .ok_or(ApiError::Unauthorized)?;
            return Ok(Self::job_principal(JobPrincipal {
                attempt_id,
                repo_id,
            }));
        }
        if !token.starts_with(TOKEN_PREFIX) {
            return Err(ApiError::Unauthorized);
        }
        let user = state
            .db
            .user_by_token_hash(&hash_token(&token))
            .await?
            .ok_or(ApiError::Unauthorized)?;
        if let Some(name) = username
            && !name.eq_ignore_ascii_case(&user.username)
        {
            return Err(ApiError::Unauthorized);
        }
        Ok(Self::user(user))
    }
}

/// Map a missing permission to the right error: anonymous callers get 401 (so clients
/// offer to log in); private repositories stay invisible (404) to users without read
/// access; users who can see but not act get 403.
pub fn deny(user: Option<&User>, have: Permission) -> ApiError {
    match (user, have) {
        (None, _) => ApiError::Unauthorized,
        (Some(_), Permission::None) => ApiError::NotFound,
        (Some(_), _) => ApiError::Forbidden,
    }
}

/// Load a repository and require `needed` permission. Returns the repo and the caller's
/// effective permission.
///
/// `caller` is anything convertible into a [`Caller`]: `Option<&User>`, `&User`, or a whole
/// `&MaybeUser` (which also carries a job principal). A job gets [`Permission::Read`] on its
/// own repository and nothing else: other repositories are 404, more than read is 403.
pub async fn authorize_repo<'a>(
    state: &AppState,
    owner: &str,
    name: &str,
    caller: impl Into<Caller<'a>>,
    needed: Permission,
) -> Result<(Repo, Permission), ApiError> {
    let caller = caller.into();
    let repo = state.db.repo_by_name(owner, name).await?;
    if let Caller::Job(job) = caller {
        let Some(repo) = repo.filter(|r| r.id == job.repo_id) else {
            return Err(ApiError::NotFound);
        };
        if needed > Permission::Read {
            return Err(ApiError::Forbidden);
        }
        return Ok((repo, Permission::Read));
    }
    let user = caller.user();
    let Some(repo) = repo else {
        // Anonymous callers can't tell "missing" from "private".
        return Err(deny(user, Permission::None));
    };
    let have = state.db.permission(user, &repo).await?;
    if have < needed {
        return Err(deny(user, have));
    }
    Ok((repo, have))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_prefixed_and_hashed() {
        let (token, hash) = generate_token().expect("random");
        assert!(token.starts_with(TOKEN_PREFIX));
        assert_eq!(token.len(), TOKEN_PREFIX.len() + 64);
        assert_eq!(hash, hash_token(&token));
        assert_ne!(hash, token);
    }

    #[test]
    fn deny_mapping() {
        let u = User {
            id: 1,
            username: "u".into(),
            is_admin: false,
            is_org: false,
            email: None,
            created_at: 0,
        };
        assert!(matches!(
            deny(None, Permission::Read),
            ApiError::Unauthorized
        ));
        assert!(matches!(
            deny(Some(&u), Permission::None),
            ApiError::NotFound
        ));
        assert!(matches!(
            deny(Some(&u), Permission::Read),
            ApiError::Forbidden
        ));
    }

    #[test]
    fn parses_cookies() {
        let req = axum::http::Request::builder()
            .header(header::COOKIE, "a=1; fornax_session=abc; b=2")
            .body(())
            .expect("req");
        let (parts, ()) = req.into_parts();
        assert_eq!(cookie_value(&parts, SESSION_COOKIE), Some("abc"));
        assert_eq!(cookie_value(&parts, "zzz"), None);
    }

    fn parts(method: &str, uri: &str) -> Parts {
        let req = axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .body(())
            .expect("req");
        req.into_parts().0
    }

    #[test]
    fn git_fetch_detection() {
        assert!(is_git_fetch(&parts(
            "GET",
            "/a/b.git/info/refs?service=git-upload-pack"
        )));
        assert!(is_git_fetch(&parts("POST", "/a/b.git/git-upload-pack")));
        assert!(!is_git_fetch(&parts(
            "GET",
            "/a/b.git/info/refs?service=git-receive-pack"
        )));
        assert!(!is_git_fetch(&parts("GET", "/a/b.git/info/refs")));
        assert!(!is_git_fetch(&parts(
            "GET",
            "/a/b.git/info/refs?service=git-upload-packx"
        )));
        assert!(!is_git_fetch(&parts("POST", "/a/b.git/git-receive-pack")));
        assert!(!is_git_fetch(&parts("GET", "/a/b.git/git-upload-pack")));
        assert!(!is_git_fetch(&parts("GET", "/api/v1/repos/a/b")));
    }

    #[test]
    fn job_token_format() {
        assert!(is_job_token_format(&format!("fnj_{}", "0a".repeat(32))));
        assert!(!is_job_token_format(&format!("fnj_{}", "0A".repeat(32))));
        assert!(!is_job_token_format("fnj_abc"));
        assert!(!is_job_token_format(&format!("fnx_{}", "0a".repeat(32))));
    }

    #[test]
    fn caller_conversion() {
        let u = User {
            id: 1,
            username: "u".into(),
            is_admin: true,
            is_org: false,
            email: None,
            created_at: 0,
        };
        let job = JobPrincipal {
            attempt_id: 7,
            repo_id: 3,
        };
        assert!(matches!(
            Caller::from(&MaybeUser::anonymous()),
            Caller::Anonymous
        ));
        assert!(matches!(
            Caller::from(&MaybeUser::user(u.clone())),
            Caller::User(_)
        ));
        let m = MaybeUser::job_principal(job);
        assert!(matches!(Caller::from(&m), Caller::Job(j) if *j == job));
        assert!(m.as_ref().is_none());
        assert_eq!(m.job(), Some(&job));
        assert!(m.require().is_err());
        assert!(Caller::Job(&job).user().is_none());
        assert!(Caller::from(Some(&u)).user().is_some());
    }
}
