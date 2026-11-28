// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! `fornax-gcp server init` — on-prem server side, run with sudo.
//!
//! Folders with exact modes, the key check, the agent's own gcloud login, the shared env
//! file, `/etc/fornax-gcp/config.toml` and the systemd unit. Filesystem access goes through
//! [`Env`] so tests run the whole flow against a temporary root without being root.

use std::io::ErrorKind;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};

use super::{ask_gcp_part, ask_server_part, changed, ensure_valid, ok, skipped, step, suggestions};
use crate::config::{self, Config};
use crate::doctor;
use crate::exec::{self, Cmd, Runner};
use crate::prompt::{DialoguerPrompter, Prompter};

/// Name of the systemd unit.
pub const UNIT_NAME: &str = "fornax-gcp-agent.service";
/// Where the full unit is written when the binary was installed without the package.
pub const UNIT_PATH: &str = "/etc/systemd/system/fornax-gcp-agent.service";
/// The unit shipped by the `.deb` / `.rpm` (disabled, no `User=`).
pub const PACKAGED_UNIT_PATH: &str = "/usr/lib/systemd/system/fornax-gcp-agent.service";
/// Host-specific settings for the packaged unit (packaging contract, D-046).
pub const DROPIN_PATH: &str = "/etc/systemd/system/fornax-gcp-agent.service.d/override.conf";
/// The binary systemd runs (installed by the package).
pub const AGENT_BIN: &str = "/usr/bin/fornax-gcp";
/// Installed configuration (root-owned, 0644, no secrets).
pub const INSTALLED_CONFIG: &str = config::DEFAULT_SERVER_CONFIG;

const STEPS: u8 = 7;

/// Characters systemd would interpret in `Environment=` (quoting, specifiers, escapes).
fn unit_safe(s: &str) -> bool {
    !s.is_empty()
        && !s
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || "\"'\\%$;".contains(c))
}

/// Drop-in for the packaged unit: the deploy user and the agent's gcloud folder.
///
/// Same validation as [`systemd_unit`].
pub fn systemd_dropin(cfg: &Config) -> Result<String> {
    systemd_unit(cfg)?;
    Ok(format!(
        "# Written by `fornax-gcp server init`; re-run it to change this file.\n\
         [Service]\n\
         User={}\n\
         Environment=CLOUDSDK_CONFIG={}\n",
        cfg.deploy.user,
        cfg.deploy.gcloud_config_dir.to_string_lossy()
    ))
}

/// The `fornax-gcp-agent.service` unit for `cfg` (also used by the packaging).
///
/// Errors when the user or gcloud folder holds characters systemd would interpret.
pub fn systemd_unit(cfg: &Config) -> Result<String> {
    let user = &cfg.deploy.user;
    let gcloud = cfg.deploy.gcloud_config_dir.to_string_lossy();
    if !config::is_valid_user(user) {
        bail!("deploy.user '{user}' is not a valid Linux user name");
    }
    if !unit_safe(&gcloud) {
        bail!(
            "deploy.gcloud_config_dir '{gcloud}' contains spaces, quotes, '%', '$' or '\\', which systemd would interpret"
        );
    }
    Ok(format!(
        "\
# Installed by `fornax-gcp server init`; re-run it to change this file.
[Unit]
Description=fornax-gcp deploy agent (Pub/Sub wake-ups + tag reconcile)
Wants=network-online.target
After=network-online.target docker.service

[Service]
Type=simple
User={user}
Environment=CLOUDSDK_CONFIG={gcloud}
Environment=FORNAX_GCP_CONFIG={INSTALLED_CONFIG}
ExecStart={AGENT_BIN} agent run
Restart=always
RestartSec=10
KillMode=mixed
TimeoutStopSec=600
NoNewPrivileges=yes
PrivateTmp=yes

[Install]
WantedBy=multi-user.target
"
    ))
}

/// Looks up users and groups.
pub trait Accounts {
    /// `(uid, primary gid)` of `name`, or `None` when the user does not exist.
    fn user(&self, name: &str) -> Result<Option<(u32, u32)>>;
    /// True when `user` is a member of `group` (supplementary or primary).
    fn in_group(&self, user: &str, group: &str) -> Result<bool>;
}

