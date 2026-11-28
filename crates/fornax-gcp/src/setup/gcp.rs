// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! `fornax-gcp gcp …` — Google Cloud side, run on an admin machine with the engineer's own
//! gcloud login. Every step checks what exists first and creates only what is missing.

use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use super::{ask_gcp_part, changed, load_existing, ok, skipped, step, suggestions, write_config};
use crate::config::{self, Config, Gcp};
use crate::doctor;
use crate::exec::{Cmd, Output, Runner};
use crate::prompt::{DialoguerPrompter, Prompter};

const INIT_STEPS: u8 = 10;

/// `gcp init` on the terminal.
pub fn init(r: &dyn Runner, config: &Path, yes: bool) -> Result<()> {
    init_with(r, &DialoguerPrompter { yes }, config)
}

/// `gcp add-repo NAME` on the terminal.
pub fn add_repo(r: &dyn Runner, config: &Path, name: &str, yes: bool) -> Result<()> {
    add_repo_with(r, &DialoguerPrompter { yes }, config, name)
}

/// `gcp create-key` on the terminal.
pub fn create_key(r: &dyn Runner, config: &Path, out: Option<&Path>, yes: bool) -> Result<()> {
    create_key_with(r, &DialoguerPrompter { yes }, config, out)
}

/// gcloud with `--project` always appended.
struct Gcloud<'a> {
    r: &'a dyn Runner,
    project: String,
}

impl Gcloud<'_> {
    fn cmd(&self, args: &[&str]) -> Cmd {
        Cmd::new("gcloud")
            .args(args.iter().copied())
            .args(["--project", self.project.as_str()])
    }

    fn dry(&self) -> bool {
        self.r.is_dry_run()
    }

    fn run(&self, args: &[&str]) -> Result<Output> {
        let cmd = self.cmd(args);
        self.r
            .run(&cmd)
            .with_context(|| format!("running {}", cmd.display()))
    }

    /// Run; error with gcloud's message when it fails.
    fn run_ok(&self, args: &[&str]) -> Result<Output> {
        let out = self.run(args)?;
        if !out.ok() {
            bail!(
                "{} failed: {}",
                self.cmd(args).display(),
                gcloud_message(&out)
            );
        }
        Ok(out)
    }

    /// Run a change; IAM changes right after creating the service account can fail until it
    /// has propagated, so "does not exist" is retried a few times.
    fn change(&self, args: &[&str]) -> Result<Output> {
        let mut attempt = 0;
        loop {
            let out = self.run(args)?;
            if out.ok() || self.dry() {
                return Ok(out);
            }
            attempt += 1;
            if attempt >= 6 || !out.stderr.to_ascii_lowercase().contains("does not exist") {
                bail!(
                    "{} failed: {}",
                    self.cmd(args).display(),
                    gcloud_message(&out)
                );
            }
            std::thread::sleep(Duration::from_secs(5));
        }
    }

    /// `describe` something: true when it exists. In `--dry-run` empty output means unknown,
    /// reported as missing ("would create").
    fn exists(&self, args: &[&str], what: &str) -> Result<Option<String>> {
        let out = self.run(args)?;
        if out.ok() {
            let value = out.stdout.trim().to_owned();
            if value.is_empty() && self.dry() {
                return Ok(None);
            }
            return Ok(Some(value));
        }
        if is_not_found(&out.stderr) {
            Ok(None)
        } else {
            bail!("checking {what}: {}", gcloud_message(&out))
        }
    }
}

fn gcloud_message(out: &Output) -> String {
    let msg = if out.stderr.trim().is_empty() {
        out.stdout.trim()
    } else {
        out.stderr.trim()
    };
    if msg.is_empty() {
        format!("exit code {}", out.code)
    } else {
        msg.to_owned()
    }
}

fn is_not_found(stderr: &str) -> bool {
    let s = stderr.to_ascii_lowercase();
    s.contains("not_found") || s.contains("not found") || s.contains("notfound")
}

