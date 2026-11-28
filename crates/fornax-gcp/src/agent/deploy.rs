// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! Deploy one app at one tag (port of deploy.sh, same steps, messages and exit codes).
//!
//! 1. Extract the config of the tag into `releases/<name>/<tag>/` (compose.yml + release.vars).
//! 2. Check it (`compose config`, WATCH_PATHS exist) and pull images that are not here yet.
//!    Anything wrong up to this point leaves the running stack and its code untouched.
//! 3. Sync the code, then `docker compose up -d --wait` (recreates only on image/config change).
//! 4. Mounted code changed but a container was not recreated => restart it.
//! 5. Success: record `state/<name>.deployed`. Failure: restore the previous release (code and
//!    containers) and record `state/<name>.failed` so the agent does not retry the same tag.
//!
//! No shell is used: the name and tag are validated first, every other value placed on a
//! command line comes from git (commit ids), the configuration (validated paths) or is checked
//! here (WATCH_PATHS, HEALTH_TIMEOUT, JOB_SERVICE, container ids).

use std::cell::Cell;
use std::collections::BTreeSet;
use std::fmt::Display;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::agent::lock::{AppLock, LockError};
use crate::agent::vars::{ReleaseVars, is_version};
use crate::config::{Config, is_valid_name};
use crate::exec::{Cmd, Output, Runner};

/// Exit codes, identical to deploy.sh (plus `timeout(1)`'s 124 used by agent.py).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeployCode {
    /// Live and healthy.
    Ok = 0,
    /// Unexpected local error (deploy.sh under `set -e`), e.g. a folder cannot be created.
    Error = 1,
    /// Another deploy of this app is running.
    Busy = 2,
    /// Failed; the previous version is (still or again) running.
    FailedKept = 3,
    /// Failed and not restored: act now.
    FailedNotRestored = 4,
    /// Bad name, tag or option.
    Usage = 64,
    /// `agent.deploy_timeout_secs` ran out; the deploy stopped where it was, no state recorded.
    TimedOut = 124,
}

impl DeployCode {
    /// Process exit code.
    pub fn as_u8(self) -> u8 {
        self as u8
    }
}

/// Deploy `name` at `tag` (`restart`: restart containers only). No time limit (like running
/// deploy.sh by hand); the agent uses [`deploy_until`].
pub fn deploy(r: &dyn Runner, cfg: &Config, name: &str, tag: &str, restart: bool) -> DeployCode {
    deploy_until(r, cfg, name, tag, restart, None)
}

/// [`deploy`] with an overall deadline: every command gets the remaining time as its timeout,
/// and once it is reached the deploy stops at once and returns [`DeployCode::TimedOut`]
/// without recording any state (as when agent.py's `timeout` killed deploy.sh).
pub fn deploy_until(
    r: &dyn Runner,
    cfg: &Config,
    name: &str,
    tag: &str,
    restart: bool,
    deadline: Option<Instant>,
) -> DeployCode {
    let Some(d) = Deployer::new(r, cfg, name, tag, deadline) else {
        return DeployCode::Usage;
    };
    d.main(restart)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// App repository: tags `vX.Y.Z`, config in `deployment_dir`, code synced.
    Repo,
    /// Shared service in the infra repo: tags `<name>-vX.Y.Z`, no code sync.
    Shared,
}

/// How `apply` failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fail {
    /// Nothing on the server was changed (deploy.sh return code 5).
    Untouched,
    /// Something was changed (code or containers).
    Changed,
}

struct Deployer<'a> {
    r: &'a dyn Runner,
    cfg: &'a Config,
    app: String,
    tag: String,
    kind: Kind,
    git_dir: PathBuf,
    /// Config folder inside the repository.
    cfg_dir: String,
    /// Path components of `cfg_dir` (tar `--strip-components`).
    strip: usize,
    /// Tag prefix: `""` or `<name>-`.
    prefix: String,
    /// Code folder (`APP_DIR`); `None` for a shared service.
    app_dir: Option<PathBuf>,
    project: String,
    state_dir: PathBuf,
    rel_root: PathBuf,
    deadline: Option<Instant>,
    started: Instant,
    timed_out: Cell<bool>,
}