/// The system's user database (NSS) via `nix`.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemAccounts;

impl Accounts for SystemAccounts {
    fn user(&self, name: &str) -> Result<Option<(u32, u32)>> {
        let u = nix::unistd::User::from_name(name)
            .with_context(|| format!("looking up user {name}"))?;
        Ok(u.map(|u| (u.uid.as_raw(), u.gid.as_raw())))
    }

    fn in_group(&self, user: &str, group: &str) -> Result<bool> {
        let Some(g) = nix::unistd::Group::from_name(group)
            .with_context(|| format!("looking up group {group}"))?
        else {
            return Ok(false);
        };
        if g.mem.iter().any(|m| m == user) {
            return Ok(true);
        }
        Ok(self
            .user(user)?
            .is_some_and(|(_, gid)| gid == g.gid.as_raw()))
    }
}

/// Where `server init` works: filesystem root, privileges, tools, account database.
pub struct Env<'a> {
    /// Filesystem root every configured absolute path is resolved under (`/` for real).
    pub root: PathBuf,
    /// Running as root (required unless `--dry-run`).
    pub is_root: bool,
    /// `setfacl` is installed (ACLs are cleared with it).
    pub has_setfacl: bool,
    /// User / group lookups.
    pub accounts: &'a dyn Accounts,
}

impl Env<'static> {
    /// The real system.
    pub fn system() -> Self {
        Self {
            root: PathBuf::from("/"),
            is_root: nix::unistd::geteuid().is_root(),
            has_setfacl: exec::which("setfacl").is_some(),
            accounts: &SystemAccounts,
        }
    }
}

impl Env<'_> {
    /// `path` (absolute, as configured) resolved under [`Env::root`].
    pub fn path(&self, path: &Path) -> PathBuf {
        self.root.join(path.strip_prefix("/").unwrap_or(path))
    }

    /// Installing into the real `/` (then installed files are chowned to root).
    fn is_system(&self) -> bool {
        self.root == Path::new("/")
    }
}

/// `server init` on the terminal against the real system.
pub fn init(r: &dyn Runner, config: &Path, yes: bool) -> Result<()> {
    init_with(r, &DialoguerPrompter { yes }, &Env::system(), config)
}

/// Problems that stop a real run; in `--dry-run` they are collected and reported instead.
struct Problems {
    dry: bool,
    list: Vec<String>,
}

impl Problems {
    fn report(&mut self, e: anyhow::Error) -> Result<()> {
        if self.dry {
            println!("      ! would stop here: {e:#}");
            self.list.push(format!("{e:#}"));
            Ok(())
        } else {
            Err(e)
        }
    }
}

fn utf8(p: &Path) -> Result<&str> {
    p.to_str()
        .ok_or_else(|| anyhow!("{} is not valid UTF-8", p.display()))
}

fn chown(path: &Path, uid: u32, gid: u32) -> Result<()> {
    nix::unistd::chown(
        path,
        Some(nix::unistd::Uid::from_raw(uid)),
        Some(nix::unistd::Gid::from_raw(gid)),
    )
    .with_context(|| format!("chown {uid}:{gid} {}", path.display()))
}

fn chmod(path: &Path, mode: u32) -> Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .with_context(|| format!("chmod {mode:04o} {}", path.display()))
}

