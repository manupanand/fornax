// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! `fornax-gcp doctor`: check that required tools are installed and usable.
//!
//! Per the packaging decision (D-045), the package does not install gcloud, git or Docker; this
//! check stops every command early with a clear message naming what is missing.

use std::fmt;

use crate::exec::{Cmd, Runner};

/// What the check is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Mode {
    /// Admin machine: gcloud logged in with a user account.
    Gcp,
    /// Server setup: git, Docker, Compose v2, gcloud.
    Server,
    /// Running agent: same tools as `server`.
    Agent,
}

/// Result of one check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    /// Short name, e.g. `gcloud`.
    pub name: &'static str,
    /// Passed?
    pub ok: bool,
    /// Version or problem description.
    pub detail: String,
    /// How to fix it (shown on failure).
    pub hint: &'static str,
}

impl fmt::Display for Check {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.ok {
            write!(f, "  ok    {:<16} {}", self.name, self.detail)
        } else {
            write!(
                f,
                "  MISSING {:<14} {}\n          -> {}",
                self.name, self.detail, self.hint
            )
        }
    }
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").trim().to_owned()
}

fn tool(r: &dyn Runner, name: &'static str, cmd: Cmd, hint: &'static str) -> Check {
    match r.run(&cmd) {
        Ok(out) if out.ok() => Check {
            name,
            ok: true,
            detail: first_line(&out.stdout),
            hint,
        },
        Ok(out) => Check {
            name,
            ok: false,
            detail: first_line(if out.stderr.trim().is_empty() {
                &out.stdout
            } else {
                &out.stderr
            }),
            hint,
        },
        Err(e) => Check {
            name,
            ok: false,
            detail: e.to_string(),
            hint,
        },
    }
}

const GCLOUD_HINT: &str = "install the Google Cloud CLI: https://cloud.google.com/sdk/docs/install";
const GIT_HINT: &str = "install git (apt install git / dnf install git)";
const DOCKER_HINT: &str = "install Docker Engine: https://docs.docker.com/engine/install/";
const COMPOSE_HINT: &str = "install the Docker Compose v2 plugin (docker-compose-plugin)";

/// Run every check for `mode`.
pub fn checks(r: &dyn Runner, mode: Mode) -> Vec<Check> {
    let mut out = vec![tool(
        r,
        "gcloud",
        Cmd::new("gcloud").args(["--version"]),
        GCLOUD_HINT,
    )];
    match mode {
        Mode::Gcp => {
            let mut c = tool(
                r,
                "gcloud login",
                Cmd::new("gcloud").args([
                    "auth",
                    "list",
                    "--filter=status:ACTIVE",
                    "--format=value(account)",
                ]),
                "log in with your own account: gcloud auth login",
            );
            if c.ok && c.detail.is_empty() && !r.is_dry_run() {
                c.ok = false;
                c.detail = "no active account".into();
            } else if c.ok && c.detail.ends_with(".gserviceaccount.com") {
                c.ok = false;
                c.detail = format!("active account {} is a service account", c.detail);
                c.hint = "run gcp init with your own (admin) login: gcloud auth login";
            }
            out.push(c);
        }
        Mode::Server | Mode::Agent => {
            out.push(tool(
                r,
                "git",
                Cmd::new("git").args(["--version"]),
                GIT_HINT,
            ));
            out.push(tool(
                r,
                "docker",
                Cmd::new("docker").args(["version", "--format", "{{.Server.Version}}"]),
                DOCKER_HINT,
            ));
            out.push(tool(
                r,
                "docker compose",
                Cmd::new("docker").args(["compose", "version", "--short"]),
                COMPOSE_HINT,
            ));
        }
    }
    out
}

/// Print the report; error naming everything that is missing.
pub fn require(r: &dyn Runner, mode: Mode) -> anyhow::Result<()> {
    let results = checks(r, mode);
    let missing: Vec<&str> = results.iter().filter(|c| !c.ok).map(|c| c.name).collect();
    if missing.is_empty() {
        return Ok(());
    }
    for c in &results {
        eprintln!("{c}");
    }
    anyhow::bail!(
        "missing or not working: {} (fornax-gcp does not install these; see the hints above)",
        missing.join(", ")
    )
}

/// `fornax-gcp doctor`.
pub fn run(r: &dyn Runner, mode: Mode) -> anyhow::Result<()> {
    let results = checks(r, mode);
    for c in &results {
        println!("{c}");
    }
    let missing: Vec<&str> = results.iter().filter(|c| !c.ok).map(|c| c.name).collect();
    if missing.is_empty() {
        println!("all checks passed");
        Ok(())
    } else {
        anyhow::bail!("missing or not working: {}", missing.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::exec::{ExecError, Output};

    /// Answers by program name; unknown programs are "not installed".
    struct Fake(Mutex<Vec<(&'static str, Output)>>);

    impl Runner for Fake {
        fn run(&self, cmd: &Cmd) -> Result<Output, ExecError> {
            let key = format!(
                "{} {}",
                cmd.program,
                cmd.args.first().map_or("", String::as_str)
            );
            self.0
                .lock()
                .expect("lock")
                .iter()
                .find(|(k, _)| key.starts_with(k))
                .map(|(_, o)| o.clone())
                .ok_or(ExecError::NotFound(cmd.program.clone()))
        }
    }

    fn ok(s: &str) -> Output {
        Output {
            code: 0,
            stdout: s.into(),
            ..Output::default()
        }
    }

    #[test]
    fn server_mode_names_missing_tools() {
        let f = Fake(Mutex::new(vec![
            ("gcloud --version", ok("Google Cloud SDK 500.0.0\n")),
            ("git --version", ok("git version 2.43.0\n")),
        ]));
        let c = checks(&f, Mode::Server);
        let missing: Vec<_> = c.iter().filter(|c| !c.ok).map(|c| c.name).collect();
        assert_eq!(missing, ["docker", "docker compose"]);
        let e = require(&f, Mode::Server).expect_err("missing");
        assert!(e.to_string().contains("docker, docker compose"));
    }

    #[test]
    fn gcp_mode_rejects_service_account_login() {
        let f = Fake(Mutex::new(vec![
            ("gcloud --version", ok("Google Cloud SDK 500.0.0\n")),
            ("gcloud auth", ok("agent@p.iam.gserviceaccount.com\n")),
        ]));
        assert!(require(&f, Mode::Gcp).is_err());
        let f = Fake(Mutex::new(vec![
            ("gcloud --version", ok("Google Cloud SDK 500.0.0\n")),
            ("gcloud auth", ok("admin@example.com\n")),
        ]));
        assert!(require(&f, Mode::Gcp).is_ok());
    }
}