fn s(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// A compose service name (`JOB_SERVICE`).
fn is_service_name(v: &str) -> bool {
    let b = v.as_bytes();
    !b.is_empty()
        && b[0].is_ascii_alphanumeric()
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || b"_.-".contains(c))
}

/// A container id or name as printed by `compose ps -q`.
fn is_container_ref(v: &str) -> bool {
    is_service_name(v)
}

impl<'a> Deployer<'a> {
    fn new(
        r: &'a dyn Runner,
        cfg: &'a Config,
        name: &str,
        tag: &str,
        deadline: Option<Instant>,
    ) -> Option<Self> {
        let die = |msg: String| {
            tracing::error!("[deploy:{}] ERROR: {msg}", name.escape_debug());
        };
        if !is_valid_name(name) {
            die("usage: fornax-gcp deploy <name> <tag> [--restart]  (bad name)".into());
            return None;
        }
        let d = &cfg.deploy;
        let (kind, git_dir, cfg_dir, prefix, app_dir, project) = if is_version(tag) {
            (
                Kind::Repo,
                d.repos_dir().join(format!("{name}.git")),
                d.deployment_dir.clone(),
                String::new(),
                Some(d.apps_dir.join(name)),
                name.to_owned(),
            )
        } else if tag
            .strip_prefix(name)
            .and_then(|t| t.strip_prefix('-'))
            .is_some_and(is_version)
        {
            let Some(sh) = &cfg.shared else {
                die(format!(
                    "tag '{}' names a shared service but no [shared] repo is configured",
                    tag.escape_debug()
                ));
                return None;
            };
            (
                Kind::Shared,
                d.infra_git_dir(),
                format!("{}/{name}", sh.apps_dir.trim_end_matches('/')),
                format!("{name}-"),
                None,
                format!("{}{name}", sh.project_prefix),
            )
        } else {
            die(format!(
                "tag '{}' must look like v1.2.3 (app repo) or {name}-v1.2.3 (shared service)",
                tag.escape_debug()
            ));
            return None;
        };
        let strip = Path::new(&cfg_dir).components().count();
        Some(Self {
            r,
            cfg,
            app: name.to_owned(),
            tag: tag.to_owned(),
            kind,
            git_dir,
            cfg_dir,
            strip,
            prefix,
            app_dir,
            project,
            state_dir: d.state_dir(),
            rel_root: d.releases_dir().join(name),
            deadline,
            started: Instant::now(),
            timed_out: Cell::new(false),
        })
    }

    fn log(&self, msg: impl Display) {
        tracing::info!("[deploy:{}] {msg}", self.app);
    }

    /// Log a command's output (what deploy.sh let through to the journal).
    fn show(&self, out: &Output) {
        for line in out.stdout.lines().chain(out.stderr.lines()) {
            let line = line.trim_end();
            if !line.is_empty() {
                tracing::info!("[deploy:{}]   {line}", self.app);
            }
        }
    }

