// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! The `fornax-gcp` configuration file (TOML, no secrets).
//!
//! `gcp init` writes it on the admin machine, `server init` completes it on the server and
//! installs it as `/etc/fornax-gcp/config.toml`. See `docs/fornax-gcp.md`.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Default location on the server.
pub const DEFAULT_SERVER_CONFIG: &str = "/etc/fornax-gcp/config.toml";
/// Default location on the admin machine (current directory).
pub const DEFAULT_ADMIN_CONFIG: &str = "fornax-gcp.toml";
/// Deployment folder suggested to new projects (BLP uses `.deployment`).
pub const SUGGESTED_DEPLOYMENT_DIR: &str = ".fornax";

/// Whole configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Google Cloud resources.
    pub gcp: Gcp,
    /// App repositories in deploy order (tags `vX.Y.Z`).
    #[serde(default)]
    pub repos: Vec<String>,
    /// Optional shared services kept in one infra repository (tags `<app>-vX.Y.Z`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shared: Option<Shared>,
    /// Server-side paths and identity.
    pub deploy: Deploy,
    /// Agent timing and behaviour.
    #[serde(default)]
    pub agent: AgentSettings,
}

/// Google Cloud resources used by the agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Gcp {
    /// Project id, e.g. `visual-analytics-1`.
    pub project: String,
    /// Project number (looked up by `gcp init`); needed for the CSR publisher account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_number: Option<String>,
    /// Pub/Sub topic that Cloud Source Repositories publishes to.
    pub topic: String,
    /// Pull subscription the agent reads.
    pub subscription: String,
    /// Service-account id (the part before `@`).
    pub service_account: String,
    /// Artifact Registry location, e.g. `asia-south1` (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_location: Option<String>,
    /// Artifact Registry repository, e.g. `srf-vision-ai` (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_repository: Option<String>,
    /// Base URL of the git repositories, e.g.
    /// `https://source.developers.google.com/p/<project>/r`.
    pub repo_base_url: String,
}

impl Gcp {
    /// `<id>@<project>.iam.gserviceaccount.com`.
    pub fn service_account_email(&self) -> String {
        format!(
            "{}@{}.iam.gserviceaccount.com",
            self.service_account, self.project
        )
    }

    /// Clone URL of a repository.
    pub fn repo_url(&self, repo: &str) -> String {
        format!("{}/{repo}", self.repo_base_url.trim_end_matches('/'))
    }

    /// `<location>-docker.pkg.dev` if Artifact Registry is configured.
    pub fn docker_registry_host(&self) -> Option<String> {
        self.artifact_location
            .as_ref()
            .map(|l| format!("{l}-docker.pkg.dev"))
    }
}

/// Shared services kept in one infra repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Shared {
    /// Infra repository name.
    pub repo: String,
    /// App names, deployed in this order before the app repos.
    pub apps: Vec<String>,
    /// Folder in the infra repo holding `<app>/compose.yml`.
    #[serde(default = "default_shared_apps_dir")]
    pub apps_dir: String,
    /// Compose project = `<project_prefix><app>`.
    #[serde(default)]
    pub project_prefix: String,
}

fn default_shared_apps_dir() -> String {
    "apps".into()
}

/// Server-side paths and identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Deploy {
    /// Folder in each app repository with `compose.yml` + `release.vars`.
    pub deployment_dir: String,
    /// User that runs the agent (must be in the `docker` group).
    pub user: String,
    /// Clones, releases, state, locks.
    pub base_dir: PathBuf,
    /// Code checked out per repository, mounted by compose as `${APP_DIR}`.
    pub apps_dir: PathBuf,
    /// Server-only secrets (`<name>.env`, shared env file).
    pub secrets_dir: PathBuf,
    /// Env file shared by every stack (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shared_env_file: Option<PathBuf>,
    /// Absolute path of the service-account key on the server.
    pub service_account_key: PathBuf,
    /// `CLOUDSDK_CONFIG` of the agent (holds only the service-account login).
    pub gcloud_config_dir: PathBuf,
}

