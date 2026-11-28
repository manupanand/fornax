// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! `fornax-gcp` — interactive setup for GCP-connected on-prem servers and the pull-based
//! deploy agent (replaces `agent.py` + `deploy.sh`). Design: `docs/fornax-gcp.md`.

mod agent;
mod config;
mod doctor;
mod exec;
mod prompt;
mod setup;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};

use crate::exec::{DryRunner, RealRunner, Runner};

#[derive(Debug, Parser)]
#[command(
    name = "fornax-gcp",
    version,
    about = "Set up a GCP-connected on-prem server and run its pull-based deploy agent"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Check that gcloud, git, Docker and Compose are installed and usable.
    Doctor {
        /// What to check for.
        #[arg(long, value_enum, default_value_t = doctor::Mode::Server)]
        mode: doctor::Mode,
    },
    /// Show or validate a configuration file.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Google Cloud side (admin machine, your own gcloud login).
    Gcp {
        #[command(subcommand)]
        command: GcpCommand,
    },
    /// On-prem server side (run with sudo).
    Server {
        #[command(subcommand)]
        command: ServerCommand,
    },
    /// The deploy agent (systemd runs `agent run` as the deploy user).
    Agent {
        #[command(subcommand)]
        command: AgentCommand,
    },
    /// Deploy one app at one tag now (same exit codes as deploy.sh: 0 ok, 2 busy,
    /// 3 failed + previous kept, 4 failed + not restored, 64 usage).
    Deploy {
        /// App or shared-service name.
        name: String,
        /// Tag, e.g. v1.2.3 or platform-v1.2.3.
        tag: String,
        /// Restart the containers only (no pull, no recreate).
        #[arg(long)]
        restart: bool,
        #[command(flatten)]
        config: ConfigArg,
    },
    /// What is deployed and what failed, per app.
    Status {
        #[command(flatten)]
        config: ConfigArg,
    },
}

#[derive(Debug, Args)]
struct ConfigArg {
    /// Configuration file.
    #[arg(long, env = "FORNAX_GCP_CONFIG", default_value = config::DEFAULT_SERVER_CONFIG)]
    config: PathBuf,
}

#[derive(Debug, Args)]
struct SetupArgs {
    /// Print every command instead of running it; change nothing.
    #[arg(long)]
    dry_run: bool,
    /// Accept the suggested / existing value for every question.
    #[arg(long)]
    yes: bool,
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// Print the configuration.
    Show(ConfigArg),
    /// Check the configuration and report every problem.
    Validate(ConfigArg),
    /// Print an example configuration (BLP values).
    Example,
}

#[derive(Debug, Subcommand)]
enum GcpCommand {
    /// Create topic, subscription, service account, IAM bindings; attach repos.
    Init {
        /// Where to write the configuration.
        #[arg(long, default_value = config::DEFAULT_ADMIN_CONFIG)]
        config: PathBuf,
        #[command(flatten)]
        setup: SetupArgs,
    },
    /// Give the agent read access to one more repo and attach the topic to it.
    AddRepo {
        /// Repository name.
        name: String,
        #[arg(long, default_value = config::DEFAULT_ADMIN_CONFIG)]
        config: PathBuf,
        #[command(flatten)]
        setup: SetupArgs,
    },
    /// Create a key for the service account at an absolute path (mode 0600).
    CreateKey {
        /// Absolute output path for the JSON key.
        #[arg(long)]
        out: Option<PathBuf>,
        #[arg(long, default_value = config::DEFAULT_ADMIN_CONFIG)]
        config: PathBuf,
        #[command(flatten)]
        setup: SetupArgs,
    },
}

#[derive(Debug, Subcommand)]
enum ServerCommand {
    /// Folders, key check, agent gcloud login, config, systemd unit (needs root).
    Init {
        /// Configuration to start from (copied from the admin machine).
        #[arg(long, default_value = config::DEFAULT_SERVER_CONFIG)]
        config: PathBuf,
        #[command(flatten)]
        setup: SetupArgs,
    },
}

#[derive(Debug, Subcommand)]
enum AgentCommand {
    /// Pull Pub/Sub wake-ups and reconcile every app (what systemd runs).
    Run {
        /// Reconcile once and exit (no Pub/Sub).
        #[arg(long)]
        once: bool,
        #[command(flatten)]
        config: ConfigArg,
    },
}

fn runner(dry_run: bool) -> Box<dyn Runner> {
    if dry_run {
        Box::new(DryRunner::default())
    } else {
        Box::new(RealRunner)
    }
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .init();
    let cli = Cli::parse();
    match dispatch(cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(1)
        }
    }
}

fn dispatch(cli: Cli) -> anyhow::Result<ExitCode> {
    match cli.command {
        Command::Doctor { mode } => doctor::run(&RealRunner, mode)?,
        Command::Config { command } => match command {
            ConfigCommand::Show(c) => print!("{}", config::Config::load(&c.config)?.to_toml()?),
            ConfigCommand::Validate(c) => {
                config::Config::load(&c.config)?;
                println!("{} is valid", c.config.display());
            }
            ConfigCommand::Example => print!("{}", config::Config::blp_example().to_toml()?),
        },
        Command::Gcp { command } => match command {
            GcpCommand::Init { config, setup } => {
                setup::gcp::init(&*runner(setup.dry_run), &config, setup.yes)?;
            }
            GcpCommand::AddRepo {
                name,
                config,
                setup,
            } => setup::gcp::add_repo(&*runner(setup.dry_run), &config, &name, setup.yes)?,
            GcpCommand::CreateKey { out, config, setup } => {
                setup::gcp::create_key(
                    &*runner(setup.dry_run),
                    &config,
                    out.as_deref(),
                    setup.yes,
                )?;
            }
        },
        Command::Server { command } => match command {
            ServerCommand::Init { config, setup } => {
                setup::server::init(&*runner(setup.dry_run), &config, setup.yes)?;
            }
        },
        Command::Agent { command } => match command {
            AgentCommand::Run { once, config } => {
                let cfg = config::Config::load(&config.config)?;
                doctor::require(&RealRunner, doctor::Mode::Agent)?;
                agent::run(&RealRunner, &cfg, once)?;
            }
        },
        Command::Deploy {
            name,
            tag,
            restart,
            config,
        } => {
            let cfg = config::Config::load(&config.config)?;
            let code = agent::deploy::deploy(&RealRunner, &cfg, &name, &tag, restart);
            return Ok(ExitCode::from(code.as_u8()));
        }
        Command::Status { config } => {
            let cfg = config::Config::load(&config.config)?;
            agent::status(&cfg)?;
        }
    }
    Ok(ExitCode::SUCCESS)
}