/// `gcp init` with an injectable prompter (the 10 steps of `docs/fornax-gcp.md`).
pub fn init_with(r: &dyn Runner, p: &dyn Prompter, config_path: &Path) -> Result<()> {
    let dry = r.is_dry_run();
    step(1, INIT_STEPS, "doctor --mode gcp");
    doctor::require(r, doctor::Mode::Gcp)?;
    ok("gcloud installed and logged in with a user account");

    let existing = load_existing(config_path)?;
    let mut cfg = ask_gcp_part(p, suggestions(existing))?;
    super::ensure_valid(&cfg)?;
    let g = Gcloud {
        r,
        project: cfg.gcp.project.clone(),
    };

    step(2, INIT_STEPS, "enable APIs");
    enable_apis(&g, &cfg.gcp)?;

    step(3, INIT_STEPS, "project number");
    let out = g.run_ok(&[
        "projects",
        "describe",
        &cfg.gcp.project,
        "--format=value(projectNumber)",
    ])?;
    let number = out.stdout.trim();
    if number.is_empty() && dry {
        skipped("unknown in --dry-run (kept the configured value)");
    } else if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
        bail!(
            "unexpected project number '{number}' for {}",
            cfg.gcp.project
        );
    } else {
        ok(&format!("{} = {number}", cfg.gcp.project));
        cfg.gcp.project_number = Some(number.to_owned());
    }

    step(4, INIT_STEPS, "topic and pull subscription");
    ensure_topic(&g, &cfg.gcp)?;
    ensure_subscription(&g, &cfg.gcp)?;

    step(5, INIT_STEPS, "service account");
    ensure_service_account(&g, &cfg.gcp)?;

    let member = format!("serviceAccount:{}", cfg.gcp.service_account_email());
    step(
        6,
        INIT_STEPS,
        "agent bindings (subscriber, Artifact Registry reader)",
    );
    ensure_binding(
        &g,
        &["pubsub", "subscriptions"],
        &cfg.gcp.subscription,
        &[],
        &member,
        "roles/pubsub.subscriber",
    )?;
    match (&cfg.gcp.artifact_location, &cfg.gcp.artifact_repository) {
        (Some(location), Some(repo)) => ensure_binding(
            &g,
            &["artifacts", "repositories"],
            repo,
            &["--location", location],
            &member,
            "roles/artifactregistry.reader",
        )?,
        _ => skipped("no Artifact Registry repository configured"),
    }

    step(
        7,
        INIT_STEPS,
        "publisher binding for Cloud Source Repositories",
    );
    let number = cfg
        .gcp
        .project_number
        .clone()
        .unwrap_or_else(|| "PROJECT_NUMBER".into());
    ensure_binding(
        &g,
        &["pubsub", "topics"],
        &cfg.gcp.topic,
        &[],
        &format!("serviceAccount:{number}-compute@developer.gserviceaccount.com"),
        "roles/pubsub.publisher",
    )?;

    step(8, INIT_STEPS, "repository read access + topic");
    for repo in watched_repos(&cfg) {
        repo_access(&g, &cfg.gcp, &repo)?;
    }

    step(9, INIT_STEPS, "configuration file");
    write_config(config_path, &cfg, dry)?;
    if !dry {
        println!(
            "      copy it to the server and run there: sudo fornax-gcp server init --config <file>"
        );
    }

    step(10, INIT_STEPS, "service-account key");
    if dry || p.assume_yes() {
        skipped(&format!(
            "create it with: fornax-gcp gcp create-key --out /abs/path/key.json --config {}",
            config_path.display()
        ));
    } else if p.confirm("Create a key for the service account now?", false)? {
        create_key_with(r, p, config_path, None)?;
    } else {
        skipped("later: fornax-gcp gcp create-key --out /abs/path/key.json");
    }
    Ok(())
}

/// The infra repo (if any) followed by the app repos.
fn watched_repos(cfg: &Config) -> Vec<String> {
    cfg.shared
        .iter()
        .map(|s| s.repo.clone())
        .chain(cfg.repos.iter().cloned())
        .collect()
}

fn enable_apis(g: &Gcloud<'_>, gcp: &Gcp) -> Result<()> {
    let mut wanted = vec![
        "pubsub.googleapis.com",
        "sourcerepo.googleapis.com",
        "iam.googleapis.com",
    ];
    if gcp.artifact_location.is_some() {
        wanted.push("artifactregistry.googleapis.com");
    }
    let out = g.run_ok(&[
        "services",
        "list",
        "--enabled",
        "--format=value(config.name)",
    ])?;
    let enabled: Vec<&str> = out.stdout.lines().map(str::trim).collect();
    let missing: Vec<&str> = wanted
        .iter()
        .copied()
        .filter(|w| !enabled.contains(w))
        .collect();
    if missing.is_empty() {
        ok(&format!("enabled: {}", wanted.join(", ")));
        return Ok(());
    }
    let mut args = vec!["services", "enable"];
    args.extend(&missing);
    g.run_ok(&args)?;
    changed(g.dry(), &format!("enable {}", missing.join(", ")));
    Ok(())
}

fn ensure_topic(g: &Gcloud<'_>, gcp: &Gcp) -> Result<()> {
    let what = format!("topic {}", gcp.topic);
    if g.exists(
        &[
            "pubsub",
            "topics",
            "describe",
            &gcp.topic,
            "--format=value(name)",
        ],
        &what,
    )?
    .is_some()
    {
        ok(&format!("{what} exists"));
        return Ok(());
    }
    g.run_ok(&["pubsub", "topics", "create", &gcp.topic])?;
    changed(g.dry(), &format!("create {what}"));
    Ok(())
}

