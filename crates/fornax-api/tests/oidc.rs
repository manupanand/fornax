// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! OIDC login against an in-process mock identity provider (H-013): discovery, PKCE,
//! state/nonce, HS256 and RS256 (JWKS) ID tokens, sessions, logout.

mod common;

use std::collections::HashMap;
use std::process::Command;
use std::sync::{Arc, Mutex};

use axum::extract::{Query, State};
use axum::response::{IntoResponse, Redirect};
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use base64::Engine;
use common::Harness;
use fornax_api::{ApiConfig, OidcConfig};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use reqwest::StatusCode;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const CLIENT_ID: &str = "fornax-test";
const CLIENT_SECRET: &str = "s3cret-for-tests-only";

#[derive(Clone)]
struct Idp {
    base: String,
    alg: Algorithm,
    rsa_pem: Option<Vec<u8>>,
    jwk: Value,
    /// code -> (nonce, code_challenge)
    codes: Arc<Mutex<HashMap<String, (String, String)>>>,
    sub: Arc<Mutex<String>>,
}

async fn discovery(State(idp): State<Idp>) -> Json<Value> {
    Json(json!({
        "issuer": idp.base,
        "authorization_endpoint": format!("{}/authorize", idp.base),
        "token_endpoint": format!("{}/token", idp.base),
        "jwks_uri": format!("{}/jwks", idp.base),
    }))
}

async fn authorize(
    State(idp): State<Idp>,
    Query(q): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    assert_eq!(q["client_id"], CLIENT_ID);
    assert_eq!(q["code_challenge_method"], "S256");
    assert!(q["scope"].contains("openid"));
    let code = format!("code-{}", q["state"]);
    idp.codes.lock().expect("lock").insert(
        code.clone(),
        (q["nonce"].clone(), q["code_challenge"].clone()),
    );
    Redirect::to(&format!(
        "{}?code={code}&state={}",
        q["redirect_uri"], q["state"]
    ))
}

async fn token(
    State(idp): State<Idp>,
    Form(f): Form<HashMap<String, String>>,
) -> axum::response::Response {
    if f.get("client_secret").map(String::as_str) != Some(CLIENT_SECRET) {
        return (axum::http::StatusCode::UNAUTHORIZED, "bad client").into_response();
    }
    let Some((nonce, challenge)) = idp.codes.lock().expect("lock").remove(&f["code"]) else {
        return (axum::http::StatusCode::BAD_REQUEST, "bad code").into_response();
    };
    let computed = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(f["code_verifier"].as_bytes()));
    if computed != challenge {
        return (axum::http::StatusCode::BAD_REQUEST, "pkce").into_response();
    }
    let now = fornax_core::now_unix();
    let claims = json!({
        "iss": idp.base, "aud": CLIENT_ID, "sub": *idp.sub.lock().expect("lock"),
        "iat": now, "exp": now + 300, "nonce": nonce,
        "preferred_username": "Jane.Doe", "email": "jane@example.com",
    });
    let (header, key) = match idp.alg {
        Algorithm::HS256 => (
            Header::new(Algorithm::HS256),
            EncodingKey::from_secret(CLIENT_SECRET.as_bytes()),
        ),
        alg => {
            let mut h = Header::new(alg);
            h.kid = Some("test-key".into());
            (
                h,
                EncodingKey::from_rsa_pem(idp.rsa_pem.as_deref().expect("pem")).expect("key"),
            )
        }
    };
    let id_token = jsonwebtoken::encode(&header, &claims, &key).expect("sign");
    Json(json!({ "access_token": "at", "token_type": "Bearer", "id_token": id_token }))
        .into_response()
}

async fn jwks(State(idp): State<Idp>) -> Json<Value> {
    Json(json!({ "keys": [idp.jwk] }))
}

/// A fresh RSA key via `openssl`: (PKCS#8 PEM, JWK). Nothing is committed to the repo.
fn rsa_key(dir: &std::path::Path) -> (Vec<u8>, Value) {
    let pem = dir.join("idp.pem");
    let ok = Command::new("openssl")
        .args(["genrsa", "-out"])
        .arg(&pem)
        .arg("2048")
        .output()
        .expect("openssl")
        .status
        .success();
    assert!(ok, "openssl genrsa");
    let modulus = Command::new("openssl")
        .args(["rsa", "-noout", "-modulus", "-in"])
        .arg(&pem)
        .output()
        .expect("openssl");
    let hex_mod = String::from_utf8(modulus.stdout).expect("utf8");
    let n = hex::decode(hex_mod.trim().trim_start_matches("Modulus=")).expect("hex");
    let b64 = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
    let jwk = json!({ "kty": "RSA", "kid": "test-key", "alg": "RS256", "use": "sig",
                      "n": b64(&n), "e": b64(&[1, 0, 1]) });
    (std::fs::read(pem).expect("read pem"), jwk)
}

async fn start_idp(alg: Algorithm, dir: &std::path::Path) -> Idp {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    let (rsa_pem, jwk) = if alg == Algorithm::HS256 {
        (None, json!({}))
    } else {
        let (p, j) = rsa_key(dir);
        (Some(p), j)
    };
    let idp = Idp {
        base,
        alg,
        rsa_pem,
        jwk,
        codes: Arc::default(),
        sub: Arc::new(Mutex::new("subject-1".into())),
    };
    let app = Router::new()
        .route("/.well-known/openid-configuration", get(discovery))
        .route("/authorize", get(authorize))
        .route("/token", post(token))
        .route("/jwks", get(jwks))
        .with_state(idp.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.expect("idp") });
    idp
}

