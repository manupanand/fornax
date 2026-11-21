// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! OpenID Connect login (authorisation code flow + PKCE) and browser sessions (H-013).
//!
//! `GET /api/v1/auth/oidc/login?return_to=/x` → provider → `GET /api/v1/auth/oidc/callback`
//! → session cookie → redirect to `return_to`. ID tokens are verified (signature via the
//! provider's JWKS, or HS256 with the client secret; issuer, audience, expiry, nonce).

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use base64::Engine;
use fornax_core::names::validate_username;
use fornax_db::{IdentityLink, OAuthState, User};
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::sync::OnceCell;

use crate::AppState;
use crate::auth::{MaybeUser, SESSION_COOKIE, hash_token, random_hex};
use crate::error::{ApiError, ApiResult};

const STATE_TTL_SECS: i64 = 600;
const ALLOWED_ALGS: &[Algorithm] = &[
    Algorithm::RS256,
    Algorithm::RS384,
    Algorithm::RS512,
    Algorithm::PS256,
    Algorithm::ES256,
    Algorithm::ES384,
    Algorithm::HS256,
];

#[derive(Debug, Clone)]
pub struct OidcConfig {
    /// Issuer URL, e.g. `https://accounts.example.com/realms/main`.
    pub issuer: String,
    pub client_id: String,
    pub client_secret: String,
    /// Create a local user on first login (otherwise an admin must pre-link identities).
    pub auto_register: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct Discovery {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
}

/// OIDC client state: configuration, HTTP client, cached discovery document.
#[derive(Debug)]
pub struct Oidc {
    pub config: OidcConfig,
    http: reqwest::Client,
    discovery: OnceCell<Discovery>,
}

impl Oidc {
    pub fn new(config: OidcConfig) -> Self {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap_or_default();
        Self {
            config,
            http,
            discovery: OnceCell::new(),
        }
    }

    async fn discovery(&self) -> ApiResult<&Discovery> {
        self.discovery
            .get_or_try_init(|| async {
                let url = format!(
                    "{}/.well-known/openid-configuration",
                    self.config.issuer.trim_end_matches('/')
                );
                let doc: Discovery = self
                    .http
                    .get(&url)
                    .send()
                    .await
                    .and_then(reqwest::Response::error_for_status)
                    .map_err(|e| ApiError::Internal(format!("OIDC discovery: {e}")))?
                    .json()
                    .await
                    .map_err(|e| ApiError::Internal(format!("OIDC discovery: {e}")))?;
                if doc.issuer.trim_end_matches('/') != self.config.issuer.trim_end_matches('/') {
                    return Err(ApiError::Internal("OIDC discovery: issuer mismatch".into()));
                }
                Ok(doc)
            })
            .await
    }
}

fn redirect_uri(state: &AppState) -> String {
    format!(
        "{}/api/v1/auth/oidc/callback",
        state.config.public_url.trim_end_matches('/')
    )
}

/// Only same-site absolute paths are allowed as post-login targets (no open redirects).
fn safe_return_to(p: Option<&str>) -> String {
    match p {
        Some(p) if p.starts_with('/') && !p.starts_with("//") && !p.contains('\\') => p.to_owned(),
        _ => "/".to_owned(),
    }
}

fn session_cookie(state: &AppState, value: &str, max_age: i64) -> HeaderValue {
    let secure = if state.config.public_url.starts_with("https://") {
        "; Secure"
    } else {
        ""
    };
    HeaderValue::from_str(&format!(
        "{SESSION_COOKIE}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age}{secure}"
    ))
    .unwrap_or_else(|_| HeaderValue::from_static(""))
}

/// `GET /api/v1/auth/config` — what login methods the UI should offer.
pub async fn auth_config(State(state): State<Arc<AppState>>) -> axum::Json<serde_json::Value> {
    axum::Json(json!({ "oidc": state.oidc.is_some() }))
}

#[derive(Debug, Deserialize)]
pub struct LoginQuery {
    pub return_to: Option<String>,
}

/// `GET /api/v1/auth/oidc/login` — redirect to the provider.
pub async fn login(
    State(state): State<Arc<AppState>>,
    Query(q): Query<LoginQuery>,
) -> ApiResult<Response> {
    let oidc = state.oidc.as_ref().ok_or(ApiError::NotFound)?;
    let disco = oidc.discovery().await?;
    let rnd = |n| random_hex(n).map_err(|e| ApiError::Internal(e.to_string()));
    let (st, nonce, verifier) = (rnd(16)?, rnd(16)?, rnd(32)?);
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(verifier.as_bytes()));
    state
        .db
        .save_oauth_state(
            &hash_token(&st),
            &OAuthState {
                nonce: nonce.clone(),
                pkce_verifier: verifier,
                return_to: safe_return_to(q.return_to.as_deref()),
            },
            STATE_TTL_SECS,
        )
        .await?;
    let mut url = url::Url::parse(&disco.authorization_endpoint)
        .map_err(|e| ApiError::Internal(format!("authorization_endpoint: {e}")))?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &oidc.config.client_id)
        .append_pair("redirect_uri", &redirect_uri(&state))
        .append_pair("scope", "openid profile email")
        .append_pair("state", &st)
        .append_pair("nonce", &nonce)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256");
    Ok(Redirect::to(url.as_str()).into_response())
}