fn ensure_subscription(g: &Gcloud<'_>, gcp: &Gcp) -> Result<()> {
    let what = format!("subscription {}", gcp.subscription);
    let topic = g.exists(
        &[
            "pubsub",
            "subscriptions",
            "describe",
            &gcp.subscription,
            "--format=value(topic)",
        ],
        &what,
    )?;
    if let Some(topic) = topic {
        if !topic.ends_with(&format!("/topics/{}", gcp.topic)) {
            bail!(
                "{what} exists but reads {topic}, not topic {}; pick another subscription name",
                gcp.topic
            );
        }
        ok(&format!("{what} exists (topic {})", gcp.topic));
        return Ok(());
    }
    g.run_ok(&[
        "pubsub",
        "subscriptions",
        "create",
        &gcp.subscription,
        "--topic",
        &gcp.topic,
        "--ack-deadline",
        "60",
        "--message-retention-duration",
        "7d",
        "--expiration-period",
        "never",
    ])?;
    changed(
        g.dry(),
        &format!("create {what} (pull, ack 60 s, retention 7 d, never expires)"),
    );
    Ok(())
}

fn ensure_service_account(g: &Gcloud<'_>, gcp: &Gcp) -> Result<()> {
    let email = gcp.service_account_email();
    let what = format!("service account {email}");
    if g.exists(
        &[
            "iam",
            "service-accounts",
            "describe",
            &email,
            "--format=value(email)",
        ],
        &what,
    )?
    .is_some()
    {
        ok(&format!("{what} exists"));
        return Ok(());
    }
    let display_name = format!("fornax-gcp deploy agent ({})", gcp.service_account);
    g.run_ok(&[
        "iam",
        "service-accounts",
        "create",
        &gcp.service_account,
        "--display-name",
        &display_name,
    ])?;
    changed(g.dry(), &format!("create {what}"));
    Ok(())
}

/// `get-iam-policy` first; `add-iam-policy-binding` only when the member lacks the role.
fn ensure_binding(
    g: &Gcloud<'_>,
    resource: &[&str],
    name: &str,
    extra: &[&str],
    member: &str,
    role: &str,
) -> Result<()> {
    let mut get: Vec<&str> = resource.to_vec();
    get.extend(["get-iam-policy", name]);
    get.extend(extra);
    get.push("--format=json");
    let out = g.run_ok(&get)?;
    let what = format!("{role} for {member} on {name}");
    if policy_has_member(&out.stdout, role, member)? {
        ok(&what);
        return Ok(());
    }
    let mut add: Vec<&str> = resource.to_vec();
    add.extend(["add-iam-policy-binding", name]);
    add.extend(extra);
    add.extend(["--member", member, "--role", role]);
    g.change(&add)?;
    changed(g.dry(), &format!("bind {what}"));
    Ok(())
}

fn parse_policy(text: &str) -> Result<Value> {
    if text.trim().is_empty() {
        return Ok(json!({}));
    }
    let v: Value = serde_json::from_str(text).context("parsing the IAM policy JSON from gcloud")?;
    if !v.is_object() {
        bail!("the IAM policy from gcloud is not a JSON object");
    }
    Ok(v)
}

fn is_plain_binding(b: &Value, role: &str) -> bool {
    b.get("role").and_then(Value::as_str) == Some(role) && b.get("condition").is_none()
}

/// True when `member` has `role` (unconditionally) in the policy JSON.
pub fn policy_has_member(text: &str, role: &str, member: &str) -> Result<bool> {
    let v = parse_policy(text)?;
    Ok(v.get("bindings")
        .and_then(Value::as_array)
        .is_some_and(|bs| {
            bs.iter().any(|b| {
                is_plain_binding(b, role)
                    && b.get("members")
                        .and_then(Value::as_array)
                        .is_some_and(|ms| ms.iter().any(|m| m.as_str() == Some(member)))
            })
        }))
}

/// Add `member` to `role` in the policy JSON, keeping every other binding, the `etag` and
/// every other field. Returns the policy and whether it changed.
pub fn merge_binding(text: &str, role: &str, member: &str) -> Result<(Value, bool)> {
    if policy_has_member(text, role, member)? {
        return Ok((parse_policy(text)?, false));
    }
    let mut v = parse_policy(text)?;
    let obj = v
        .as_object_mut()
        .ok_or_else(|| anyhow!("the IAM policy is not a JSON object"))?;
    let bindings = obj
        .entry("bindings")
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .ok_or_else(|| anyhow!("'bindings' in the IAM policy is not a list"))?;
    if let Some(b) = bindings.iter_mut().find(|b| is_plain_binding(b, role)) {
        let members = b
            .as_object_mut()
            .ok_or_else(|| anyhow!("an IAM binding is not a JSON object"))?
            .entry("members")
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .ok_or_else(|| anyhow!("'members' of an IAM binding is not a list"))?;
        members.push(json!(member));
    } else {
        bindings.push(json!({ "role": role, "members": [member] }));
    }
    Ok((v, true))
}