async fn harness(idp: &Idp, secret: &str) -> Harness {
    let oidc = OidcConfig {
        issuer: idp.base.clone(),
        client_id: CLIENT_ID.into(),
        client_secret: secret.into(),
        auto_register: true,
    };
    Harness::start_with(Some(oidc), ApiConfig::default()).await
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("client")
}

fn location(res: &reqwest::Response) -> String {
    res.headers()[reqwest::header::LOCATION]
        .to_str()
        .expect("location")
        .to_owned()
}

/// Browser-style login; returns (session cookie, final redirect target).
async fn login(h: &Harness, return_to: &str) -> (String, String) {
    let c = client();
    let res = c
        .get(format!(
            "{}/api/v1/auth/oidc/login?return_to={return_to}",
            h.base
        ))
        .send()
        .await
        .expect("login");
    assert!(res.status().is_redirection(), "{}", res.status());
    let res = c.get(location(&res)).send().await.expect("authorize");
    assert!(res.status().is_redirection());
    let callback = location(&res);
    let res = c.get(&callback).send().await.expect("callback");
    assert!(
        res.status().is_redirection(),
        "callback: {} {:?}",
        res.status(),
        res.text().await
    );
    let cookie = res.headers()[reqwest::header::SET_COOKIE]
        .to_str()
        .expect("cookie")
        .to_owned();
    assert!(cookie.contains("HttpOnly") && cookie.contains("SameSite=Lax"));
    let target = location(&res);
    // Replaying the callback must fail: state is single use.
    assert_eq!(
        c.get(&callback).send().await.expect("replay").status(),
        StatusCode::BAD_REQUEST
    );
    (cookie.split(';').next().expect("pair").to_owned(), target)
}

async fn whoami(h: &Harness, cookie: &str) -> (StatusCode, Value) {
    let res = client()
        .get(format!("{}/api/v1/user", h.base))
        .header("cookie", cookie)
        .send()
        .await
        .expect("user");
    let status = res.status();
    (status, res.json().await.unwrap_or(Value::Null))
}

#[tokio::test(flavor = "multi_thread")]
async fn login_with_hs256_and_logout() {
    let dir = tempfile::tempdir().expect("tmp");
    let idp = start_idp(Algorithm::HS256, dir.path()).await;
    let h = harness(&idp, CLIENT_SECRET).await;
    let (_, cfg) = h.call("GET", "/api/v1/auth/config", None, None).await;
    assert_eq!(cfg["oidc"], true);

    let (cookie, target) = login(&h, "/jane-doe").await;
    assert_eq!(target, "/jane-doe");
    let (status, me) = whoami(&h, &cookie).await;
    assert_eq!(
        (status, me["username"].as_str()),
        (StatusCode::OK, Some("Jane-Doe"))
    );
    assert_eq!(me["email"], "jane@example.com");

    // Same subject → same account; open redirects are neutralised.
    let (cookie2, target) = login(&h, "//evil.example").await;
    assert_eq!(target, "/");
    assert_eq!(whoami(&h, &cookie2).await.1["id"], me["id"]);

    // Cookie sessions can use the API (JSON bodies only).
    let res = client()
        .post(format!("{}/api/v1/user/repos", h.base))
        .header("cookie", &cookie)
        .json(&json!({ "name": "from-browser" }))
        .send()
        .await
        .expect("create");
    assert_eq!(res.status(), StatusCode::CREATED);

    let res = client()
        .post(format!("{}/api/v1/auth/logout", h.base))
        .header("cookie", &cookie)
        .send()
        .await
        .expect("logout");
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    assert_eq!(whoami(&h, &cookie).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(
        whoami(&h, &cookie2).await.0,
        StatusCode::OK,
        "other sessions stay valid"
    );

    // A second person whose preferred_username collides gets a suffixed name.
    *idp.sub.lock().expect("lock") = "subject-2".into();
    let (cookie3, _) = login(&h, "/").await;
    assert_eq!(whoami(&h, &cookie3).await.1["username"], "Jane-Doe-2");
}

#[tokio::test(flavor = "multi_thread")]
async fn login_with_rs256_jwks() {
    let dir = tempfile::tempdir().expect("tmp");
    let idp = start_idp(Algorithm::RS256, dir.path()).await;
    let h = harness(&idp, CLIENT_SECRET).await;
    let (cookie, _) = login(&h, "/").await;
    assert_eq!(whoami(&h, &cookie).await.0, StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread")]
async fn bad_client_secret_and_disabled_oidc() {
    let dir = tempfile::tempdir().expect("tmp");
    let idp = start_idp(Algorithm::HS256, dir.path()).await;
    let h = harness(&idp, "wrong-secret").await;
    let c = client();
    let res = c
        .get(format!("{}/api/v1/auth/oidc/login", h.base))
        .send()
        .await
        .expect("login");
    let res = c.get(location(&res)).send().await.expect("authorize");
    let res = c.get(location(&res)).send().await.expect("callback");
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    assert!(res.headers().get(reqwest::header::SET_COOKIE).is_none());

    let plain = Harness::start().await;
    assert_eq!(
        plain
            .status("GET", "/api/v1/auth/oidc/login", None, None)
            .await,
        axum::http::StatusCode::NOT_FOUND
    );
    let (_, cfg) = plain.call("GET", "/api/v1/auth/config", None, None).await;
    assert_eq!(cfg["oidc"], false);
    // An unknown session cookie is simply anonymous.
    let res = c
        .get(format!("{}/api/v1/user", plain.base))
        .header("cookie", "fornax_session=deadbeef")
        .send()
        .await
        .expect("user");
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}
