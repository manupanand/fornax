// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! `fornax` — the single Fornax binary.
//!
//! ```text
//! fornax serve                        run the server (API, Git over HTTP/SSH, web UI)
//! fornax admin create-user NAME       create a user and print an access token
//! fornax admin create-token NAME      issue another access token for a user
//! fornax admin backup --out DIR       consistent copy of DB, repositories, host key
//! fornax admin restore --from DIR     restore a backup into an empty data directory
//! fornax admin create-runner NAME     register a CI runner and print its runner token
//! fornax agent --server URL           run a CI runner agent (host executor)
//! ```

mod app;
mod backup;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use fornax_api::auth::generate_token;
use fornax_api::{ApiConfig, AppState, Oidc, OidcConfig};
use fornax_db::Db;
use fornax_git::RepoStore;
use fornax_runner::{
    DEFAULT_LEASE_SECS, DEFAULT_REAP_INTERVAL, QueueNotifier, RunnerConfig, RunnerService,
};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "fornax",
    version,
    about = "Git hosting, CI/CD and GitOps in one binary"
)]
struct Cli {
    /// Directory holding the database, repositories and SSH host key.
    #[arg(long, global = true, env = "FORNAX_DATA_DIR", default_value = "./data")]
    data_dir: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the server.
    Serve(ServeArgs),
    /// Administrative tasks run directly against the data directory.
    Admin {
        #[command(subcommand)]
        command: AdminCommand,
    },
    /// Run a CI runner agent with the host executor until Ctrl-C.
    ///
    /// SECURITY: the host executor runs job scripts directly on this machine with no
    /// isolation (LOG D-008). Run it as a dedicated unprivileged user, for trusted
    /// repositories only.
    Agent(AgentArgs),
}

#[derive(Debug, Args)]
struct AgentArgs {
    /// Fornax server URL, e.g. `https://fornax.example.com`.
    #[arg(long, env = "FORNAX_SERVER_URL")]
    server: String,
    /// File containing the runner token (`fnr_…`) from `fornax admin create-runner`.
    /// Alternatively set FORNAX_RUNNER_TOKEN. There is deliberately no `--token` flag:
    /// command-line arguments are visible to every local user via `ps` (D-043).
    #[arg(long, env = "FORNAX_RUNNER_TOKEN_FILE")]
    token_file: Option<PathBuf>,
    /// Directory for per-job workspaces.
    #[arg(long, env = "FORNAX_WORK_DIR")]
    work_dir: PathBuf,
    /// Maximum number of concurrent jobs.
    #[arg(long, env = "FORNAX_RUNNER_CAPACITY", default_value_t = 1,
          value_parser = clap::value_parser!(u16).range(1..))]
    capacity: u16,
}

#[derive(Debug, Args)]
struct ServeArgs {
    /// HTTP listen address.
    #[arg(long, env = "FORNAX_LISTEN", default_value = "127.0.0.1:3000")]
    listen: SocketAddr,
    /// URL clients use to reach the server (clone URLs, OIDC redirect).
    #[arg(long, env = "FORNAX_PUBLIC_URL")]
    public_url: Option<String>,
    /// SSH listen address; SSH is disabled when unset.
    #[arg(long, env = "FORNAX_SSH_LISTEN")]
    ssh_listen: Option<SocketAddr>,
    /// Prefix for SSH clone URLs, e.g. `ssh://git@git.example.com:2222/`.
    #[arg(long, env = "FORNAX_SSH_URL")]
    ssh_url: Option<String>,
    /// OIDC issuer URL; OIDC login is disabled when unset.
    #[arg(long, env = "FORNAX_OIDC_ISSUER")]
    oidc_issuer: Option<String>,
    #[arg(long, env = "FORNAX_OIDC_CLIENT_ID", requires = "oidc_issuer")]
    oidc_client_id: Option<String>,
    #[arg(
        long,
        env = "FORNAX_OIDC_CLIENT_SECRET",
        requires = "oidc_issuer",
        hide_env_values = true
    )]
    oidc_client_secret: Option<String>,
    /// Create local users on first OIDC login.
    #[arg(long, env = "FORNAX_OIDC_AUTO_REGISTER", default_value_t = true, action = clap::ArgAction::Set)]
    oidc_auto_register: bool,
    /// Shared secret that lets agents self-register (runner protocol `Register`).
    /// Self-registration is disabled when unset; use `fornax admin create-runner`.
    #[arg(long, env = "FORNAX_RUNNER_REGISTRATION_TOKEN", hide_env_values = true)]
    runner_registration_token: Option<String>,
    /// CI attempt lease in seconds; agents renew it every heartbeat (10 s by default).
    #[arg(long, env = "FORNAX_RUNNER_LEASE_SECS", default_value_t = DEFAULT_LEASE_SECS,
          value_parser = clap::value_parser!(i64).range(15..=3600))]
    runner_lease_secs: i64,
}