/// `server init` with injectable prompter and environment (design steps 1–7).
pub fn init_with(
    r: &dyn Runner,
    p: &dyn Prompter,
    env: &Env<'_>,
    config_path: &Path,
) -> Result<()> {
    let dry = r.is_dry_run();
    if !dry && !env.is_root {
        bail!("server init must run as root: sudo fornax-gcp server init (or try --dry-run)");
    }
    let mut problems = Problems {
        dry,
        list: Vec::new(),
    };

    let cfg = if config_path.exists() {
        println!("using {} for the suggested values", config_path.display());
        ask_server_part(p, Config::load(config_path)?)?
    } else {
        println!(
            "no configuration at {}: asking for everything",
            config_path.display()
        );
        ask_server_part(p, ask_gcp_part(p, suggestions(None))?)?
    };
    ensure_valid(&cfg)?;
    let unit = systemd_unit(&cfg)?;
    let d = &cfg.deploy;

    step(1, STEPS, "doctor --mode server");
    doctor::require(r, doctor::Mode::Server)?;
    let ids = match env.accounts.user(&d.user)? {
        Some(ids) => {
            if env.accounts.in_group(&d.user, "docker")? {
                ok(&format!(
                    "tools present; user {} is in the docker group",
                    d.user
                ));
            } else {
                problems.report(anyhow!(
                    "user {u} is not in the docker group: sudo usermod -aG docker {u}",
                    u = d.user
                ))?;
            }
            Some(ids)
        }
        None => {
            problems.report(anyhow!(
                "user {u} does not exist: sudo useradd --create-home {u} && sudo usermod -aG docker {u}",
                u = d.user
            ))?;
            None
        }
    };

    step(2, STEPS, "folders (owner deploy user, mode 0700, no ACLs)");
    let mut dirs: Vec<(PathBuf, bool)> = vec![(d.base_dir.clone(), true)];
    for sub in ["bin", "state", "locks", "releases", "repos"] {
        dirs.push((d.base_dir.join(sub), false));
    }
    for dir in [&d.secrets_dir, &d.gcloud_config_dir, &d.apps_dir] {
        if !dirs.iter().any(|(x, _)| x == dir) {
            dirs.push((dir.clone(), false));
        }
    }
    for (dir, recursive) in &dirs {
        ensure_dir(r, env, dir, *recursive, ids)?;
    }

    step(3, STEPS, "service-account key");
    match check_key(p, env, &d.service_account_key, &d.user, ids, dry) {
        Ok(()) => ok(&format!(
            "{} is private to {}",
            d.service_account_key.display(),
            d.user
        )),
        Err(e) => problems.report(e)?,
    }

    step(4, STEPS, "agent gcloud login (as the deploy user)");
    let gcloud_dir = env.path(&d.gcloud_config_dir);
    let key = env.path(&d.service_account_key);
    let as_user = |args: &[&str]| -> Result<Cmd> {
        Ok(Cmd::new("runuser")
            .args(["-u", d.user.as_str(), "--"])
            .args(args.iter().copied())
            .env("CLOUDSDK_CONFIG", utf8(&gcloud_dir)?)
            .cwd(gcloud_dir.clone()))
    };
    run_ok(
        r,
        &as_user(&[
            "gcloud",
            "auth",
            "activate-service-account",
            "--key-file",
            utf8(&key)?,
        ])?,
    )?;
    changed(
        dry,
        &format!(
            "activate {} for {}",
            cfg.gcp.service_account_email(),
            d.user
        ),
    );
    match cfg.gcp.docker_registry_host() {
        Some(host) => {
            run_ok(
                r,
                &as_user(&["gcloud", "auth", "configure-docker", &host, "--quiet"])?,
            )?;
            changed(
                dry,
                &format!("configure the docker credential helper for {host}"),
            );
        }
        None => skipped("no Artifact Registry configured"),
    }

    step(5, STEPS, "shared env file");
    match &d.shared_env_file {
        Some(f) => ensure_env_file(env, f, ids, dry)?,
        None => skipped("none configured"),
    }

    step(6, STEPS, "configuration and systemd unit");
    let installed = env.path(Path::new(INSTALLED_CONFIG));
    install_file(env, &installed, &cfg.to_toml()?, dry)?;
    if env.path(Path::new(PACKAGED_UNIT_PATH)).exists() {
        // Package installed: keep its unit, add only the host settings (no second unit file).
        install_file(
            env,
            &env.path(Path::new(DROPIN_PATH)),
            &systemd_dropin(&cfg)?,
            dry,
        )?;
    } else {
        install_file(env, &env.path(Path::new(UNIT_PATH)), &unit, dry)?;
    }
    run_ok(r, &Cmd::new("systemctl").arg("daemon-reload"))?;
    changed(dry, "systemctl daemon-reload");

    step(7, STEPS, "smoke test and start");
    // `agent run --once` before the service starts, so the two never compete for app locks.
    let smoke = as_user(&[
        AGENT_BIN,
        "agent",
        "run",
        "--once",
        "--config",
        utf8(&installed)?,
    ])?
    .timeout(Duration::from_secs(
        cfg.agent.deploy_timeout_secs.saturating_add(300),
    ));
    match r.run(&smoke) {
        Ok(_) if dry => changed(true, "run agent run --once as a smoke test"),
        Ok(o) if o.ok() => ok("agent run --once succeeded"),
        Ok(o) => println!(
            "      ! agent run --once exited with {}: {} (the service starts anyway; check the log)",
            o.code,
            o.stderr.trim().lines().last().unwrap_or("")
        ),
        Err(e) => println!("      ! agent run --once could not start: {e}"),
    }
    run_ok(
        r,
        &Cmd::new("systemctl").args(["enable", "--now", UNIT_NAME]),
    )?;
    changed(dry, &format!("systemctl enable --now {UNIT_NAME}"));
    println!("watch the agent:   journalctl -u fornax-gcp-agent -f");
    println!(
        "after re-running server init on a running agent: sudo systemctl restart fornax-gcp-agent"
    );

    if !problems.list.is_empty() {
        println!(
            "dry-run: {} problem(s) would stop a real run:",
            problems.list.len()
        );
        for (i, e) in problems.list.iter().enumerate() {
            println!("  {}. {e}", i + 1);
        }
    }
    Ok(())
}