#[derive(Debug, Deserialize)]
pub struct CallbackQuery {
    pub code: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    id_token: String,
}

#[derive(Debug, Deserialize)]
struct IdClaims {
    sub: String,
    nonce: Option<String>,
    preferred_username: Option<String>,
    email: Option<String>,
}

/// `GET /api/v1/auth/oidc/callback` — finish login, create the session.
pub async fn callback(
    State(state): State<Arc<AppState>>,
    Query(q): Query<CallbackQuery>,
) -> ApiResult<Response> {
    let oidc = state.oidc.as_ref().ok_or(ApiError::NotFound)?;
    if let Some(err) = q.error {
        return Err(ApiError::BadRequest(format!("login failed: {err}")));
    }
    let (Some(code), Some(st)) = (q.code, q.state) else {
        return Err(ApiError::BadRequest("missing code or state".into()));
    };
    let pending = state
        .db
        .take_oauth_state(&hash_token(&st))
        .await?
        .ok_or_else(|| ApiError::BadRequest("unknown or expired login state".into()))?;
    let disco = oidc.discovery().await?;

    let token: TokenResponse = oidc
        .http
        .post(&disco.token_endpoint)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", redirect_uri(&state).as_str()),
            ("client_id", oidc.config.client_id.as_str()),
            ("client_secret", oidc.config.client_secret.as_str()),
            ("code_verifier", pending.pkce_verifier.as_str()),
        ])
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|e| ApiError::BadRequest(format!("token exchange failed: {e}")))?
        .json()
        .await
        .map_err(|e| ApiError::BadRequest(format!("token response: {e}")))?;

    let claims = verify_id_token(oidc, disco, &token.id_token).await?;
    if claims.nonce.as_deref() != Some(pending.nonce.as_str()) {
        return Err(ApiError::BadRequest("nonce mismatch".into()));
    }

    let link = IdentityLink {
        provider: disco.issuer.clone(),
        subject: claims.sub.clone(),
    };
    let user = match state.db.user_by_identity(&link).await? {
        Some(u) => u,
        None if oidc.config.auto_register => register(&state, &link, &claims).await?,
        None => return Err(ApiError::Forbidden),
    };

    let session = random_hex(32).map_err(|e| ApiError::Internal(e.to_string()))?;
    let ttl = state.config.session_ttl_secs;
    state
        .db
        .create_session(&hash_token(&session), user.id, ttl)
        .await?;
    state
        .db
        .audit(
            Some(user.id),
            "user.login",
            &user.username,
            &json!({ "method": "oidc" }).to_string(),
        )
        .await?;
    let mut res = Redirect::to(&pending.return_to).into_response();
    res.headers_mut()
        .insert(header::SET_COOKIE, session_cookie(&state, &session, ttl));
    Ok(res)
}

