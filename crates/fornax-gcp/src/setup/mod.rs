// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! Interactive setup commands (`gcp …` on the admin machine, `server init` on the server).
//! See `docs/fornax-gcp.md`, "What each setup command does".

pub mod gcp;
pub mod server;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::config::{self, Config, Shared};
use crate::prompt::Prompter;

/// Starting values for the questions: the existing configuration if there is one, else the
/// BLP example with `deploy.deployment_dir` set to [`config::SUGGESTED_DEPLOYMENT_DIR`].
pub fn suggestions(existing: Option<Config>) -> Config {
    existing.unwrap_or_else(|| {
        let mut c = Config::blp_example();
        c.deploy.deployment_dir = config::SUGGESTED_DEPLOYMENT_DIR.into();
        c
    })
}

/// Load `path` when it exists (it must then be valid), else `None`.
pub fn load_existing(path: &Path) -> Result<Option<Config>> {
    if path.exists() {
        println!("using {} for the suggested values", path.display());
        Config::load(path).map(Some)
    } else {
        Ok(None)
    }
}

/// Every problem of `cfg`, as one error.
pub fn ensure_valid(cfg: &Config) -> Result<()> {
    if let Err(errors) = cfg.validate() {
        let list: Vec<String> = errors.iter().map(ToString::to_string).collect();
        bail!("the configuration is invalid:\n  {}", list.join("\n  "));
    }
    Ok(())
}

/// Validate and write `cfg` to `path` (in `--dry-run`, print it instead).
pub fn write_config(path: &Path, cfg: &Config, dry_run: bool) -> Result<()> {
    ensure_valid(cfg)?;
    let text = cfg.to_toml()?;
    if dry_run {
        would(&format!("write {}:", path.display()));
        for line in text.lines() {
            println!("        | {line}");
        }
    } else {
        std::fs::write(path, &text).with_context(|| format!("writing {}", path.display()))?;
        done(&format!("wrote {}", path.display()));
    }
    Ok(())
}

// ---- one-line progress output ------------------------------------------------------------

pub(crate) fn step(n: u8, total: u8, title: &str) {
    println!("[{n}/{total}] {title}");
}

/// Already as wanted.
pub(crate) fn ok(what: &str) {
    println!("      \u{2713} {what}");
}

/// Changed something.
pub(crate) fn done(what: &str) {
    println!("      created: {what}");
}

/// Would change something (`--dry-run`).
pub(crate) fn would(what: &str) {
    println!("      would {what} (dry-run)");
}

/// Not applicable.
pub(crate) fn skipped(what: &str) {
    println!("      skipped: {what}");
}

/// `done` or `would`.
pub(crate) fn changed(dry_run: bool, what: &str) {
    if dry_run { would(what) } else { done(what) }
}

// ---- validators ---------------------------------------------------------------------------

type Check = Box<dyn Fn(&str) -> std::result::Result<(), String>>;

fn rule(f: fn(&str) -> bool, reason: &'static str) -> Check {
    Box::new(move |s| if f(s) { Ok(()) } else { Err(reason.into()) })
}

const NAME_RULE: &str = "lowercase letters, digits and '-', starting with a letter or digit";

fn nonempty(s: &str) -> Option<&str> {
    (!s.is_empty()).then_some(s)
}