fn run_ok(r: &dyn Runner, cmd: &Cmd) -> Result<()> {
    let out = r
        .run(cmd)
        .with_context(|| format!("running {}", cmd.display()))?;
    if !out.ok() {
        bail!(
            "{} failed (exit {}): {}",
            cmd.display(),
            out.code,
            out.stderr.trim()
        );
    }
    Ok(())
}

/// Create `dir` (if missing), clear ACLs, chown to the deploy user, chmod 0700.
fn ensure_dir(
    r: &dyn Runner,
    env: &Env<'_>,
    dir: &Path,
    recursive_acl: bool,
    ids: Option<(u32, u32)>,
) -> Result<()> {
    let real = env.path(dir);
    let existed = match std::fs::symlink_metadata(&real) {
        Ok(m) if m.file_type().is_symlink() => {
            bail!("{} is a symlink; refusing to use it", dir.display())
        }
        Ok(m) if !m.is_dir() => bail!("{} exists and is not a folder", dir.display()),
        Ok(_) => true,
        Err(e) if e.kind() == ErrorKind::NotFound => false,
        Err(e) => return Err(e).with_context(|| format!("checking {}", dir.display())),
    };
    if !r.is_dry_run() {
        std::fs::create_dir_all(&real).with_context(|| format!("creating {}", dir.display()))?;
    }
    if env.has_setfacl {
        let mut cmd = Cmd::new("setfacl");
        if recursive_acl {
            cmd = cmd.arg("-R");
        }
        run_ok(r, &cmd.args(["-b", "-k", utf8(&real)?]))?;
    }
    if r.is_dry_run() {
        let verb = if existed { "set" } else { "create" };
        changed(true, &format!("{verb} {} (0700)", dir.display()));
        return Ok(());
    }
    let (uid, gid) = ids.ok_or_else(|| anyhow!("deploy user not found"))?;
    chown(&real, uid, gid)?;
    chmod(&real, 0o700)?;
    if existed {
        ok(&format!("{} (0700)", dir.display()));
    } else {
        changed(false, &format!("{} (0700)", dir.display()));
    }
    Ok(())
}