/// Per repository: it exists, the agent may read it (merged policy), it publishes to the
/// topic. Shared by `gcp init` and `gcp add-repo`.
fn repo_access(g: &Gcloud<'_>, gcp: &Gcp, repo: &str) -> Result<()> {
    let out = g.run(&["source", "repos", "describe", repo, "--format=json"])?;
    if !out.ok() {
        if is_not_found(&out.stderr) {
            bail!(
                "repository {repo} does not exist in project {} (create it, or remove it from the list)",
                gcp.project
            );
        }
        bail!("checking repository {repo}: {}", gcloud_message(&out));
    }
    let attached = topic_attached(&out.stdout, &gcp.project, &gcp.topic)?;

    let member = format!("serviceAccount:{}", gcp.service_account_email());
    let policy = g.run_ok(&["source", "repos", "get-iam-policy", repo, "--format=json"])?;
    let (merged, needs_change) = merge_binding(&policy.stdout, "roles/source.reader", &member)?;
    if needs_change {
        let file = PolicyFile::new(repo, &merged, g.dry())?;
        let path = file.path.to_string_lossy().into_owned();
        g.change(&["source", "repos", "set-iam-policy", repo, &path])?;
        changed(
            g.dry(),
            &format!("add roles/source.reader for the agent on {repo} (existing bindings kept)"),
        );
    } else {
        ok(&format!("{repo}: agent has roles/source.reader"));
    }

    if attached {
        ok(&format!("{repo}: publishes to {}", gcp.topic));
    } else {
        let add_topic = format!("--add-topic={}", gcp.topic);
        g.run_ok(&[
            "source",
            "repos",
            "update",
            repo,
            &add_topic,
            "--message-format=json",
        ])?;
        changed(g.dry(), &format!("attach topic {} to {repo}", gcp.topic));
    }
    Ok(())
}

/// True when the repo's `pubsubConfigs` (from `describe --format=json`) holds the topic.
fn topic_attached(describe_json: &str, project: &str, topic: &str) -> Result<bool> {
    if describe_json.trim().is_empty() {
        return Ok(false);
    }
    let v: Value =
        serde_json::from_str(describe_json).context("parsing the repository JSON from gcloud")?;
    let full = format!("projects/{project}/topics/{topic}");
    Ok(v.get("pubsubConfigs")
        .and_then(Value::as_object)
        .is_some_and(|m| {
            m.iter().any(|(k, c)| {
                k == &full || c.get("topic").and_then(Value::as_str) == Some(full.as_str())
            })
        }))
}

/// A merged policy written to a 0600 temp file for `set-iam-policy`, removed on drop.
/// Nothing is written in `--dry-run`.
struct PolicyFile {
    path: PathBuf,
    written: bool,
}

impl PolicyFile {
    fn new(repo: &str, policy: &Value, dry_run: bool) -> Result<Self> {
        let path = std::env::temp_dir().join(format!(
            "fornax-gcp-{}-{repo}-policy.json",
            std::process::id()
        ));
        if dry_run {
            println!(
                "      policy that would be set on {repo}: {}",
                serde_json::to_string(policy)?
            );
            return Ok(Self {
                path,
                written: false,
            });
        }
        let text = serde_json::to_string_pretty(policy)?;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .with_context(|| format!("creating {}", path.display()))?;
        f.write_all(text.as_bytes())
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(Self {
            path,
            written: true,
        })
    }
}