    /// Run one command (CLOUDSDK_CONFIG set, bounded by the deadline). Never fails: a command
    /// that cannot start is reported and counts as a failed command.
    fn exec(&self, cmd: Cmd) -> Output {
        let mut cmd = cmd.env("CLOUDSDK_CONFIG", s(&self.cfg.deploy.gcloud_config_dir));
        if let Some(deadline) = self.deadline {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                self.timed_out.set(true);
                return Output {
                    code: -1,
                    timed_out: true,
                    ..Output::default()
                };
            }
            cmd.timeout = Some(left);
        }
        match self.r.run(&cmd) {
            Ok(out) => {
                if out.timed_out {
                    self.timed_out.set(true);
                }
                out
            }
            Err(e) => {
                self.log(&e);
                Output {
                    code: -1,
                    stderr: e.to_string(),
                    ..Output::default()
                }
            }
        }
    }

    fn git(&self) -> Cmd {
        Cmd::new("git").arg(format!("--git-dir={}", s(&self.git_dir)))
    }

    /// `docker compose -p <project> -f <d>/compose.yml --project-directory <d> [--env-file …]`.
    fn dc<I, S>(&self, dir: &Path, args: I) -> Cmd
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut c = Cmd::new("docker").args([
            "compose".to_owned(),
            "-p".to_owned(),
            self.project.clone(),
            "-f".to_owned(),
            s(&dir.join("compose.yml")),
            "--project-directory".to_owned(),
            s(dir),
        ]);
        let vars = dir.join("release.vars");
        if vars.is_file() {
            c = c.arg("--env-file").arg(s(&vars));
        }
        let shared = self.cfg.deploy.shared_env_file.as_deref();
        if let Some(f) = shared
            && f.is_file()
        {
            c = c.arg("--env-file").arg(s(f));
        }
        let own = self
            .cfg
            .deploy
            .secrets_dir
            .join(format!("{}.env", self.app));
        if own.is_file() {
            c = c.arg("--env-file").arg(s(&own));
        }
        c = c.env(
            "APP_DIR",
            self.app_dir.as_deref().map(s).unwrap_or_default(),
        );
        if let Some(f) = shared {
            c = c.env("SHARED_ENV_FILE", s(f));
        }
        c.args(args)
    }

    fn read_state(&self, kind: &str) -> String {
        fs::read_to_string(self.state_dir.join(format!("{}.{kind}", self.app)))
            .map(|v| v.trim_end_matches('\n').to_owned())
            .unwrap_or_default()
    }

    fn write_state(&self, kind: &str, value: &str) -> io::Result<()> {
        let path = self.state_dir.join(format!("{}.{kind}", self.app));
        let tmp = self.state_dir.join(format!("{}.{kind}.tmp", self.app));
        fs::write(&tmp, format!("{value}\n"))?;
        fs::rename(&tmp, &path)
    }

    fn commit_of(&self, tag: &str) -> Option<String> {
        let out = self.exec(self.git().args([
            "rev-parse".to_owned(),
            "--verify".to_owned(),
            "-q".to_owned(),
            format!("refs/tags/{tag}^{{commit}}"),
        ]));
        let c = out.stdout.trim();
        (out.ok() && !c.is_empty()).then(|| c.to_owned())
    }

    fn release(&self, tag: &str) -> PathBuf {
        self.rel_root.join(tag)
    }

    fn vars(&self, tag: &str) -> ReleaseVars {
        ReleaseVars::load(&self.release(tag))
    }

    /// Extract `cfg_dir/` of `commit` into `releases/<name>/<tag>/` (kept when present).
    fn extract(&self, tag: &str, commit: &str) -> bool {
        let dest = self.release(tag);
        if dest.join("compose.yml").is_file() {
            return true;
        }
        if dest.is_dir() {
            let _ = fs::remove_dir_all(&dest);
        } else if dest.symlink_metadata().is_ok() {
            let _ = fs::remove_file(&dest);
        }
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let unique = format!(".tmp.{}.{nanos}", std::process::id());
        let tmp = self.rel_root.join(&unique);
        let tarball = self.rel_root.join(format!("{unique}.tar"));
        if fs::create_dir(&tmp).is_err() {
            return false;
        }
        let cleanup = || {
            let _ = fs::remove_dir_all(&tmp);
            let _ = fs::remove_file(&tarball);
        };
        let archived = self.exec(self.git().args([
            "archive".to_owned(),
            "--format=tar".to_owned(),
            format!("--output={}", s(&tarball)),
            commit.to_owned(),
            self.cfg_dir.clone(),
        ]));
        let ok = archived.ok() && {
            let out = self.exec(Cmd::new("tar").args([
                "-x".to_owned(),
                "-f".to_owned(),
                s(&tarball),
                "-C".to_owned(),
                s(&tmp),
                format!("--strip-components={}", self.strip),
            ]));
            if !out.ok() {
                self.show(&out);
            }
            out.ok()
        };
        let _ = fs::remove_file(&tarball);
        if !ok {
            cleanup();
            self.log(format!("cannot read {}/ at {tag}", self.cfg_dir));
            return false;
        }
        if !tmp.join("compose.yml").is_file() {
            cleanup();
            self.log(format!("{}/compose.yml missing at {tag}", self.cfg_dir));
            return false;
        }
        if fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755)).is_err()
            || fs::rename(&tmp, &dest).is_err()
        {
            cleanup();
            return false;
        }
        true
    }

    fn check_watch_paths(&self, tag: &str, commit: &str) -> bool {
        if self.kind != Kind::Repo {
            return true;
        }
        for p in self.vars(tag).list("WATCH_PATHS") {
            let bad = p.starts_with('/') || p.contains("..") || {
                let path = p.strip_suffix('/').unwrap_or(&p);
                !self
                    .exec(self.git().args([
                        "cat-file".to_owned(),
                        "-e".to_owned(),
                        format!("{commit}:{path}"),
                    ]))
                    .ok()
            };
            if bad {
                self.log(format!(
                    "WATCH_PATHS entry '{p}' is not in this tag (is it committed? check .gitignore)"
                ));
                return false;
            }
        }
        true
    }

    /// True when the watched code differs between the commit on disk and the new one.
    fn code_changed(&self, tag: &str, from: &str, to: &str) -> bool {
        if self.kind != Kind::Repo || from.is_empty() || from == to {
            return false;
        }
        let mut c = self.git().args(["diff", "--quiet", from, to, "--"]);
        c = c.args(self.vars(tag).list("WATCH_PATHS"));
        !self.exec(c).ok()
    }

    /// Make `apps_dir/<name>` match the commit. Local edits to tracked files are overwritten.
    fn sync_code(&self, commit: &str) -> bool {
        let Some(app_dir) = &self.app_dir else {
            return true;
        };
        self.log(format!("syncing code to {}", app_dir.display()));
        if fs::create_dir_all(app_dir).is_err() {
            return false;
        }
        let out = self.exec(self.git().args([
            format!("--work-tree={}", s(app_dir)),
            "checkout".to_owned(),
            "-q".to_owned(),
            "-f".to_owned(),
            "--detach".to_owned(),
            commit.to_owned(),
        ]));
        self.show(&out);
        out.ok()
    }

    fn up(&self, tag: &str) -> bool {
        let d = self.release(tag);
        let mut wait = self.vars(tag).get("HEALTH_TIMEOUT");
        if wait.is_empty() {
            wait = self.cfg.agent.health_timeout_secs.to_string();
        }
        self.log(format!("up -d (waits up to {wait}s for healthy)"));
        let ok = if wait.bytes().all(|b| b.is_ascii_digit()) {
            let out = self.exec(self.dc(
                &d,
                [
                    "up",
                    "-d",
                    "--pull",
                    "never",
                    "--remove-orphans",
                    "--wait",
                    "--wait-timeout",
                    wait.as_str(),
                ],
            ));
            self.show(&out);
            out.ok()
        } else {
            self.log(format!(
                "HEALTH_TIMEOUT '{wait}' is not a number of seconds"
            ));
            false
        };
        if ok {
            return true;
        }
        self.log("stack did not become healthy; recent state and logs:");
        self.show(&self.exec(self.dc(&d, ["ps", "-a"])));
        self.show(&self.exec(self.dc(&d, ["logs", "--tail", "40", "--no-color"])));
        false
    }

    fn container_ids(&self, d: &Path) -> BTreeSet<String> {
        let out = self.exec(self.dc(d, ["ps", "-q"]));
        if !out.ok() {
            return BTreeSet::new();
        }
        out.stdout
            .lines()
            .map(str::trim)
            .filter(|l| is_container_ref(l))
            .map(str::to_owned)
            .collect()
    }

    /// Deploy `tag` (`commit`) over the code currently on disk (`on_disk`, may be empty).
    fn apply(&self, tag: &str, commit: &str, on_disk: &str) -> Result<(), Fail> {
        let d = self.release(tag);
        if !self.extract(tag, commit) {
            return Err(Fail::Untouched);
        }
        self.log(format!("validating compose file of {tag}"));
        let out = self.exec(self.dc(&d, ["config", "-q"]));
        self.show(&out);
        if !out.ok() {
            self.log(format!("compose file of {tag} is invalid"));
            return Err(Fail::Untouched);
        }
        if !self.check_watch_paths(tag, commit) {
            return Err(Fail::Untouched);
        }
        self.log("pulling images that are not on this server yet");
        let out = self.exec(self.dc(&d, ["pull", "--policy", "missing", "--quiet"]));
        self.show(&out);
        if !out.ok() {
            self.log("image pull FAILED - check the image name and tag in release.vars");
            return Err(Fail::Untouched);
        }
        if !self.sync_code(commit) {
            self.log("code sync failed");
            return Err(Fail::Changed);
        }

        let job = self.vars(tag).get("JOB_SERVICE");
        if !job.is_empty() {
            self.log(format!("running one-shot job '{job}'"));
            let ok = if is_service_name(&job) {
                let out =
                    self.exec(self.dc(&d, ["run", "--rm", "-T", "--pull", "never", job.as_str()]));
                self.show(&out);
                out.ok()
            } else {
                self.log("JOB_SERVICE is not a valid compose service name");
                false
            };
            if !ok {
                self.log(format!("job '{job}' failed"));
                return Err(Fail::Changed);
            }
            return Ok(());
        }

        if self.code_changed(tag, on_disk, commit) {
            // Create / recreate first, then restart what was NOT recreated (it still runs the
            // old code), and only then judge health. A failure of this first `up` shows up in
            // the final one.
            let before = self.container_ids(&d);
            let out = self.exec(self.dc(&d, ["up", "-d", "--pull", "never", "--remove-orphans"]));
            self.show(&out);
            let after = self.container_ids(&d);
            let kept: Vec<String> = before.intersection(&after).cloned().collect();
            if !kept.is_empty() {
                self.log(format!(
                    "mounted code changed: restarting {} container(s)",
                    kept.len()
                ));
                let out = self.exec(Cmd::new("docker").arg("restart").args(kept));
                if !out.ok() {
                    self.show(&out);
                    return Err(Fail::Changed);
                }
            }
        }
        if self.up(tag) {
            Ok(())
        } else {
            Err(Fail::Changed)
        }
    }

    /// `ln -sfn releases/<name>/<tag> releases/<name>/current`.
    fn link_current(&self, tag: &str) -> bool {
        let link = self.rel_root.join("current");
        let tmp = self
            .rel_root
            .join(format!(".current.{}", std::process::id()));
        let _ = fs::remove_file(&tmp);
        let ok = std::os::unix::fs::symlink(self.release(tag), &tmp).is_ok()
            && fs::rename(&tmp, &link).is_ok();
        if !ok {
            let _ = fs::remove_file(&tmp);
            tracing::error!("[deploy:{}] cannot update {}", self.app, link.display());
        }
        ok
    }

    /// Keep the newest `keep_releases` release folders, never the live or previous one.
    fn prune(&self, prev: &str) {
        for d in prune_candidates(
            &self.rel_root,
            &self.prefix,
            self.cfg.agent.keep_releases,
            &[self.tag.as_str(), prev],
        ) {
            let _ = fs::remove_dir_all(d);
        }
    }

    fn timed_out_code(&self) -> DeployCode {
        self.log(format!(
            "TIMED OUT after {}s (agent.deploy_timeout_secs): stopped here, no state recorded",
            self.started.elapsed().as_secs()
        ));
        DeployCode::TimedOut
    }

    fn main(&self, restart: bool) -> DeployCode {
        // Synced code must be readable by the container user.
        nix::sys::stat::umask(nix::sys::stat::Mode::from_bits_truncate(0o022));
        let locks = self.cfg.deploy.locks_dir();
        for dir in [&self.state_dir, &locks, &self.rel_root] {
            if let Err(e) = fs::create_dir_all(dir) {
                tracing::error!("[deploy:{}] cannot create {}: {e}", self.app, dir.display());
                return DeployCode::Error;
            }
        }
        // One deploy per app at a time (the agent and a human can both call this).
        let _lock = match AppLock::try_lock(&locks.join(format!("{}.lock", self.app))) {
            Ok(l) => l,
            Err(LockError::Busy) => {
                self.log(format!("another deploy of {} is running", self.app));
                return DeployCode::Busy;
            }
            Err(e) => {
                tracing::error!("[deploy:{}] {e}", self.app);
                return DeployCode::Error;
            }
        };
        let tag = self.tag.as_str();
        let Some(new) = self.commit_of(tag) else {
            if self.timed_out.get() {
                return self.timed_out_code();
            }
            tracing::error!(
                "[deploy:{}] ERROR: tag '{tag}' not found in {} (has the agent fetched it?)",
                self.app,
                self.git_dir.display()
            );
            return DeployCode::Usage;
        };
        let prev = self.read_state("deployed");
        let mut prev_commit = self.read_state("commit");
        if prev_commit.is_empty() && !prev.is_empty() {
            prev_commit = self.commit_of(&prev).unwrap_or_default();
        }

        if restart {
            if !self.extract(tag, &new) {
                return DeployCode::FailedNotRestored;
            }
            self.log(format!(
                "restarting containers of {tag} (no pull, no recreate)"
            ));
            let out = self.exec(self.dc(&self.release(tag), ["restart"]));
            self.show(&out);
            if self.timed_out.get() {
                return self.timed_out_code();
            }
            return if out.ok() {
                DeployCode::Ok
            } else {
                DeployCode::Error
            };
        }

        self.log(format!(
            "target {tag} (currently deployed: {})",
            if prev.is_empty() { "none" } else { &prev }
        ));
        let result = self.apply(tag, &new, &prev_commit);
        if self.timed_out.get() {
            return self.timed_out_code();
        }
        let state_err = |e: io::Error| {
            tracing::error!("[deploy:{}] cannot write state: {e}", self.app);
            DeployCode::Error
        };
        let Err(fail) = result else {
            if let Err(e) = self
                .write_state("deployed", tag)
                .and_then(|()| self.write_state("commit", &new))
            {
                return state_err(e);
            }
            let failed = self.state_dir.join(format!("{}.failed", self.app));
            if failed.symlink_metadata().is_ok()
                && let Err(e) = fs::remove_file(&failed)
            {
                return state_err(e);
            }
            if !self.link_current(tag) {
                return DeployCode::Error;
            }
            self.prune(&prev);
            self.log(format!("OK: {tag} is live"));
            return DeployCode::Ok;
        };

        self.log(format!("FAILED: {tag}"));
        if let Err(e) = self.write_state("failed", tag) {
            return state_err(e);
        }
        if fail == Fail::Untouched {
            if prev.is_empty() {
                self.log("nothing was changed on this server");
                return DeployCode::FailedNotRestored;
            }
            self.log(format!(
                "nothing was changed on this server; {prev} keeps running"
            ));
            return DeployCode::FailedKept;
        }
        if !self.vars(tag).get("JOB_SERVICE").is_empty() {
            self.log("a job cannot be rolled back automatically - check what it already did");
            if !prev_commit.is_empty() {
                self.sync_code(&prev_commit);
            }
            return DeployCode::FailedNotRestored;
        }
        if prev == tag {
            self.log(
                "redeploy of the live tag failed; leaving the stack as it is - manual action needed",
            );
            return DeployCode::FailedNotRestored;
        }
        if !prev.is_empty() && !prev_commit.is_empty() {
            self.log(format!("rolling back to {prev}"));
            let back = self.apply(&prev, &prev_commit, &new);
            if self.timed_out.get() {
                return self.timed_out_code();
            }
            if back.is_ok() {
                if !self.link_current(&prev) {
                    return DeployCode::Error;
                }
                self.log(format!("ROLLED BACK to {prev}"));
                return DeployCode::FailedKept;
            }
            self.log(format!(
                "ROLLBACK FAILED: {prev} is not healthy either - manual action needed"
            ));
            return DeployCode::FailedNotRestored;
        }
        self.log("no previous release to roll back to; stopping the failed stack");
        self.show(&self.exec(self.dc(&self.release(tag), ["down"])));
        DeployCode::FailedNotRestored
    }
}