#[derive(Debug, Subcommand)]
enum AdminCommand {
    /// Create a user and print a new access token for it.
    CreateUser {
        username: String,
        /// Make the user a site administrator.
        #[arg(long)]
        admin: bool,
    },
    /// Issue a new access token for an existing user.
    CreateToken {
        username: String,
        /// Label shown in token listings.
        #[arg(long, default_value = "cli")]
        name: String,
    },
    /// Write a backup (database, repositories, SSH host key) to a new directory.
    Backup {
        #[arg(long)]
        out: PathBuf,
    },
    /// Restore a backup into an empty data directory.
    Restore {
        #[arg(long)]
        from: PathBuf,
    },
    /// Register a CI runner and print its runner token (shown once).
    CreateRunner {
        name: String,
        /// Comma-separated labels the runner offers, e.g. `linux,x86_64`.
        #[arg(long, value_delimiter = ',')]
        labels: Vec<String>,
        /// Also run jobs of untrusted refs (fork pull requests). Off by default: host
        /// runners have no isolation (LOG D-008).
        #[arg(long)]
        allow_untrusted: bool,
    },
}

async fn open_db(data_dir: &std::path::Path) -> Result<Db> {
    tokio::fs::create_dir_all(data_dir.join("repos"))
        .await
        .with_context(|| format!("creating {}", data_dir.display()))?;
    let path = data_dir.join("fornax.db");
    Db::open(&path)
        .await
        .with_context(|| format!("opening {}", path.display()))
}

async fn issue_token(db: &Db, user_id: i64, name: &str) -> Result<String> {
    let (token, hash) =
        generate_token().map_err(|e| anyhow::anyhow!("random source failed: {e}"))?;
    db.create_token(user_id, name, &hash).await?;
    Ok(token)
}

fn default_ssh_url(public_url: &str, ssh: SocketAddr) -> String {
    let host = public_url
        .split("://")
        .nth(1)
        .and_then(|rest| rest.split(['/', ':']).next())
        .filter(|h| !h.is_empty())
        .unwrap_or("localhost");
    format!("ssh://git@{host}:{}/", ssh.port())
}

async fn serve(data_dir: PathBuf, args: ServeArgs) -> Result<()> {
    let db = open_db(&data_dir).await?;
    let public_url = args
        .public_url
        .unwrap_or_else(|| format!("http://{}", args.listen));
    let ssh_url_base = args.ssh_listen.map(|ssh| {
        args.ssh_url
            .clone()
            .unwrap_or_else(|| default_ssh_url(&public_url, ssh))
    });
    let oidc = match (
        args.oidc_issuer,
        args.oidc_client_id,
        args.oidc_client_secret,
    ) {
        (Some(issuer), Some(client_id), Some(client_secret)) => Some(Oidc::new(OidcConfig {
            issuer,
            client_id,
            client_secret,
            auto_register: args.oidc_auto_register,
        })),
        (None, _, _) => None,
        _ => bail!("OIDC needs --oidc-issuer, --oidc-client-id and --oidc-client-secret"),
    };
    let repos = RepoStore::new(data_dir.join("repos"));
    // CI runner protocol (gRPC on this port) and its queue wake-ups. The push path must call
    // `notifier.notify()` after creating a pipeline.
    let notifier = QueueNotifier::new();
    let mut runner_config = RunnerConfig::new(public_url.clone());
    runner_config.lease_secs = args.runner_lease_secs;
    runner_config.registration_token = args.runner_registration_token;
    // Pushes (HTTP and SSH) wake long-polling runners through the same notifier.
    let wakeup = {
        let n = notifier.clone();
        fornax_ci::Wakeup::new(move || n.notify())
    };
    let runner = RunnerService::new(db.clone(), runner_config, notifier);
    let reaper = runner.spawn_reaper(DEFAULT_REAP_INTERVAL);
    let state = Arc::new(AppState {
        db: db.clone(),
        repos: repos.clone(),
        config: ApiConfig {
            public_url,
            ssh_url_base,
            ..ApiConfig::default()
        },
        oidc,
        ci_wakeup: wakeup.clone(),
    });

    if let Some(addr) = args.ssh_listen {
        let key = fornax_ssh::load_or_create_host_key(&data_dir.join("ssh_host_ed25519_key"))?;
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .with_context(|| format!("binding SSH {addr}"))?;
        tracing::info!(%addr, "ssh listening");
        let ssh_state = fornax_ssh::SshState {
            db: db.clone(),
            repos,
            ci_wakeup: wakeup.clone(),
        };
        tokio::spawn(async move {
            if let Err(e) = fornax_ssh::serve(listener, ssh_state, key).await {
                tracing::error!(error = %e, "ssh server stopped");
            }
        });
    }

    // Housekeeping: expired sessions and login states.
    let prune_db = db.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(3600));
        loop {
            tick.tick().await;
            if let Err(e) = prune_db.prune_expired().await {
                tracing::warn!(error = %e, "pruning sessions failed");
            }
        }
    });

    let listener = tokio::net::TcpListener::bind(args.listen)
        .await
        .with_context(|| format!("binding {}", args.listen))?;
    tracing::info!(
        listen = %args.listen,
        data_dir = %data_dir.display(),
        ui = fornax_api::ui::available(),
        oidc = state.oidc.is_some(),
        "fornax listening"
    );
    let stop_runner = runner.clone();
    axum::serve(listener, app::build(state, runner))
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("shutting down");
            // Release runner long-polls so they don't hold up the graceful shutdown.
            stop_runner.shutdown();
        })
        .await?;
    let _ = reaper.await;
    db.close().await;
    Ok(())
}

