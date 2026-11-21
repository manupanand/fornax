// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! SSH public keys of the current user (`/api/v1/user/keys`), used by the built-in SSH
//! server (H-014).

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use base64::Engine;
use fornax_db::SshKey;
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::AppState;
use crate::auth::MaybeUser;
use crate::error::{ApiError, ApiResult};

const ALLOWED_TYPES: &[&str] = &[
    "ssh-ed25519",
    "ecdsa-sha2-nistp256",
    "ecdsa-sha2-nistp384",
    "ecdsa-sha2-nistp521",
    "ssh-rsa",
    "sk-ssh-ed25519@openssh.com",
    "sk-ecdsa-sha2-nistp256@openssh.com",
];

/// A parsed OpenSSH public key line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedKey {
    /// Normalised `"<type> <base64>"` (comment dropped).
    pub openssh: String,
    /// `SHA256:<base64 without padding>`, as printed by `ssh-keygen -lf`.
    pub fingerprint: String,
}

/// Parse `"<type> <base64> [comment]"`, checking the key type and that the blob's embedded
/// type matches.
pub fn parse_public_key(line: &str) -> Result<ParsedKey, String> {
    let mut parts = line.split_whitespace();
    let (Some(kind), Some(b64)) = (parts.next(), parts.next()) else {
        return Err("expected '<type> <base64> [comment]'".into());
    };
    if !ALLOWED_TYPES.contains(&kind) {
        return Err(format!("unsupported key type {kind}"));
    }
    let blob = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|_| "key is not valid base64".to_owned())?;
    let embedded = blob
        .get(..4)
        .map(|n| u32::from_be_bytes([n[0], n[1], n[2], n[3]]) as usize)
        .and_then(|len| blob.get(4..4 + len))
        .ok_or_else(|| "truncated key".to_owned())?;
    if embedded != kind.as_bytes() {
        return Err("key type does not match key data".into());
    }
    let digest = Sha256::digest(&blob);
    Ok(ParsedKey {
        openssh: format!("{kind} {b64}"),
        fingerprint: format!(
            "SHA256:{}",
            base64::engine::general_purpose::STANDARD_NO_PAD.encode(digest)
        ),
    })
}

pub async fn list_keys(
    State(state): State<Arc<AppState>>,
    user: MaybeUser,
) -> ApiResult<Json<Vec<SshKey>>> {
    let user = user.require()?;
    Ok(Json(state.db.ssh_keys(user.id).await?))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AddKey {
    pub name: String,
    pub key: String,
}

pub async fn add_key(
    State(state): State<Arc<AppState>>,
    user: MaybeUser,
    Json(body): Json<AddKey>,
) -> ApiResult<(StatusCode, Json<SshKey>)> {
    let user = user.require()?;
    if body.name.is_empty() || body.name.len() > 100 {
        return Err(ApiError::BadRequest("name: must be 1-100 bytes".into()));
    }
    let parsed =
        parse_public_key(&body.key).map_err(|e| ApiError::BadRequest(format!("key: {e}")))?;
    let key = state
        .db
        .add_ssh_key(user.id, &body.name, &parsed.fingerprint, &parsed.openssh)
        .await?;
    state
        .db
        .audit(
            Some(user.id),
            "user.ssh_key.add",
            &user.username,
            &json!({ "fingerprint": parsed.fingerprint }).to_string(),
        )
        .await?;
    Ok((StatusCode::CREATED, Json(key)))
}

pub async fn delete_key(
    State(state): State<Arc<AppState>>,
    user: MaybeUser,
    Path(id): Path<i64>,
) -> ApiResult<StatusCode> {
    let user = user.require()?;
    if !state.db.delete_ssh_key(user.id, id).await? {
        return Err(ApiError::NotFound);
    }
    state
        .db
        .audit(
            Some(user.id),
            "user.ssh_key.delete",
            &user.username,
            &json!({ "id": id }).to_string(),
        )
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Public half of a throwaway test key; fingerprint from `ssh-keygen -lf`.
    const ED25519: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJdD7y3aLq454yWBdwLWbieU1ebz9/cu7/QEXn9OIeZJ test@x";

    #[test]
    fn parses_and_fingerprints() {
        let k = parse_public_key(ED25519).expect("parse");
        assert!(k.openssh.starts_with("ssh-ed25519 AAAA") && !k.openssh.contains("test@x"));
        assert!(k.fingerprint.starts_with("SHA256:"));
        assert_eq!(k.fingerprint.len(), "SHA256:".len() + 43);
    }

    #[test]
    fn rejects_bad_keys() {
        assert!(parse_public_key("").is_err());
        assert!(parse_public_key("ssh-dss AAAAB3NzaC1kc3M=").is_err());
        assert!(parse_public_key("ssh-ed25519 !!!").is_err());
        // Valid base64, but the blob claims to be ssh-rsa.
        assert!(parse_public_key("ssh-ed25519 AAAAB3NzaC1yc2EAAAADAQAB").is_err());
    }
}