/// Release folders to delete: entries `<prefix>v*` of `rel_root` that are directories, newest
/// first by modification time (`ls -1dt`, ties by name), minus the first `keep` and minus
/// `protect` (live and previous tag).
pub fn prune_candidates(
    rel_root: &Path,
    prefix: &str,
    keep: usize,
    protect: &[&str],
) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(rel_root) else {
        return Vec::new();
    };
    let start = format!("{prefix}v");
    let mut dirs: Vec<(SystemTime, String, PathBuf)> = entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let path = e.path();
            // `ls -d <glob>/` follows symlinks to directories.
            let meta = fs::metadata(&path).ok()?;
            if !name.starts_with(&start) || !meta.is_dir() {
                return None;
            }
            Some((meta.modified().unwrap_or(UNIX_EPOCH), name, path))
        })
        .collect();
    dirs.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    dirs.into_iter()
        .skip(keep)
        .filter(|(_, name, _)| !protect.contains(&name.as_str()))
        .map(|(_, _, p)| p)
        .collect()
}

/// Seconds as a [`Duration`] deadline from now.
pub fn deadline_after(secs: u64) -> Option<Instant> {
    Instant::now().checked_add(Duration::from_secs(secs))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::exec::ExecError;

    #[test]
    fn exit_codes_match_deploy_sh() {
        assert_eq!(DeployCode::Ok.as_u8(), 0);
        assert_eq!(DeployCode::Error.as_u8(), 1);
        assert_eq!(DeployCode::Busy.as_u8(), 2);
        assert_eq!(DeployCode::FailedKept.as_u8(), 3);
        assert_eq!(DeployCode::FailedNotRestored.as_u8(), 4);
        assert_eq!(DeployCode::Usage.as_u8(), 64);
        assert_eq!(DeployCode::TimedOut.as_u8(), 124);
    }

    fn set_mtime(p: &Path, secs: u64) {
        let f = fs::File::open(p).expect("open dir");
        f.set_modified(UNIX_EPOCH + Duration::from_secs(secs))
            .expect("set mtime");
    }

    #[test]
    fn prune_keeps_newest_and_never_live_or_previous() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        for (i, t) in ["v0.1.0", "v0.2.0", "v0.3.0", "v0.4.0", "v0.5.0"]
            .iter()
            .enumerate()
        {
            fs::create_dir(root.join(t)).expect("mkdir");
            set_mtime(&root.join(t), 1_000 + i as u64);
        }
        fs::create_dir(root.join(".tmp.1.2")).expect("tmp");
        fs::write(root.join("vfile"), "x").expect("file");
        std::os::unix::fs::symlink(root.join("v0.5.0"), root.join("current")).expect("link");
        // Newest two are v0.5.0, v0.4.0; v0.1.0 is the "previous" one and must stay.
        let mut gone = prune_candidates(root, "", 2, &["v0.5.0", "v0.1.0"]);
        gone.sort();
        assert_eq!(gone, [root.join("v0.2.0"), root.join("v0.3.0")]);
        // Shared prefix only sees its own tags.
        fs::create_dir(root.join("cache-v0.1.0")).expect("mkdir");
        assert!(prune_candidates(root, "cache-", 0, &[]) == [root.join("cache-v0.1.0")]);
        assert!(prune_candidates(&root.join("missing"), "", 0, &[]).is_empty());
    }

    #[test]
    fn name_checks() {
        assert!(is_service_name("migrate") && is_service_name("kafka_init.1"));
        assert!(!is_service_name("-rm") && !is_service_name("") && !is_service_name("a b"));
    }

    /// Records commands and fails everything (no git, no docker).
    #[derive(Default)]
    struct Rec(Mutex<Vec<Cmd>>);
    impl Runner for Rec {
        fn run(&self, cmd: &Cmd) -> Result<Output, ExecError> {
            self.0.lock().expect("lock").push(cmd.clone());
            Ok(Output {
                code: 1,
                ..Output::default()
            })
        }
    }

    fn test_config(base: &Path) -> Config {
        let mut c = Config::blp_example();
        c.deploy.base_dir = base.join("base");
        c.deploy.apps_dir = base.join("apps");
        c.deploy.secrets_dir = base.join("base/secrets");
        c.deploy.shared_env_file = Some(base.join("base/secrets/ppe.env"));
        c.deploy.gcloud_config_dir = base.join("gcloud");
        c
    }

    #[test]
    fn bad_name_and_tag_are_usage_errors_before_any_command() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = test_config(dir.path());
        let r = Rec::default();
        for (name, tag) in [
            ("../x", "v1"),
            ("Demo", "v1.0.0"),
            ("demo-ui", "v1.0;id"),
            ("demo-ui", "--help"),
            ("demo-ui", "other-v1.0.0"),
            ("demo-ui", "v1.0.0-rc1"),
        ] {
            assert_eq!(
                deploy(&r, &cfg, name, tag, false),
                DeployCode::Usage,
                "{name} {tag}"
            );
        }
        assert!(r.0.lock().expect("lock").is_empty());
        assert!(!cfg.deploy.base_dir.exists(), "nothing created");
    }

    #[test]
    fn shared_tag_without_shared_repo_is_usage() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut cfg = test_config(dir.path());
        cfg.shared = None;
        let r = Rec::default();
        assert_eq!(
            deploy(&r, &cfg, "cache", "cache-v1.0.0", false),
            DeployCode::Usage
        );
    }

    #[test]
    fn unknown_tag_is_usage_and_uses_cloudsdk_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = test_config(dir.path());
        let r = Rec::default();
        assert_eq!(
            deploy(&r, &cfg, "demo-ui", "v9.0.0", false),
            DeployCode::Usage
        );
        let cmds = r.0.lock().expect("lock");
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].program, "git");
        assert!(
            cmds[0]
                .args
                .contains(&"refs/tags/v9.0.0^{commit}".to_owned())
        );
        assert_eq!(
            cmds[0].env.get("CLOUDSDK_CONFIG"),
            Some(&s(&cfg.deploy.gcloud_config_dir))
        );
    }

    #[test]
    fn busy_when_lock_is_held() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = test_config(dir.path());
        fs::create_dir_all(cfg.deploy.locks_dir()).expect("locks");
        let _held = AppLock::try_lock(&cfg.deploy.locks_dir().join("demo-ui.lock")).expect("lock");
        let r = Rec::default();
        assert_eq!(
            deploy(&r, &cfg, "demo-ui", "v1.0.0", false),
            DeployCode::Busy
        );
        assert!(r.0.lock().expect("lock").is_empty());
    }

    #[test]
    fn expired_deadline_times_out_without_state() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = test_config(dir.path());
        let r = Rec::default();
        let past = Instant::now();
        assert_eq!(
            deploy_until(&r, &cfg, "demo-ui", "v1.0.0", false, Some(past)),
            DeployCode::TimedOut
        );
        assert!(r.0.lock().expect("lock").is_empty());
        assert!(!cfg.deploy.state_dir().join("demo-ui.failed").exists());
    }
}
