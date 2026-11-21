// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! Built-in SSH server for `git clone`/`git push` over SSH (H-014).
//!
//! Users authenticate with a public key registered via `/api/v1/user/keys`; the SSH user
//! name is ignored (clients usually connect as `git@`). Only two commands are accepted:
//! `git-upload-pack '<owner>/<repo>.git'` and `git-receive-pack '<owner>/<repo>.git'`.
//! No shell, PTY, port forwarding or subsystems.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;

use fornax_core::perm::Permission;
use fornax_db::{Db, User};
use fornax_git::{GitService, RepoStore};
use russh::keys::ssh_key::LineEnding;
use russh::keys::ssh_key::private::Ed25519Keypair;
use russh::keys::{HashAlg, PrivateKey};
use russh::server::{Auth, ChannelOpenHandle, Msg, Server as _, Session, VerifiedPublicKey};
use russh::{Channel, ChannelId};
use serde_json_lite::audit_detail;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;

#[derive(Debug, thiserror::Error)]
pub enum SshError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("host key: {0}")]
    HostKey(String),
}

/// Shared server state.
#[derive(Debug, Clone)]
pub struct SshState {
    pub db: Db,
    pub repos: RepoStore,
    /// Wakes runners after a push queued new jobs.
    pub ci_wakeup: fornax_ci::Wakeup,
}

/// Load the Ed25519 host key at `path`, creating it (mode 0600) on first start.
pub fn load_or_create_host_key(path: &Path) -> Result<PrivateKey, SshError> {
    if path.exists() {
        let text = std::fs::read_to_string(path)?;
        return PrivateKey::from_openssh(text).map_err(|e| SshError::HostKey(e.to_string()));
    }
    let mut seed = [0u8; 32];
    getrandom::getrandom(&mut seed).map_err(|e| SshError::HostKey(e.to_string()))?;
    let key = PrivateKey::from(Ed25519Keypair::from_seed(&seed));
    let text = key
        .to_openssh(LineEnding::LF)
        .map_err(|e| SshError::HostKey(e.to_string()))?;
    write_private(path, text.as_bytes())?;
    Ok(key)
}

#[cfg(unix)]
fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(data)
}

#[cfg(not(unix))]
fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, data)
}

/// Serve SSH on `listener` until the returned future completes.
pub async fn serve(
    listener: TcpListener,
    state: SshState,
    host_key: PrivateKey,
) -> std::io::Result<()> {
    let config = Arc::new(russh::server::Config {
        keys: vec![host_key],
        auth_rejection_time: std::time::Duration::from_secs(1),
        auth_rejection_time_initial: Some(std::time::Duration::from_secs(0)),
        inactivity_timeout: Some(std::time::Duration::from_secs(3600)),
        methods: russh::MethodSet::from(&[russh::MethodKind::PublicKey][..]),
        ..Default::default()
    });
    let mut server = Server {
        state: Arc::new(state),
    };
    server.run_on_socket(config, &listener).await
}

struct Server {
    state: Arc<SshState>,
}

impl russh::server::Server for Server {
    type Handler = Connection;

    fn new_client(&mut self, peer: Option<SocketAddr>) -> Connection {
        Connection {
            state: self.state.clone(),
            user: None,
            peer,
            channels: HashMap::new(),
        }
    }

    fn handle_session_error(&mut self, error: russh::Error) {
        tracing::debug!(%error, "ssh session ended with error");
    }
}

struct Connection {
    state: Arc<SshState>,
    user: Option<User>,
    peer: Option<SocketAddr>,
    channels: HashMap<ChannelId, Channel<Msg>>,
}

/// A parsed git-over-SSH command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitCommand {
    pub service: GitService,
    pub owner: String,
    pub repo: String,
}