impl Deploy {
    /// `<base_dir>/repos`.
    pub fn repos_dir(&self) -> PathBuf {
        self.base_dir.join("repos")
    }
    /// `<base_dir>/repo.git` (bare clone of the infra repo).
    pub fn infra_git_dir(&self) -> PathBuf {
        self.base_dir.join("repo.git")
    }
    /// `<base_dir>/state`.
    pub fn state_dir(&self) -> PathBuf {
        self.base_dir.join("state")
    }
    /// `<base_dir>/locks`.
    pub fn locks_dir(&self) -> PathBuf {
        self.base_dir.join("locks")
    }
    /// `<base_dir>/releases`.
    pub fn releases_dir(&self) -> PathBuf {
        self.base_dir.join("releases")
    }
}

/// Agent timing and behaviour.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AgentSettings {
    /// Reconcile at least this often (seconds).
    pub reconcile_interval_secs: u64,
    /// Max time one Pub/Sub pull may block (seconds).
    pub pull_timeout_secs: u64,
    /// Max time one deploy may take (seconds).
    pub deploy_timeout_secs: u64,
    /// Default wait for healthy containers (seconds); `HEALTH_TIMEOUT` in release.vars wins.
    pub health_timeout_secs: u64,
    /// Release folders kept per app.
    pub keep_releases: usize,
    /// Command receiving `subject\n\nbody` on stdin after each deploy (optional).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub notify_cmd: String,
}

impl Default for AgentSettings {
    fn default() -> Self {
        Self {
            reconcile_interval_secs: 300,
            pull_timeout_secs: 120,
            deploy_timeout_secs: 1800,
            health_timeout_secs: 120,
            keep_releases: 5,
            notify_cmd: String::new(),
        }
    }
}

/// A configuration problem (field + reason).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{field}: {reason}")]
pub struct ConfigError {
    /// Dotted field name, e.g. `deploy.base_dir`.
    pub field: String,
    /// What is wrong.
    pub reason: String,
}

fn err(field: &str, reason: impl Into<String>) -> ConfigError {
    ConfigError {
        field: field.into(),
        reason: reason.into(),
    }
}

/// App, repository and shared-service names: `^[a-z0-9][a-z0-9-]*$` (as agent.py).
pub fn is_valid_name(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && b.len() <= 100
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

/// GCP project id: 6–30 chars, lowercase letters, digits, hyphens, starts with a letter.
pub fn is_valid_project_id(id: &str) -> bool {
    let b = id.as_bytes();
    (6..=30).contains(&b.len())
        && b[0].is_ascii_lowercase()
        && !id.ends_with('-')
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

/// Service-account id: 6–30 chars, lowercase letters, digits, hyphens, starts with a letter.
pub fn is_valid_service_account_id(id: &str) -> bool {
    is_valid_project_id(id)
}

/// Pub/Sub topic / subscription id: 3–255 chars, starts with a letter, `[A-Za-z0-9-_.~+%]`.
pub fn is_valid_pubsub_id(id: &str) -> bool {
    let b = id.as_bytes();
    (3..=255).contains(&b.len())
        && b[0].is_ascii_alphabetic()
        && !id.starts_with("goog")
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || b"-_.~+%".contains(c))
}

/// A relative path inside a repository without `..`, e.g. `.deployment` or `apps`.
pub fn is_safe_relative(path: &str) -> bool {
    !path.is_empty()
        && Path::new(path)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
}

/// A Linux user name.
pub fn is_valid_user(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && b.len() <= 32
        && (b[0].is_ascii_lowercase() || b[0] == b'_')
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"-_".contains(c))
}