impl Drop for PolicyFile {
    fn drop(&mut self) {
        if self.written {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// `gcp add-repo NAME` with an injectable prompter.
pub fn add_repo_with(
    r: &dyn Runner,
    p: &dyn Prompter,
    config_path: &Path,
    name: &str,
) -> Result<()> {
    if !config::is_valid_name(name) {
        bail!("'{name}' is not a valid repository name (^[a-z0-9][a-z0-9-]*$)");
    }
    let mut cfg = Config::load(config_path)?;
    let already = cfg.repos.iter().position(|n| n == name);
    if already.is_none() {
        let mut probe = cfg.clone();
        probe.repos.push(name.to_owned());
        super::ensure_valid(&probe)?;
    }
    doctor::require(r, doctor::Mode::Gcp)?;
    let g = Gcloud {
        r,
        project: cfg.gcp.project.clone(),
    };
    println!("repository {name}");
    repo_access(&g, &cfg.gcp, name)?;
    if let Some(i) = already {
        ok(&format!("{name} is already in repos (position {})", i + 1));
        return Ok(());
    }
    println!("current deploy order:");
    for (i, n) in cfg.repos.iter().enumerate() {
        println!("  {}. {n}", i + 1);
    }
    let last = cfg.repos.len() + 1;
    let answer = p.text(
        &format!("Deploy position for {name} (1 = first, {last} = last)"),
        Some(&last.to_string()),
        &|s| match s.parse::<usize>() {
            Ok(n) if (1..=last).contains(&n) => Ok(()),
            _ => Err(format!("a number from 1 to {last}")),
        },
    )?;
    let pos: usize = answer.parse().context("deploy position")?;
    cfg.repos.insert(pos - 1, name.to_owned());
    write_config(config_path, &cfg, r.is_dry_run())?;
    println!(
        "on the server: copy the config, then sudo fornax-gcp server init (or restart the agent)"
    );
    Ok(())
}

/// Refuse relative paths, existing files and missing parent folders.
fn check_new_key_path(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        bail!("{} is not an absolute path", path.display());
    }
    if std::fs::symlink_metadata(path).is_ok() {
        bail!(
            "{} already exists; refusing to overwrite it",
            path.display()
        );
    }
    match path.parent() {
        Some(dir) if dir.is_dir() => Ok(()),
        _ => bail!("the folder of {} does not exist", path.display()),
    }
}

/// `gcp create-key` with an injectable prompter.
pub fn create_key_with(
    r: &dyn Runner,
    p: &dyn Prompter,
    config_path: &Path,
    out: Option<&Path>,
) -> Result<()> {
    let cfg = Config::load(config_path)?;
    let path = match out {
        Some(o) => o.to_path_buf(),
        None => p.path(
            "Absolute path for the new key file (outside any git checkout)",
            None,
            true,
        )?,
    };
    check_new_key_path(&path)?;
    let path_str = path
        .to_str()
        .ok_or_else(|| anyhow!("{} is not valid UTF-8", path.display()))?;
    doctor::require(r, doctor::Mode::Gcp)?;
    let email = cfg.gcp.service_account_email();
    let cmd = Cmd::new("gcloud").args([
        "iam",
        "service-accounts",
        "keys",
        "create",
        path_str,
        "--iam-account",
        &email,
        "--project",
        &cfg.gcp.project,
    ]);
    // gcloud creates the file; a 077 umask keeps it private from the first byte.
    let old = nix::sys::stat::umask(nix::sys::stat::Mode::from_bits_truncate(0o077));
    let res = r.run(&cmd);
    nix::sys::stat::umask(old);
    let result = res.with_context(|| format!("running {}", cmd.display()))?;
    if !result.ok() {
        bail!(
            "creating the key failed (an organisation policy may block keys): {}",
            gcloud_message(&result)
        );
    }
    if r.is_dry_run() {
        changed(true, &format!("create key {} (mode 0600)", path.display()));
    } else {
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("chmod 0600 {}", path.display()))?;
        changed(false, &format!("key {} (mode 0600)", path.display()));
    }
    let d = &cfg.deploy;
    println!("next steps (the key is as sensitive as a password; never commit or paste it):");
    println!(
        "  1. copy it to the server:   scp {} <server>:/tmp/agent-key.json",
        path.display()
    );
    println!(
        "  2. on the server:           sudo install -m 0600 -o {u} -g {u} /tmp/agent-key.json {} && rm /tmp/agent-key.json",
        d.service_account_key.display(),
        u = d.user
    );
    println!("  3. delete the local copy:   rm {}", path.display());
    println!("  4. on the server:           sudo fornax-gcp server init");
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::super::fake::{FakeRunner, fail, line, out};
    use super::*;
    use crate::prompt::ScriptedPrompter;

    const P: &str = "--project my-project-1";
    const EMAIL: &str = "deploy-agent@my-project-1.iam.gserviceaccount.com";

    fn all_bindings() -> String {
        json!({
            "etag": "BwYx",
            "bindings": [
                {"role": "roles/pubsub.subscriber", "members": [format!("serviceAccount:{EMAIL}")]},
                {"role": "roles/artifactregistry.reader", "members": [format!("serviceAccount:{EMAIL}")]},
                {"role": "roles/pubsub.publisher", "members": ["serviceAccount:121549680248-compute@developer.gserviceaccount.com"]},
                {"role": "roles/source.reader", "members": [format!("serviceAccount:{EMAIL}")]},
            ]
        })
        .to_string()
    }

    /// A fake gcloud: `exists` = every resource is there already.
    fn gcloud(exists: bool, policies: Arc<Mutex<Vec<String>>>) -> FakeRunner {
        FakeRunner::new(move |c| {
            let l = line(c);
            let a: Vec<&str> = c.args.iter().map(String::as_str).collect();
            if l == "gcloud --version" {
                return out("Google Cloud SDK 500.0.0\n");
            }
            if l.starts_with("gcloud auth list") {
                return out("admin@example.com\n");
            }
            if l.starts_with("gcloud services list") {
                return out(if exists {
                    "iam.googleapis.com\npubsub.googleapis.com\nsourcerepo.googleapis.com\nartifactregistry.googleapis.com\n"
                } else {
                    "compute.googleapis.com\n"
                });
            }
            if l.starts_with("gcloud projects describe") {
                return out("121549680248\n");
            }
            if l.starts_with("gcloud source repos describe") {
                return out(&if exists {
                    json!({"name": format!("projects/my-project-1/repos/{}", a[3]),
                           "pubsubConfigs": {"projects/my-project-1/topics/events-topic":
                               {"messageFormat": "JSON", "topic": "projects/my-project-1/topics/events-topic"}}})
                    .to_string()
                } else {
                    json!({"name": format!("projects/my-project-1/repos/{}", a[3])}).to_string()
                });
            }
            if a.contains(&"describe") {
                if !exists {
                    return fail("ERROR: (gcloud) NOT_FOUND: Resource not found");
                }
                return out(if a[1] == "subscriptions" {
                    "projects/my-project-1/topics/events-topic\n"
                } else {
                    "present\n"
                });
            }
            if a.contains(&"get-iam-policy") {
                return out(&if exists {
                    all_bindings()
                } else if a[0] == "source" {
                    json!({"etag": "ACAB", "bindings": [
                        {"role": "roles/source.writer", "members": ["user:dev@example.com"]}]})
                    .to_string()
                } else {
                    json!({"etag": "ACAB"}).to_string()
                });
            }
            if a.contains(&"set-iam-policy") {
                let text = std::fs::read_to_string(a[4]).expect("policy file exists during set");
                policies.lock().expect("lock").push(text);
            }
            out("")
        })
    }

    fn script() -> ScriptedPrompter {
        ScriptedPrompter::new(&[
            "my-project-1",     // project
            "events-topic",     // topic
            "agent-sub",        // subscription
            "deploy-agent",     // service account
            "y",                // Artifact Registry?
            "europe-west1",     // location
            "images",           // repository
            "",                 // repo base URL (suggested from the project)
            "app-one, app-two", // repos
            "n",                // shared services?
            "",                 // deployment dir (.fornax)
            "n",                // create a key now?
        ])
    }

    fn normalise(lines: Vec<String>) -> Vec<String> {
        lines
            .into_iter()
            .map(|l| {
                let mut parts: Vec<String> = l.split(' ').map(String::from).collect();
                if parts.get(3).map(String::as_str) == Some("set-iam-policy") {
                    parts[5] = "<policy>".into();
                }
                parts.join(" ")
            })
            .collect()
    }

    #[test]
    fn init_creates_everything_in_order_then_is_idempotent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg_path = dir.path().join("fornax-gcp.toml");
        let policies = Arc::new(Mutex::new(Vec::new()));
        let r = gcloud(false, policies.clone());
        let p = script();
        init_with(&r, &p, &cfg_path).expect("init");
        assert_eq!(p.remaining(), 0);

        let sa = format!("serviceAccount:{EMAIL}");
        let mut expected: Vec<String> = vec![
            "gcloud --version".into(),
            "gcloud auth list --filter=status:ACTIVE --format=value(account)".into(),
            format!("gcloud services list --enabled --format=value(config.name) {P}"),
            format!(
                "gcloud services enable pubsub.googleapis.com sourcerepo.googleapis.com iam.googleapis.com artifactregistry.googleapis.com {P}"
            ),
            format!("gcloud projects describe my-project-1 --format=value(projectNumber) {P}"),
            format!("gcloud pubsub topics describe events-topic --format=value(name) {P}"),
            format!("gcloud pubsub topics create events-topic {P}"),
            format!("gcloud pubsub subscriptions describe agent-sub --format=value(topic) {P}"),
            format!(
                "gcloud pubsub subscriptions create agent-sub --topic events-topic --ack-deadline 60 --message-retention-duration 7d --expiration-period never {P}"
            ),
            format!("gcloud iam service-accounts describe {EMAIL} --format=value(email) {P}"),
            format!(
                "gcloud iam service-accounts create deploy-agent --display-name fornax-gcp deploy agent (deploy-agent) {P}"
            ),
            format!("gcloud pubsub subscriptions get-iam-policy agent-sub --format=json {P}"),
            format!(
                "gcloud pubsub subscriptions add-iam-policy-binding agent-sub --member {sa} --role roles/pubsub.subscriber {P}"
            ),
            format!(
                "gcloud artifacts repositories get-iam-policy images --location europe-west1 --format=json {P}"
            ),
            format!(
                "gcloud artifacts repositories add-iam-policy-binding images --location europe-west1 --member {sa} --role roles/artifactregistry.reader {P}"
            ),
            format!("gcloud pubsub topics get-iam-policy events-topic --format=json {P}"),
            format!(
                "gcloud pubsub topics add-iam-policy-binding events-topic --member serviceAccount:121549680248-compute@developer.gserviceaccount.com --role roles/pubsub.publisher {P}"
            ),
        ];
        for repo in ["app-one", "app-two"] {
            expected.extend([
                format!("gcloud source repos describe {repo} --format=json {P}"),
                format!("gcloud source repos get-iam-policy {repo} --format=json {P}"),
                format!("gcloud source repos set-iam-policy {repo} <policy> {P}"),
                format!("gcloud source repos update {repo} --add-topic=events-topic --message-format=json {P}"),
            ]);
        }
        assert_eq!(normalise(r.lines()), expected);

        // The merged policies kept the existing binding and etag.
        let policies = policies.lock().expect("lock").clone();
        assert_eq!(policies.len(), 2);
        for text in &policies {
            let v: Value = serde_json::from_str(text).expect("json");
            assert_eq!(v["etag"], "ACAB");
            assert!(
                policy_has_member(text, "roles/source.writer", "user:dev@example.com").expect("p")
            );
            assert!(policy_has_member(text, "roles/source.reader", &sa).expect("p"));
        }
        // Temp policy files are removed.
        let leftover = std::fs::read_dir(std::env::temp_dir())
            .expect("tmp")
            .filter_map(|e| e.ok())
            .any(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(&format!("fornax-gcp-{}-", std::process::id()))
            });
        assert!(!leftover, "policy temp file left behind");