async fn verify_id_token(oidc: &Oidc, disco: &Discovery, token: &str) -> ApiResult<IdClaims> {
    let bad = |e: &dyn std::fmt::Display| ApiError::BadRequest(format!("invalid id_token: {e}"));
    let head = jsonwebtoken::decode_header(token).map_err(|e| bad(&e))?;
    if !ALLOWED_ALGS.contains(&head.alg) {
        return Err(bad(&"algorithm not allowed"));
    }
    let key = if head.alg == Algorithm::HS256 {
        DecodingKey::from_secret(oidc.config.client_secret.as_bytes())
    } else {
        let jwks: JwkSet = oidc
            .http
            .get(&disco.jwks_uri)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|e| ApiError::Internal(format!("JWKS: {e}")))?
            .json()
            .await
            .map_err(|e| ApiError::Internal(format!("JWKS: {e}")))?;
        let jwk = match &head.kid {
            Some(kid) => jwks.find(kid),
            None if jwks.keys.len() == 1 => jwks.keys.first(),
            None => None,
        }
        .ok_or_else(|| bad(&"signing key not found"))?;
        DecodingKey::from_jwk(jwk).map_err(|e| bad(&e))?
    };
    let mut v = Validation::new(head.alg);
    v.set_issuer(&[disco.issuer.as_str()]);
    v.set_audience(&[oidc.config.client_id.as_str()]);
    v.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
    Ok(jsonwebtoken::decode::<IdClaims>(token, &key, &v)
        .map_err(|e| bad(&e))?
        .claims)
}

/// Turn an arbitrary `preferred_username` into a valid, free Fornax name.
fn username_candidates(claims: &IdClaims) -> Vec<String> {
    let raw = claims
        .preferred_username
        .clone()
        .or_else(|| {
            claims
                .email
                .as_ref()
                .and_then(|e| e.split('@').next().map(str::to_owned))
        })
        .unwrap_or_else(|| "user".to_owned());
    let mut base: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_start_matches(['-', '_'])
        .chars()
        .take(32)
        .collect();
    if validate_username(&base).is_err() {
        base = format!("user-{}", &hash_token(&claims.sub)[..8]);
    }
    std::iter::once(base.clone())
        .chain((2..100).map(|n| format!("{base}-{n}")))
        .filter(|n| validate_username(n).is_ok())
        .collect()
}

async fn register(state: &AppState, link: &IdentityLink, claims: &IdClaims) -> ApiResult<User> {
    for name in username_candidates(claims) {
        match state.db.create_user(&name, false).await {
            Ok(user) => {
                state.db.link_identity(link, user.id).await?;
                state
                    .db
                    .set_user_email(user.id, claims.email.as_deref())
                    .await?;
                state
                    .db
                    .audit(
                        Some(user.id),
                        "user.create",
                        &user.username,
                        &json!({ "source": "oidc", "issuer": link.provider }).to_string(),
                    )
                    .await?;
                return Ok(user);
            }
            Err(fornax_db::DbError::Conflict(_)) => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Err(ApiError::Conflict("could not allocate a username".into()))
}

/// `POST /api/v1/auth/logout` — end the browser session.
pub async fn logout(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    _user: MaybeUser,
) -> ApiResult<Response> {
    let session = headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == SESSION_COOKIE)
        .map(|(_, v)| v.to_owned());
    if let Some(s) = session {
        state.db.delete_session(&hash_token(&s)).await?;
    }
    let mut res = StatusCode::NO_CONTENT.into_response();
    res.headers_mut()
        .insert(header::SET_COOKIE, session_cookie(&state, "", 0));
    Ok(res)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn return_to_is_restricted() {
        assert_eq!(safe_return_to(Some("/alice/repo")), "/alice/repo");
        assert_eq!(safe_return_to(Some("//evil.com")), "/");
        assert_eq!(safe_return_to(Some("https://evil.com")), "/");
        assert_eq!(safe_return_to(Some("/\\evil.com")), "/");
        assert_eq!(safe_return_to(None), "/");
    }

    #[test]
    fn derives_usernames() {
        let c = |u: Option<&str>, e: Option<&str>| IdClaims {
            sub: "s1".into(),
            nonce: None,
            preferred_username: u.map(Into::into),
            email: e.map(Into::into),
        };
        assert_eq!(
            username_candidates(&c(Some("Jane.Doe"), None))[0],
            "Jane-Doe"
        );
        assert_eq!(username_candidates(&c(None, Some("bob@x.io")))[1], "bob-2");
        assert!(username_candidates(&c(Some("admin"), None))[0].starts_with("user-"));
        assert!(username_candidates(&c(Some("ü"), None))[0].starts_with("user-"));
    }
}