/// Parse `git-upload-pack 'owner/repo.git'` (also `git upload-pack`, leading `/`, `~`
/// and missing quotes or `.git`).
pub fn parse_command(cmd: &str) -> Option<GitCommand> {
    let cmd = cmd.trim();
    let (verb, rest) = if let Some(rest) = cmd.strip_prefix("git ") {
        rest.trim_start().split_once(' ')?
    } else {
        cmd.split_once(' ')?
    };
    let service = match verb.trim_start_matches("git-") {
        "upload-pack" => GitService::UploadPack,
        "receive-pack" => GitService::ReceivePack,
        _ => return None,
    };
    let arg = rest.trim();
    let arg = arg
        .strip_prefix('\'')
        .and_then(|a| a.strip_suffix('\''))
        .unwrap_or(arg);
    if arg.contains(['\'', '"', ' ', '\\', '\0']) {
        return None;
    }
    let path = arg.trim_start_matches('/').trim_start_matches("~/");
    let (owner, repo) = path.split_once('/')?;
    let repo = repo.strip_suffix(".git").unwrap_or(repo);
    if fornax_core::names::validate_repo_name(repo).is_err()
        || owner.is_empty()
        || repo.contains('/')
    {
        return None;
    }
    Some(GitCommand {
        service,
        owner: owner.to_owned(),
        repo: repo.to_owned(),
    })
}

/// Minimal JSON for audit details without pulling serde_json into this crate.
mod serde_json_lite {
    pub fn audit_detail(transport: &str) -> String {
        format!("{{\"transport\":\"{transport}\"}}")
    }
}

impl russh::server::Handler for Connection {
    type Error = russh::Error;