/// Exists, absolute, regular file (not a symlink), owner root or the deploy user, no group /
/// other bits (fixed to 0600 after asking), readable by the deploy user.
fn check_key(
    p: &dyn Prompter,
    env: &Env<'_>,
    key: &Path,
    user: &str,
    ids: Option<(u32, u32)>,
    dry: bool,
) -> Result<()> {
    if !key.is_absolute() {
        bail!("deploy.service_account_key must be an absolute path");
    }
    let real = env.path(key);
    let meta = match std::fs::symlink_metadata(&real) {
        Ok(m) => m,
        Err(e) if e.kind() == ErrorKind::NotFound => bail!(
            "the service-account key {} does not exist: create it on the admin machine \
             (fornax-gcp gcp create-key) and copy it here",
            key.display()
        ),
        Err(e) => return Err(e).with_context(|| format!("checking {}", key.display())),
    };
    if meta.file_type().is_symlink() {
        bail!(
            "{} is a symlink; put the key file itself there",
            key.display()
        );
    }
    if !meta.is_file() {
        bail!("{} is not a regular file", key.display());
    }
    let user_uid = ids.map(|(u, _)| u);
    if meta.uid() != 0 && Some(meta.uid()) != user_uid {
        bail!(
            "{} is owned by uid {}; it must be owned by root or {user}: sudo chown {user}: {}",
            key.display(),
            meta.uid(),
            key.display()
        );
    }
    let mode = meta.mode() & 0o7777;
    if mode & 0o077 != 0 {
        if !p.confirm(
            &format!(
                "{} has mode {mode:04o} (group/other may read it). Change it to 0600?",
                key.display()
            ),
            true,
        )? {
            bail!(
                "{} must not be readable by group or others (mode {mode:04o})",
                key.display()
            );
        }
        if dry {
            changed(true, &format!("chmod 0600 {}", key.display()));
        } else {
            chmod(&real, 0o600)?;
            changed(false, &format!("chmod 0600 {}", key.display()));
        }
    }
    if meta.uid() == 0 && Some(0) != user_uid {
        // gcloud runs as the deploy user and must read the key.
        if !p.confirm(
            &format!(
                "{} is owned by root; give it to {user} so the agent can read it?",
                key.display()
            ),
            true,
        )? {
            bail!("{user} cannot read {} (owned by root, 0600)", key.display());
        }
        match ids {
            Some((uid, gid)) if !dry => chown(&real, uid, gid)?,
            _ => changed(true, &format!("chown {user} {}", key.display())),
        }
    }
    Ok(())
}

/// Create the shared env file (0600, deploy user) when missing; never overwrite.
fn ensure_env_file(env: &Env<'_>, file: &Path, ids: Option<(u32, u32)>, dry: bool) -> Result<()> {
    let real = env.path(file);
    if std::fs::symlink_metadata(&real).is_ok() {
        ok(&format!("{} exists (not changed)", file.display()));
        return Ok(());
    }
    if dry {
        changed(true, &format!("create {} (empty, 0600)", file.display()));
        return Ok(());
    }
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&real)
        .with_context(|| format!("creating {}", file.display()))?;
    chmod(&real, 0o600)?;
    if let Some((uid, gid)) = ids {
        chown(&real, uid, gid)?;
    }
    changed(false, &format!("{} (empty, 0600)", file.display()));
    Ok(())
}