impl Config {
    /// Parse and validate a TOML file.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
        let config: Self = toml::from_str(&text)
            .map_err(|e| anyhow::anyhow!("parsing {}: {e}", path.display()))?;
        if let Err(errors) = config.validate() {
            let list: Vec<String> = errors.iter().map(ToString::to_string).collect();
            anyhow::bail!("{} is invalid:\n  {}", path.display(), list.join("\n  "));
        }
        Ok(config)
    }

    /// Serialise to TOML.
    pub fn to_toml(&self) -> anyhow::Result<String> {
        Ok(toml::to_string_pretty(self)?)
    }

    /// All problems, or `Ok` when the configuration is usable.
    pub fn validate(&self) -> Result<(), Vec<ConfigError>> {
        let mut e = Vec::new();
        let g = &self.gcp;
        if !is_valid_project_id(&g.project) {
            e.push(err("gcp.project", "not a valid GCP project id"));
        }
        if let Some(n) = &g.project_number
            && (n.is_empty() || !n.bytes().all(|c| c.is_ascii_digit()))
        {
            e.push(err("gcp.project_number", "must be digits"));
        }
        if !is_valid_pubsub_id(&g.topic) {
            e.push(err("gcp.topic", "not a valid Pub/Sub topic id"));
        }
        if !is_valid_pubsub_id(&g.subscription) {
            e.push(err(
                "gcp.subscription",
                "not a valid Pub/Sub subscription id",
            ));
        }
        if !is_valid_service_account_id(&g.service_account) {
            e.push(err(
                "gcp.service_account",
                "6-30 lowercase letters, digits or '-', starting with a letter",
            ));
        }
        if g.artifact_location.is_some() != g.artifact_repository.is_some() {
            e.push(err(
                "gcp.artifact_repository",
                "set both artifact_location and artifact_repository, or neither",
            ));
        }
        if !g.repo_base_url.starts_with("https://") {
            e.push(err("gcp.repo_base_url", "must start with https://"));
        }

        let mut seen = HashSet::new();
        let shared_apps = self
            .shared
            .as_ref()
            .map(|s| s.apps.as_slice())
            .unwrap_or(&[]);
        for name in self.repos.iter().chain(shared_apps) {
            if !is_valid_name(name) {
                e.push(err(
                    "repos",
                    format!("'{name}' must match ^[a-z0-9][a-z0-9-]*$"),
                ));
            }
            if !seen.insert(name.as_str()) {
                e.push(err(
                    "repos",
                    format!("'{name}' appears twice (repos / shared.apps)"),
                ));
            }
        }
        if self.repos.is_empty() && shared_apps.is_empty() {
            e.push(err("repos", "name at least one repository or shared app"));
        }
        if let Some(s) = &self.shared {
            if !is_valid_name(&s.repo) {
                e.push(err("shared.repo", "must match ^[a-z0-9][a-z0-9-]*$"));
            }
            if !is_safe_relative(&s.apps_dir) {
                e.push(err(
                    "shared.apps_dir",
                    "relative path inside the repo, no '..'",
                ));
            }
            if !s.project_prefix.is_empty() && !is_valid_name(&s.project_prefix) {
                e.push(err(
                    "shared.project_prefix",
                    "lowercase letters, digits, '-'",
                ));
            }
        }

        let d = &self.deploy;
        if !is_safe_relative(&d.deployment_dir) {
            e.push(err(
                "deploy.deployment_dir",
                "relative path inside the repo, no '..'",
            ));
        }
        if !is_valid_user(&d.user) {
            e.push(err("deploy.user", "not a valid Linux user name"));
        }
        for (field, path) in [
            ("deploy.base_dir", &d.base_dir),
            ("deploy.apps_dir", &d.apps_dir),
            ("deploy.secrets_dir", &d.secrets_dir),
            ("deploy.service_account_key", &d.service_account_key),
            ("deploy.gcloud_config_dir", &d.gcloud_config_dir),
        ] {
            if !path.is_absolute() {
                e.push(err(field, "must be an absolute path"));
            }
        }
        if let Some(p) = &d.shared_env_file
            && !p.is_absolute()
        {
            e.push(err("deploy.shared_env_file", "must be an absolute path"));
        }
        if d.apps_dir.starts_with(&d.base_dir) {
            e.push(err("deploy.apps_dir", "must not be inside deploy.base_dir"));
        }

        let a = &self.agent;
        if a.reconcile_interval_secs < 30 {
            e.push(err("agent.reconcile_interval_secs", "at least 30"));
        }
        if a.keep_releases < 2 {
            e.push(err("agent.keep_releases", "at least 2 (live + previous)"));
        }
        if !(10..=3600).contains(&a.health_timeout_secs) {
            e.push(err("agent.health_timeout_secs", "10..=3600"));
        }
        if e.is_empty() { Ok(()) } else { Err(e) }
    }

    /// The BLP production values, used as suggestions by the interactive prompts and tests.
    pub fn blp_example() -> Self {
        Self {
            gcp: Gcp {
                project: "visual-analytics-1".into(),
                project_number: Some("121549680248".into()),
                topic: "srf-source-repo-events".into(),
                subscription: "srf-onprem-pull-subscription".into(),
                service_account: "srf-blp-deploy-agent".into(),
                artifact_location: Some("asia-south1".into()),
                artifact_repository: Some("srf-vision-ai".into()),
                repo_base_url: "https://source.developers.google.com/p/visual-analytics-1/r".into(),
            },
            repos: [
                "srf-blp-kafka-init",
                "srf-blp-db-migrate",
                "srf-blp-pg-consumer",
                "srf-blp-aggregator",
                "srf-blp-api",
                "srf-blp-deepstream-app",
                "srf-blp-ui",
            ]
            .map(String::from)
            .to_vec(),
            shared: Some(Shared {
                repo: "srf-blp-infra-configuration".into(),
                apps: vec!["platform".into(), "testcameras".into()],
                apps_dir: "apps".into(),
                project_prefix: "srf-blp-".into(),
            }),
            deploy: Deploy {
                deployment_dir: ".deployment".into(),
                user: "blpdevops".into(),
                base_dir: "/data/blp/deploy".into(),
                apps_dir: "/data/blp/apps".into(),
                secrets_dir: "/data/blp/deploy/secrets".into(),
                shared_env_file: Some("/data/blp/deploy/secrets/ppe.env".into()),
                service_account_key: "/data/blp/deploy/secrets/srf-blp-deploy-agent-key.json"
                    .into(),
                gcloud_config_dir: "/data/blp/deploy/gcloud".into(),
            },
            agent: AgentSettings::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blp_example_is_valid_and_round_trips() {
        let c = Config::blp_example();
        assert_eq!(c.validate(), Ok(()));
        let text = c.to_toml().expect("toml");
        let back: Config = toml::from_str(&text).expect("parse");
        assert_eq!(back, c);
        assert_eq!(
            c.gcp.service_account_email(),
            "srf-blp-deploy-agent@visual-analytics-1.iam.gserviceaccount.com"
        );
        assert_eq!(
            c.gcp.repo_url("srf-blp-ui"),
            "https://source.developers.google.com/p/visual-analytics-1/r/srf-blp-ui"
        );
        assert_eq!(
            c.gcp.docker_registry_host().as_deref(),
            Some("asia-south1-docker.pkg.dev")
        );
    }

    #[test]
    fn rejects_bad_values() {
        let mut c = Config::blp_example();
        c.repos.push("Bad_Name".into());
        c.repos.push("srf-blp-ui".into());
        c.deploy.deployment_dir = "../x".into();
        c.deploy.service_account_key = "relative/key.json".into();
        c.deploy.apps_dir = "/data/blp/deploy/apps".into();
        c.gcp.artifact_repository = None;
        c.agent.keep_releases = 1;
        let errors = c.validate().expect_err("invalid");
        let fields: Vec<_> = errors.iter().map(|e| e.field.as_str()).collect();
        for f in [
            "repos",
            "deploy.deployment_dir",
            "deploy.service_account_key",
            "deploy.apps_dir",
            "gcp.artifact_repository",
            "agent.keep_releases",
        ] {
            assert!(fields.contains(&f), "{f} not reported: {errors:?}");
        }
        assert!(
            errors.iter().filter(|e| e.field == "repos").count() >= 2,
            "bad name and duplicate"
        );
    }

    #[test]
    fn name_rules() {
        assert!(is_valid_name("srf-blp-ui") && is_valid_name("0app"));
        assert!(
            !is_valid_name("-x")
                && !is_valid_name("A")
                && !is_valid_name("a_b")
                && !is_valid_name("")
        );
        assert!(is_valid_project_id("visual-analytics-1") && !is_valid_project_id("short"));
        assert!(
            is_valid_pubsub_id("srf-onprem-pull-subscription") && !is_valid_pubsub_id("goog-x")
        );
        assert!(is_safe_relative(".deployment") && is_safe_relative("apps/x"));
        assert!(!is_safe_relative("/abs") && !is_safe_relative("a/../b") && !is_safe_relative(""));
        assert!(is_valid_user("blpdevops") && !is_valid_user("Root") && !is_valid_user("a b"));
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let mut text = Config::blp_example().to_toml().expect("toml");
        text.push_str("\n[extra]\nx = 1\n");
        assert!(toml::from_str::<Config>(&text).is_err());
    }
}