    async fn auth_publickey(
        &mut self,
        _user: &str,
        key: VerifiedPublicKey<'_>,
    ) -> Result<Auth, Self::Error> {
        let fingerprint = key.public_key.fingerprint(HashAlg::Sha256).to_string();
        match self.state.db.user_by_ssh_fingerprint(&fingerprint).await {
            Ok(Some(user)) => {
                tracing::debug!(user = %user.username, peer = ?self.peer, "ssh login");
                self.user = Some(user);
                Ok(Auth::Accept)
            }
            Ok(None) => Ok(Auth::reject()),
            Err(e) => {
                tracing::error!(error = %e, "ssh key lookup failed");
                Ok(Auth::reject())
            }
        }
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.channels.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    async fn exec_request(
        &mut self,
        channel_id: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let Some(channel) = self.channels.remove(&channel_id) else {
            session.channel_failure(channel_id)?;
            return Ok(());
        };
        let (Some(user), Some(cmd)) = (
            self.user.clone(),
            std::str::from_utf8(data).ok().and_then(parse_command),
        ) else {
            reject(
                channel,
                "fornax: only git-upload-pack and git-receive-pack are supported\n",
            )
            .await;
            session.channel_success(channel_id)?;
            return Ok(());
        };
        session.channel_success(channel_id)?;
        let state = self.state.clone();
        tokio::spawn(async move {
            if let Err(msg) = run_git(state, user, cmd, channel).await {
                tracing::warn!(%msg, "ssh git command failed");
            }
        });
        Ok(())
    }
}

async fn reject(channel: Channel<Msg>, message: &str) {
    let mut err = channel.make_writer_ext(Some(1));
    let _ = err.write_all(message.as_bytes()).await;
    let _ = err.flush().await;
    let _ = channel.exit_status(1).await;
    let _ = channel.eof().await;
    let _ = channel.close().await;
}

async fn run_git(
    state: Arc<SshState>,
    user: User,
    cmd: GitCommand,
    channel: Channel<Msg>,
) -> Result<(), String> {
    let repo = state
        .db
        .repo_by_name(&cmd.owner, &cmd.repo)
        .await
        .map_err(|e| e.to_string())?;
    let needed = match cmd.service {
        GitService::UploadPack => Permission::Read,
        GitService::ReceivePack => Permission::Write,
    };
    let Some(repo) = repo else {
        reject(channel, "fornax: repository not found\n").await;
        return Ok(());
    };
    let have = state
        .db
        .permission(Some(&user), &repo)
        .await
        .map_err(|e| e.to_string())?;
    if have < needed {
        let msg = if have == Permission::None {
            "fornax: repository not found\n"
        } else {
            "fornax: permission denied\n"
        };
        reject(channel, msg).await;
        return Ok(());
    }

    let before = if cmd.service == GitService::ReceivePack {
        fornax_ci::before_push(&state.repos, &repo).await.ok()
    } else {
        None
    };

    let (o, n) = repo.storage_key();
    let mut child = state
        .repos
        .ssh_service_command(&state.repos.repo_path(&o, &n), cmd.service)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let (Some(mut stdin), Some(mut stdout), Some(mut stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        return Err("git pipes unavailable".into());
    };

    let (mut read_half, write_half) = channel.split();
    let mut out = write_half.make_writer();
    let mut err = write_half.make_writer_ext(Some(1));
    let to_git = tokio::spawn(async move {
        let mut reader = read_half.make_reader();
        let _ = tokio::io::copy(&mut reader, &mut stdin).await;
        let _ = stdin.shutdown().await;
    });
    let from_git = async {
        let _ = tokio::io::copy(&mut stdout, &mut out).await;
        let _ = out.flush().await;
    };
    let errors = async {
        let _ = tokio::io::copy(&mut stderr, &mut err).await;
        let _ = err.flush().await;
    };
    tokio::join!(from_git, errors);
    let status = child.wait().await.map_err(|e| e.to_string())?;
    to_git.abort();

    let code = status.code().unwrap_or(1).clamp(0, 255) as u32;
    let _ = write_half.exit_status(code).await;
    let _ = write_half.eof().await;
    let _ = write_half.close().await;

    if status.success() && cmd.service == GitService::ReceivePack {
        let full = repo.full_name();
        let result = async {
            state.db.touch_repo(repo.id).await?;
            state
                .db
                .audit(Some(user.id), "repo.push", &full, &audit_detail("ssh"))
                .await
        }
        .await;
        if let Err(e) = result {
            tracing::error!(error = %e, repo = %full, "failed to record push");
        }
        if let Some(before) = before {
            let now = fornax_core::now_unix();
            match fornax_ci::after_push(
                &state.db,
                &state.repos,
                &repo,
                Some(user.id),
                &before,
                &state.ci_wakeup,
                now,
            )
            .await
            {
                Ok(outcome) => {
                    for (git_ref, diags) in &outcome.errors {
                        for d in diags {
                            tracing::warn!(repo = %full, %git_ref, diagnostic = %d, ".fornax spec error");
                        }
                    }
                }
                Err(e) => tracing::error!(error = %e, repo = %full, "creating pipelines failed"),
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_git_commands() {
        let c = parse_command("git-upload-pack 'alice/demo.git'").expect("parse");
        assert_eq!(
            c,
            GitCommand {
                service: GitService::UploadPack,
                owner: "alice".into(),
                repo: "demo".into()
            }
        );
        let c = parse_command("git-receive-pack '/acme/site.git'").expect("parse");
        assert_eq!(
            (c.service, c.owner.as_str()),
            (GitService::ReceivePack, "acme")
        );
        assert!(parse_command("git upload-pack alice/demo").is_some());
        assert!(parse_command("ls -la").is_none());
        assert!(parse_command("git-upload-pack 'a/b'; rm -rf /'").is_none());
        assert!(parse_command("git-upload-pack '../etc/passwd'").is_none());
        assert!(parse_command("git-upload-pack 'a/b/c.git'").is_none());
        assert!(parse_command("git-upload-archive 'a/b.git'").is_none());
    }

    #[test]
    fn creates_and_reloads_host_key() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("host_key");
        let a = load_or_create_host_key(&path).expect("create");
        let b = load_or_create_host_key(&path).expect("load");
        assert_eq!(a.public_key(), b.public_key());
    }
}