/// Write `content` to `real` with mode 0644 (root-owned on the real system).
fn install_file(env: &Env<'_>, real: &Path, content: &str, dry: bool) -> Result<()> {
    let shown = real.strip_prefix(&env.root).map_or_else(
        |_| real.display().to_string(),
        |p| format!("/{}", p.display()),
    );
    if dry {
        changed(true, &format!("install {shown} (0644 root)"));
        return Ok(());
    }
    if let Some(dir) = real.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    if std::fs::read_to_string(real).is_ok_and(|old| old == content) {
        chmod(real, 0o644)?;
        ok(&format!("{shown} unchanged"));
        return Ok(());
    }
    std::fs::write(real, content).with_context(|| format!("writing {shown}"))?;
    chmod(real, 0o644)?;
    if env.is_system() {
        chown(real, 0, 0)?;
    }
    changed(false, &format!("{shown} (0644 root)"));
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::super::fake::{FakeRunner, line};
    use super::*;
    use crate::exec::Output;
    use crate::prompt::ScriptedPrompter;

    /// Every user is the current user and in every group.
    struct Me;

    impl Accounts for Me {
        fn user(&self, name: &str) -> Result<Option<(u32, u32)>> {
            Ok((name != "nobody-here").then(|| {
                (
                    nix::unistd::getuid().as_raw(),
                    nix::unistd::getgid().as_raw(),
                )
            }))
        }
        fn in_group(&self, _user: &str, _group: &str) -> Result<bool> {
            Ok(true)
        }
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        root: PathBuf,
        config: PathBuf,
        cfg: Config,
    }

    /// A temp root, the BLP example config, and a private key at its configured path.
    fn fixture(key_mode: Option<u32>) -> Fixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("root");
        std::fs::create_dir(&root).expect("root");
        let cfg = Config::blp_example();
        let config = dir.path().join("fornax-gcp.toml");
        std::fs::write(&config, cfg.to_toml().expect("toml")).expect("config");
        if let Some(mode) = key_mode {
            let key = root.join(
                cfg.deploy
                    .service_account_key
                    .strip_prefix("/")
                    .expect("abs"),
            );
            std::fs::create_dir_all(key.parent().expect("parent")).expect("mkdir");
            std::fs::write(&key, "{\"type\":\"service_account\"}").expect("key");
            chmod(&key, mode).expect("chmod");
        }
        Fixture {
            _dir: dir,
            root,
            config,
            cfg,
        }
    }

    fn env(root: &Path) -> Env<'static> {
        Env {
            root: root.to_path_buf(),
            is_root: true,
            has_setfacl: true,
            accounts: &Me,
        }
    }

    fn mode(p: &Path) -> u32 {
        std::fs::metadata(p).expect("metadata").mode() & 0o7777
    }

    fn real(f: &Fixture, p: &str) -> PathBuf {
        f.root.join(p.trim_start_matches('/'))
    }

    #[test]
    fn full_flow_against_temp_root() {
        let f = fixture(Some(0o600));
        let r = FakeRunner::new(|_| Output::default());
        init_with(&r, &ScriptedPrompter::yes(), &env(&f.root), &f.config).expect("init");

        for d in [
            "/data/blp/deploy",
            "/data/blp/deploy/bin",
            "/data/blp/deploy/state",
            "/data/blp/deploy/locks",
            "/data/blp/deploy/releases",
            "/data/blp/deploy/repos",
            "/data/blp/deploy/secrets",
            "/data/blp/deploy/gcloud",
            "/data/blp/apps",
        ] {
            assert_eq!(mode(&real(&f, d)), 0o700, "{d}");
        }
        let env_file = real(&f, "/data/blp/deploy/secrets/ppe.env");
        assert_eq!(mode(&env_file), 0o600);
        assert_eq!(std::fs::read_to_string(&env_file).expect("env"), "");
        let installed = real(&f, INSTALLED_CONFIG);
        assert_eq!(mode(&installed), 0o644);
        assert_eq!(Config::load(&installed).expect("installed config"), f.cfg);
        let unit_path = real(&f, UNIT_PATH);
        assert_eq!(mode(&unit_path), 0o644);
        let unit = std::fs::read_to_string(&unit_path).expect("unit");
        assert_eq!(unit, systemd_unit(&f.cfg).expect("unit"));
        for want in [
            "User=blpdevops",
            "Environment=CLOUDSDK_CONFIG=/data/blp/deploy/gcloud",
            "Environment=FORNAX_GCP_CONFIG=/etc/fornax-gcp/config.toml",
            "ExecStart=/usr/bin/fornax-gcp agent run",
            "Restart=always",
            "RestartSec=10",
            "KillMode=mixed",
            "TimeoutStopSec=600",
            "NoNewPrivileges=yes",
            "PrivateTmp=yes",
            "After=network-online.target docker.service",
            "WantedBy=multi-user.target",
        ] {
            assert!(
                unit.lines().any(|l| l == want),
                "unit lacks {want}:\n{unit}"
            );
        }

        let calls = r.calls.lock().expect("lock").clone();
        let lines: Vec<String> = calls.iter().map(line).collect();
        let root = f.root.to_string_lossy();
        let key = format!("{root}/data/blp/deploy/secrets/srf-blp-deploy-agent-key.json");
        let gcloud_dir = format!("{root}/data/blp/deploy/gcloud");
        let pos = |want: &str| {
            lines
                .iter()
                .position(|l| l == want)
                .unwrap_or_else(|| panic!("missing {want} in {lines:#?}"))
        };
        let order = [
            pos("gcloud --version"),
            pos("docker compose version --short"),
            pos(&format!("setfacl -R -b -k {root}/data/blp/deploy")),
            pos(&format!("setfacl -b -k {root}/data/blp/apps")),
            pos(&format!(
                "runuser -u blpdevops -- gcloud auth activate-service-account --key-file {key}"
            )),
            pos(
                "runuser -u blpdevops -- gcloud auth configure-docker asia-south1-docker.pkg.dev --quiet",
            ),
            pos("systemctl daemon-reload"),
            pos(&format!(
                "runuser -u blpdevops -- /usr/bin/fornax-gcp agent run --once --config {root}/etc/fornax-gcp/config.toml"
            )),
            pos("systemctl enable --now fornax-gcp-agent.service"),
        ];
        assert!(
            order.windows(2).all(|w| w[0] < w[1]),
            "order {order:?} in {lines:#?}"
        );
        let activate = &calls[order[4]];
        assert_eq!(activate.env.get("CLOUDSDK_CONFIG"), Some(&gcloud_dir));
        assert_eq!(lines.iter().filter(|l| l.starts_with("setfacl")).count(), 9);

        // Re-run: the shared env file is never overwritten.
        std::fs::write(&env_file, "A=1\n").expect("write");
        init_with(&r, &ScriptedPrompter::yes(), &env(&f.root), &f.config).expect("re-run");
        assert_eq!(std::fs::read_to_string(&env_file).expect("env"), "A=1\n");
    }

    #[test]
    fn packaged_unit_gets_a_dropin_only() {
        let f = fixture(Some(0o600));
        let packaged = real(&f, PACKAGED_UNIT_PATH);
        std::fs::create_dir_all(packaged.parent().expect("dir")).expect("mkdir");
        std::fs::write(
            &packaged,
            "[Service]\nExecStart=/usr/bin/fornax-gcp agent run\n",
        )
        .expect("write");
        let r = FakeRunner::new(|_| Output::default());
        init_with(&r, &ScriptedPrompter::yes(), &env(&f.root), &f.config).expect("init");

        assert!(!real(&f, UNIT_PATH).exists(), "no second full unit");
        let dropin = std::fs::read_to_string(real(&f, DROPIN_PATH)).expect("drop-in");
        assert!(dropin.contains("[Service]\nUser=blpdevops\n"), "{dropin}");
        assert!(
            dropin.contains("Environment=CLOUDSDK_CONFIG=/data/blp/deploy/gcloud"),
            "{dropin}"
        );
        assert!(
            !dropin.contains("ExecStart"),
            "drop-in only overrides host settings"
        );
        assert_eq!(mode(&real(&f, DROPIN_PATH)), 0o644);
    }

    #[test]
    fn key_problems_are_clear_errors() {
        // Missing.
        let f = fixture(None);
        let r = FakeRunner::new(|_| Output::default());
        let e =
            init_with(&r, &ScriptedPrompter::yes(), &env(&f.root), &f.config).expect_err("missing");
        assert!(format!("{e:#}").contains("does not exist"), "{e:#}");
        assert!(
            !r.lines()
                .iter()
                .any(|l| l.contains("activate-service-account"))
        );

        // Symlink.
        let f = fixture(Some(0o600));
        let key = real(&f, "/data/blp/deploy/secrets/srf-blp-deploy-agent-key.json");
        let target = key.with_file_name("elsewhere.json");
        std::fs::rename(&key, &target).expect("mv");
        symlink(&target, &key).expect("symlink");
        let e =
            init_with(&r, &ScriptedPrompter::yes(), &env(&f.root), &f.config).expect_err("symlink");
        assert!(format!("{e:#}").contains("symlink"), "{e:#}");

        // Bad mode, fix declined: 8 questions accept the suggestions, then "n".
        let f = fixture(Some(0o640));
        let p = ScriptedPrompter::new(&["", "", "", "", "", "", "", "", "n"]);
        let e = init_with(&r, &p, &env(&f.root), &f.config).expect_err("bad mode");
        assert!(
            format!("{e:#}").contains("must not be readable by group or others"),
            "{e:#}"
        );
        let key = real(&f, "/data/blp/deploy/secrets/srf-blp-deploy-agent-key.json");
        assert_eq!(mode(&key), 0o640, "not changed without consent");

        // Bad mode, fix accepted (--yes takes the suggested "yes").
        init_with(&r, &ScriptedPrompter::yes(), &env(&f.root), &f.config).expect("fixed");
        assert_eq!(mode(&key), 0o600);
    }

    #[test]
    fn user_must_exist_and_root_is_required() {
        let f = fixture(Some(0o600));
        let mut cfg = f.cfg.clone();
        cfg.deploy.user = "nobody-here".into();
        std::fs::write(&f.config, cfg.to_toml().expect("toml")).expect("write");
        let r = FakeRunner::new(|_| Output::default());
        let e =
            init_with(&r, &ScriptedPrompter::yes(), &env(&f.root), &f.config).expect_err("user");
        assert!(
            format!("{e:#}").contains("does not exist: sudo useradd"),
            "{e:#}"
        );

        let mut not_root = env(&f.root);
        not_root.is_root = false;
        let e = init_with(&r, &ScriptedPrompter::yes(), &not_root, &f.config).expect_err("root");
        assert!(e.to_string().contains("must run as root"), "{e}");
    }

    #[test]
    fn dry_run_changes_nothing_on_disk() {
        let f = fixture(Some(0o644));
        let r = FakeRunner::dry();
        let mut e = env(&f.root);
        e.is_root = false;
        init_with(&r, &ScriptedPrompter::yes(), &e, &f.config).expect("dry run");
        let key = real(&f, "/data/blp/deploy/secrets/srf-blp-deploy-agent-key.json");
        assert_eq!(mode(&key), 0o644, "key mode untouched");
        let mut found = Vec::new();
        let mut stack = vec![f.root.clone()];
        while let Some(d) = stack.pop() {
            for entry in std::fs::read_dir(&d).expect("read_dir") {
                let p = entry.expect("entry").path();
                if p.is_dir() {
                    stack.push(p.clone());
                }
                found.push(p.strip_prefix(&f.root).expect("prefix").to_path_buf());
            }
        }
        found.sort();
        let expected: Vec<PathBuf> = [
            "data",
            "data/blp",
            "data/blp/deploy",
            "data/blp/deploy/secrets",
            "data/blp/deploy/secrets/srf-blp-deploy-agent-key.json",
        ]
        .iter()
        .map(PathBuf::from)
        .collect();
        assert_eq!(found, expected);
        let lines = r.lines();
        assert!(lines.iter().any(|l| l.contains("activate-service-account")));
        assert!(
            lines
                .iter()
                .any(|l| l == "systemctl enable --now fornax-gcp-agent.service")
        );
        // Missing key in dry-run is reported, not fatal.
        let g = fixture(None);
        let mut e2 = env(&g.root);
        e2.is_root = false;
        init_with(&r, &ScriptedPrompter::yes(), &e2, &g.config).expect("dry run without key");
        assert!(std::fs::read_dir(&g.root).expect("root").next().is_none());
    }

    #[test]
    fn unit_rejects_unsafe_paths() {
        let mut cfg = Config::blp_example();
        cfg.deploy.gcloud_config_dir = "/data/my gcloud".into();
        assert!(systemd_unit(&cfg).is_err());
        cfg.deploy.gcloud_config_dir = "/data/%h".into();
        assert!(systemd_unit(&cfg).is_err());
    }
}