        let written = Config::load(&cfg_path).expect("written config is valid");
        assert_eq!(written.gcp.project, "my-project-1");
        assert_eq!(written.gcp.project_number.as_deref(), Some("121549680248"));
        assert_eq!(written.gcp.topic, "events-topic");
        assert_eq!(written.gcp.subscription, "agent-sub");
        assert_eq!(written.gcp.service_account, "deploy-agent");
        assert_eq!(
            written.gcp.artifact_location.as_deref(),
            Some("europe-west1")
        );
        assert_eq!(written.gcp.artifact_repository.as_deref(), Some("images"));
        assert_eq!(
            written.gcp.repo_base_url,
            "https://source.developers.google.com/p/my-project-1/r"
        );
        assert_eq!(written.repos, ["app-one", "app-two"]);
        assert_eq!(written.shared, None);
        assert_eq!(written.deploy.deployment_dir, ".fornax");

        // Second run: everything exists → only reads.
        let r2 = gcloud(true, Arc::new(Mutex::new(Vec::new())));
        init_with(&r2, &ScriptedPrompter::yes(), &cfg_path).expect("second init");
        let lines = r2.lines();
        for l in &lines {
            for verb in [
                " create ",
                " enable ",
                "add-iam-policy-binding",
                "set-iam-policy",
                " update ",
            ] {
                assert!(!l.contains(verb), "second run changed something: {l}");
            }
        }
        assert_eq!(lines.iter().filter(|l| l.contains(" describe ")).count(), 6);
        assert_eq!(Config::load(&cfg_path).expect("config"), written);
    }

    #[test]
    fn init_dry_run_with_yes_writes_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg_path = dir.path().join("fornax-gcp.toml");
        let r = FakeRunner::dry();
        init_with(&r, &ScriptedPrompter::yes(), &cfg_path).expect("dry init");
        assert!(!cfg_path.exists());
        let lines = r.lines();
        assert!(
            lines
                .iter()
                .any(|l| l.contains("pubsub topics create srf-source-repo-events"))
        );
        assert!(lines.iter().any(|l| l.contains(
            "--member serviceAccount:121549680248-compute@developer.gserviceaccount.com"
        )));
        // Infra repo first, then the app repos.
        let attach: Vec<&String> = lines.iter().filter(|l| l.contains("--add-topic")).collect();
        assert_eq!(attach.len(), 8);
        assert!(attach[0].contains("srf-blp-infra-configuration"));
    }

    #[test]
    fn merge_keeps_bindings_and_etag_and_does_not_duplicate() {
        let m = "serviceAccount:a@p.iam.gserviceaccount.com";
        let text = json!({
            "etag": "BwX1", "version": 1,
            "bindings": [
                {"role": "roles/source.writer", "members": ["user:x@example.com"]},
                {"role": "roles/source.reader", "members": ["group:devs@example.com"]},
                {"role": "roles/source.reader", "members": [m], "condition": {"title": "t", "expression": "true"}},
            ]
        })
        .to_string();
        let (v, changed) = merge_binding(&text, "roles/source.reader", m).expect("merge");
        assert!(changed);
        assert_eq!(v["etag"], "BwX1");
        assert_eq!(v["version"], 1);
        let bindings = v["bindings"].as_array().expect("list");
        assert_eq!(bindings.len(), 3);
        assert_eq!(bindings[0]["members"], json!(["user:x@example.com"]));
        assert_eq!(bindings[1]["members"], json!(["group:devs@example.com", m]));
        // Merging again changes nothing and does not duplicate the member.
        let again = serde_json::to_string(&v).expect("json");
        let (v2, changed2) = merge_binding(&again, "roles/source.reader", m).expect("merge");
        assert!(!changed2);
        assert_eq!(v2, v);
        // An empty policy gets one binding and keeps its etag.
        let (v3, _) = merge_binding(r#"{"etag":"ACAB"}"#, "roles/source.reader", m).expect("merge");
        assert_eq!(
            v3,
            json!({"etag": "ACAB", "bindings": [{"role": "roles/source.reader", "members": [m]}]})
        );
        assert!(merge_binding("[1]", "r", m).is_err());
    }

    #[test]
    fn add_repo_inserts_at_the_chosen_position() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg_path = dir.path().join("fornax-gcp.toml");
        let mut cfg = Config::blp_example();
        cfg.gcp.project = "my-project-1".into();
        cfg.gcp.service_account = "deploy-agent".into();
        cfg.gcp.topic = "events-topic".into();
        cfg.repos = vec!["a-one".into(), "a-two".into()];
        std::fs::write(&cfg_path, cfg.to_toml().expect("toml")).expect("write");
        let r = gcloud(false, Arc::new(Mutex::new(Vec::new())));
        let p = ScriptedPrompter::new(&["9", "2"]);
        add_repo_with(&r, &p, &cfg_path, "new-repo").expect("add");
        let after = Config::load(&cfg_path).expect("load");
        assert_eq!(after.repos, ["a-one", "new-repo", "a-two"]);
        let lines = normalise(r.lines());
        assert!(lines.contains(&format!(
            "gcloud source repos set-iam-policy new-repo <policy> {P}"
        )));
        assert!(lines.contains(&format!(
            "gcloud source repos update new-repo --add-topic=events-topic --message-format=json {P}"
        )));
        assert!(add_repo_with(&r, &p, &cfg_path, "Bad").is_err());
        // A shared app name cannot become a repo.
        assert!(add_repo_with(&r, &ScriptedPrompter::yes(), &cfg_path, "platform").is_err());
    }

    #[test]
    fn create_key_checks_path_and_sets_0600() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg_path = dir.path().join("fornax-gcp.toml");
        std::fs::write(&cfg_path, Config::blp_example().to_toml().expect("toml")).expect("write");
        let r = FakeRunner::new(|c| {
            let l = line(c);
            if l.starts_with("gcloud auth list") {
                return out("admin@example.com\n");
            }
            if l.starts_with("gcloud iam service-accounts keys create") {
                std::fs::write(&c.args[4], "{}").expect("fake key");
                std::fs::set_permissions(&c.args[4], std::fs::Permissions::from_mode(0o644))
                    .expect("chmod");
            }
            out("ok\n")
        });
        let e = create_key_with(
            &r,
            &ScriptedPrompter::yes(),
            &cfg_path,
            Some(Path::new("key.json")),
        )
        .expect_err("relative");
        assert!(e.to_string().contains("absolute"), "{e}");
        let e = create_key_with(&r, &ScriptedPrompter::yes(), &cfg_path, None)
            .expect_err("--yes without --out");
        assert!(e.to_string().contains("no suggested value"), "{e}");
        let e = create_key_with(&r, &ScriptedPrompter::yes(), &cfg_path, Some(&cfg_path))
            .expect_err("existing");
        assert!(e.to_string().contains("already exists"), "{e}");
        assert!(r.lines().is_empty(), "nothing ran for refused paths");

        let key = dir.path().join("key.json");
        // Interactive: a relative answer is asked again.
        let key_str = key.to_string_lossy().into_owned();
        let p = ScriptedPrompter::new(&["relative.json", &key_str]);
        create_key_with(&r, &p, &cfg_path, None).expect("create");
        let mode = std::fs::metadata(&key).expect("key").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(
            r.lines().last().expect("call"),
            &format!(
                "gcloud iam service-accounts keys create {key_str} --iam-account srf-blp-deploy-agent@visual-analytics-1.iam.gserviceaccount.com --project visual-analytics-1"
            )
        );
    }
}