/// Ask for the Google Cloud part: project, topic, subscription, service account, Artifact
/// Registry, repo URL, repos, shared services and the deployment folder.
pub fn ask_gcp_part(p: &dyn Prompter, mut c: Config) -> Result<Config> {
    let project_rule = rule(
        config::is_valid_project_id,
        "6-30 lowercase letters, digits or '-', starting with a letter",
    );
    let pubsub_rule = rule(
        config::is_valid_pubsub_id,
        "3-255 chars [A-Za-z0-9-_.~+%], starting with a letter, not 'goog'",
    );
    let name_rule = rule(config::is_valid_name, NAME_RULE);

    let old_project = c.gcp.project.clone();
    c.gcp.project = p.text("GCP project id", Some(&old_project), &project_rule)?;
    let project_changed = c.gcp.project != old_project;
    if project_changed {
        c.gcp.project_number = None;
    }
    c.gcp.topic = p.text(
        "Pub/Sub topic the repositories publish to",
        Some(&c.gcp.topic),
        &pubsub_rule,
    )?;
    c.gcp.subscription = p.text(
        "Pull subscription the agent reads",
        Some(&c.gcp.subscription),
        &pubsub_rule,
    )?;
    c.gcp.service_account = p.text(
        "Service-account id for the agent (before the @)",
        Some(&c.gcp.service_account),
        &project_rule,
    )?;
    if p.confirm(
        "Give the agent read access to an Artifact Registry repository?",
        c.gcp.artifact_location.is_some(),
    )? {
        let location = p.text(
            "Artifact Registry location (e.g. asia-south1)",
            c.gcp.artifact_location.as_deref(),
            &name_rule,
        )?;
        let repo = p.text(
            "Artifact Registry repository",
            c.gcp.artifact_repository.as_deref(),
            &name_rule,
        )?;
        c.gcp.artifact_location = Some(location);
        c.gcp.artifact_repository = Some(repo);
    } else {
        c.gcp.artifact_location = None;
        c.gcp.artifact_repository = None;
    }
    let url = if project_changed || c.gcp.repo_base_url.is_empty() {
        format!("https://source.developers.google.com/p/{}/r", c.gcp.project)
    } else {
        c.gcp.repo_base_url.clone()
    };
    c.gcp.repo_base_url = p.text("Base URL of the git repositories", Some(&url), &|s| {
        if s.starts_with("https://") && !s.contains(char::is_whitespace) {
            Ok(())
        } else {
            Err("must start with https:// and contain no spaces".into())
        }
    })?;
    c.repos = p.list(
        "App repositories, in deploy order",
        c.repos.clone(),
        &name_rule,
    )?;
    if p.confirm(
        "Shared services kept in one infra repository (tags <app>-vX.Y.Z)?",
        c.shared.is_some(),
    )? {
        let s = c.shared.clone().unwrap_or(Shared {
            repo: String::new(),
            apps: Vec::new(),
            apps_dir: "apps".into(),
            project_prefix: String::new(),
        });
        let repo = p.text("Infra repository", nonempty(&s.repo), &name_rule)?;
        let apps = p.list(
            "Shared apps, deployed first in this order",
            s.apps.clone(),
            &name_rule,
        )?;
        let apps_dir = p.text(
            "Folder in the infra repo holding <app>/compose.yml",
            Some(&s.apps_dir),
            &rule(
                config::is_safe_relative,
                "relative path inside the repo, no '..'",
            ),
        )?;
        let project_prefix = p.text(
            "Compose project prefix for shared apps (may be empty)",
            Some(&s.project_prefix),
            &|v| {
                if v.is_empty() || config::is_valid_name(v) {
                    Ok(())
                } else {
                    Err(NAME_RULE.into())
                }
            },
        )?;
        c.shared = Some(Shared {
            repo,
            apps,
            apps_dir,
            project_prefix,
        });
    } else {
        c.shared = None;
    }
    c.deploy.deployment_dir = p.text(
        "Deployment folder in each app repo (compose.yml + release.vars)",
        Some(&c.deploy.deployment_dir),
        &rule(
            config::is_safe_relative,
            "relative path inside the repo, no '..'",
        ),
    )?;
    Ok(c)
}

/// `new_base/<rel>` when `path` was `old_base/<rel>`, else `path` unchanged.
fn rebase(path: &Path, old_base: &Path, new_base: &Path) -> PathBuf {
    path.strip_prefix(old_base)
        .map_or_else(|_| path.to_path_buf(), |rel| new_base.join(rel))
}

/// Ask for the server part: deploy user and every server path (key path absolute).
pub fn ask_server_part(p: &dyn Prompter, mut c: Config) -> Result<Config> {
    let d = &mut c.deploy;
    d.user = p.text(
        "Deploy user (runs the agent; must be in the docker group)",
        Some(&d.user),
        &rule(config::is_valid_user, "not a valid Linux user name"),
    )?;
    let old_base = d.base_dir.clone();
    d.base_dir = p.path(
        "Agent working folder (clones, releases, state, locks)",
        Some(&old_base),
        true,
    )?;
    let base = d.base_dir.clone();
    // Suggest the code folder next to the new working folder when it was next to the old one.
    let apps_suggestion = match (old_base.parent(), d.apps_dir.file_name(), base.parent()) {
        (Some(op), Some(name), Some(np)) if d.apps_dir.parent() == Some(op) => np.join(name),
        _ => d.apps_dir.clone(),
    };
    let apps = p.text(
        "Code folder (one checkout per repo, mounted as ${APP_DIR})",
        Some(&apps_suggestion.to_string_lossy()),
        &|s| {
            let path = Path::new(s);
            if !path.is_absolute() {
                Err("must be an absolute path (starting with /)".into())
            } else if path.starts_with(&base) {
                Err(format!("must not be inside {}", base.display()))
            } else {
                Ok(())
            }
        },
    )?;
    d.apps_dir = PathBuf::from(apps);
    let old_secrets = d.secrets_dir.clone();
    d.secrets_dir = p.path(
        "Secrets folder (server-only <name>.env files)",
        Some(&rebase(&old_secrets, &old_base, &base)),
        true,
    )?;
    let secrets = d.secrets_dir.clone();
    if p.confirm(
        "Use an env file shared by every stack (passed as --env-file, ${SHARED_ENV_FILE})?",
        d.shared_env_file.is_some(),
    )? {
        let suggestion = d.shared_env_file.as_deref().map_or_else(
            || secrets.join("shared.env"),
            |f| rebase(f, &old_secrets, &secrets),
        );
        d.shared_env_file = Some(p.path("Shared env file", Some(&suggestion), true)?);
    } else {
        d.shared_env_file = None;
    }
    let key_suggestion = rebase(&d.service_account_key, &old_secrets, &secrets);
    d.service_account_key = p.path(
        "Absolute path of the service-account key on this server",
        Some(&key_suggestion),
        true,
    )?;
    d.gcloud_config_dir = p.path(
        "gcloud folder of the agent (CLOUDSDK_CONFIG)",
        Some(&rebase(&d.gcloud_config_dir, &old_base, &base)),
        true,
    )?;
    Ok(c)
}