/// The runner token from `FORNAX_RUNNER_TOKEN` or `--token-file` (never argv).
fn runner_token(args: &AgentArgs) -> Result<String> {
    if let Ok(token) = std::env::var("FORNAX_RUNNER_TOKEN")
        && !token.trim().is_empty()
    {
        return Ok(token.trim().to_owned());
    }
    match &args.token_file {
        Some(path) => Ok(std::fs::read_to_string(path)
            .with_context(|| format!("reading runner token from {}", path.display()))?
            .trim()
            .to_owned()),
        None => bail!("set FORNAX_RUNNER_TOKEN or pass --token-file with the runner token"),
    }
}

async fn run_agent(args: AgentArgs) -> Result<()> {
    let token = runner_token(&args)?;
    let mut config = fornax_agent::AgentConfig::new(args.server, token, args.work_dir.clone());
    config.capacity = usize::from(args.capacity);
    config.validate()?;
    tokio::fs::create_dir_all(&args.work_dir)
        .await
        .with_context(|| format!("creating {}", args.work_dir.display()))?;
    tracing::warn!(
        "host executor: jobs run unisolated as this OS user; use only for trusted repositories"
    );
    fornax_agent::run(config, async {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("agent shutting down");
    })
    .await?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    let cli = Cli::parse();

    match cli.command {
        Command::Serve(args) => serve(cli.data_dir, args).await?,
        Command::Agent(args) => run_agent(args).await?,
        Command::Admin { command } => match command {
            AdminCommand::CreateUser { username, admin } => {
                fornax_core::names::validate_username(&username)?;
                let db = open_db(&cli.data_dir).await?;
                let user = db.create_user(&username, admin).await?;
                db.audit(None, "user.create", &user.username, "{\"source\":\"cli\"}")
                    .await?;
                let token = issue_token(&db, user.id, "initial").await?;
                println!("created user {} (admin: {})", user.username, user.is_admin);
                println!("access token (shown once): {token}");
            }
            AdminCommand::CreateToken { username, name } => {
                let db = open_db(&cli.data_dir).await?;
                let Some(user) = db.user_by_name(&username).await?.filter(|u| !u.is_org) else {
                    bail!("no such user: {username}");
                };
                let token = issue_token(&db, user.id, &name).await?;
                db.audit(None, "token.create", &user.username, "{\"source\":\"cli\"}")
                    .await?;
                println!("access token (shown once): {token}");
            }
            AdminCommand::Backup { out } => {
                let db = open_db(&cli.data_dir).await?;
                let manifest = backup::backup(&db, &cli.data_dir, &out).await?;
                println!(
                    "backup written to {} ({} repositories)",
                    out.display(),
                    manifest.repositories
                );
            }
            AdminCommand::CreateRunner {
                name,
                labels,
                allow_untrusted,
            } => {
                let db = open_db(&cli.data_dir).await?;
                let trusted_only = !allow_untrusted;
                let (runner, token) = fornax_runner::create_runner(
                    &db,
                    &name,
                    &labels,
                    trusted_only,
                    fornax_core::now_unix(),
                )
                .await?;
                let detail = serde_json::json!({
                    "source": "cli",
                    "runner_id": runner.id,
                    "trusted_only": trusted_only,
                });
                db.audit(None, "runner.create", &runner.name, &detail.to_string())
                    .await?;
                println!(
                    "created runner {} (id {}, labels [{}], trusted only: {})",
                    runner.name,
                    runner.id,
                    runner.labels.join(","),
                    runner.trusted_only
                );
                println!("runner token (shown once): {token}");
            }
            AdminCommand::Restore { from } => {
                let manifest = backup::restore(&from, &cli.data_dir).await?;
                println!(
                    "restored {} repositories into {}",
                    manifest.repositories,
                    cli.data_dir.display()
                );
            }
        },
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_ssh_url() {
        let addr: SocketAddr = "0.0.0.0:2222".parse().expect("addr");
        assert_eq!(
            default_ssh_url("https://git.example.com", addr),
            "ssh://git@git.example.com:2222/"
        );
        assert_eq!(
            default_ssh_url("http://localhost:3000/", addr),
            "ssh://git@localhost:2222/"
        );
    }
}