#[cfg(test)]
pub(crate) mod fake {
    //! A recording [`Runner`] for the setup tests.

    use std::sync::Mutex;

    use crate::exec::{Cmd, ExecError, Output, Runner};

    type Respond = Box<dyn Fn(&Cmd) -> Output + Send + Sync>;

    /// Records every command and answers with `respond`.
    pub struct FakeRunner {
        /// Every command run, in order.
        pub calls: Mutex<Vec<Cmd>>,
        respond: Respond,
        dry: bool,
    }

    impl FakeRunner {
        /// A runner answering with `respond`.
        pub fn new(respond: impl Fn(&Cmd) -> Output + Send + Sync + 'static) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                respond: Box::new(respond),
                dry: false,
            }
        }

        /// Like `DryRunner`: empty successful output, `is_dry_run()`.
        pub fn dry() -> Self {
            Self {
                dry: true,
                ..Self::new(|_| Output::default())
            }
        }

        /// `program arg arg …` of every call.
        pub fn lines(&self) -> Vec<String> {
            self.calls.lock().expect("lock").iter().map(line).collect()
        }
    }

    /// `program arg arg …` (no quoting, no env).
    pub fn line(c: &Cmd) -> String {
        std::iter::once(c.program.as_str())
            .chain(c.args.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ")
    }

    impl Runner for FakeRunner {
        fn run(&self, cmd: &Cmd) -> Result<Output, ExecError> {
            self.calls.lock().expect("lock").push(cmd.clone());
            Ok((self.respond)(cmd))
        }

        fn is_dry_run(&self) -> bool {
            self.dry
        }
    }

    /// Successful output.
    pub fn out(stdout: &str) -> Output {
        Output {
            code: 0,
            stdout: stdout.into(),
            ..Output::default()
        }
    }

    /// Failed output with `stderr`.
    pub fn fail(stderr: &str) -> Output {
        Output {
            code: 1,
            stderr: stderr.into(),
            ..Output::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt::ScriptedPrompter;

    #[test]
    fn suggestions_use_fornax_dir_without_config() {
        let s = suggestions(None);
        assert_eq!(s.deploy.deployment_dir, ".fornax");
        assert_eq!(s.gcp.project, "visual-analytics-1");
        let mut existing = Config::blp_example();
        existing.deploy.deployment_dir = ".deployment".into();
        assert_eq!(
            suggestions(Some(existing)).deploy.deployment_dir,
            ".deployment"
        );
    }

    #[test]
    fn server_part_rebases_paths_on_new_base() {
        // user, base, apps, secrets, shared env?, shared env path, key, gcloud
        let p = ScriptedPrompter::new(&[
            "svc",
            "/srv/x/deploy",
            "/srv/x/deploy/apps",
            "",
            "",
            "",
            "",
            "",
            "",
        ]);
        let c = ask_server_part(&p, Config::blp_example()).expect("ask");
        let d = &c.deploy;
        assert_eq!(d.user, "svc");
        assert_eq!(
            d.apps_dir,
            PathBuf::from("/srv/x/apps"),
            "apps inside base re-asked"
        );
        assert_eq!(d.secrets_dir, PathBuf::from("/srv/x/deploy/secrets"));
        assert_eq!(
            d.shared_env_file.as_deref(),
            Some(Path::new("/srv/x/deploy/secrets/ppe.env"))
        );
        assert_eq!(
            d.service_account_key,
            PathBuf::from("/srv/x/deploy/secrets/srf-blp-deploy-agent-key.json")
        );
        assert_eq!(d.gcloud_config_dir, PathBuf::from("/srv/x/deploy/gcloud"));
        assert_eq!(p.remaining(), 0);
    }
}
