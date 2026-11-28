// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! The deploy agent: Pub/Sub wake-ups + tag reconcile (port of agent.py).
//!
//! 1. Pulls messages from a Cloud Pub/Sub *pull* subscription (outbound HTTPS only). A message
//!    is only a wake-up call; its content is never used.
//! 2. Fetches git tags and reconciles: for every app, deploy the newest version tag when it is
//!    not what is already deployed (shared services first, then the app repos in order).
//! 3. Deploying is done in-process by [`deploy::deploy_until`] (same as `fornax-gcp deploy`).
//!
//! Because git is the source of truth, lost or duplicate messages do no harm, and a timer
//! (`agent.reconcile_interval_secs`) catches anything missed. A tag whose deploy failed is not
//! retried until a newer tag appears (delete `state/<name>.failed` to force a retry).

pub mod deploy;
pub mod lock;
pub mod vars;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::agent::deploy::DeployCode;
use crate::config::{Config, is_valid_name};
use crate::exec::{Cmd, Runner};

/// Reset any other credential helper, then use gcloud's (service-account credentials).
const GIT_AUTH: [&str; 4] = [
    "-c",
    "credential.helper=",
    "-c",
    "credential.helper=gcloud.sh",
];

/// Systemd unit named in failure notifications.
const UNIT: &str = "fornax-gcp-agent";

/// One thing to deploy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// App / shared-service name.
    pub name: String,
    /// Clone URL.
    pub url: String,
    /// Local bare clone.
    pub git_dir: PathBuf,
    /// Tag prefix: `""` for an app repo, `<name>-` for a shared service.
    pub prefix: String,
}

/// Targets in deploy order: shared services first (database, Kafka …), then the app repos in
/// the order they are listed (jobs before the services that need them).
pub fn targets(cfg: &Config) -> Vec<Target> {
    let mut found = Vec::new();
    if let Some(sh) = &cfg.shared {
        for name in sh.apps.iter().filter(|n| is_valid_name(n)) {
            found.push(Target {
                name: name.clone(),
                url: cfg.gcp.repo_url(&sh.repo),
                git_dir: cfg.deploy.infra_git_dir(),
                prefix: format!("{name}-"),
            });
        }
    }
    for name in cfg.repos.iter().filter(|n| is_valid_name(n)) {
        found.push(Target {
            name: name.clone(),
            url: cfg.gcp.repo_url(name),
            git_dir: cfg.deploy.repos_dir().join(format!("{name}.git")),
            prefix: String::new(),
        });
    }
    found
}

/// Subject and body of the notification sent after a deploy (`None`: no notification).
pub fn notification(name: &str, tag: &str, code: DeployCode) -> Option<(String, String)> {
    match code {
        DeployCode::Ok => Some((format!("fornax deploy OK: {name} {tag}"), String::new())),
        DeployCode::Busy => None,
        DeployCode::FailedKept => Some((
            format!("fornax deploy FAILED, previous version kept: {name} {tag}"),
            format!("The previous version is running. Failed tag recorded in state/{name}.failed."),
        )),
        other => Some((
            format!(
                "fornax deploy FAILED: {name} {tag} (exit {})",
                other.as_u8()
            ),
            format!("Rollback was not possible or failed. Check: journalctl -u {UNIT}"),
        )),
    }
}

/// Wait after the `failures`-th pull error in a row: `min(300, 15 * 2^min(failures, 5))` s.
pub fn backoff(failures: u32) -> Duration {
    Duration::from_secs((15u64 << failures.min(5)).min(300))
}

/// `shlex.split` for `agent.notify_cmd` (whitespace, quotes, backslashes; no shell).
pub fn split_command(s: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '\'' => break,
                        x => cur.push(x),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' => match chars.next()? {
                            x @ ('"' | '\\' | '$' | '`') => cur.push(x),
                            '\n' => {}
                            x => {
                                cur.push('\\');
                                cur.push(x);
                            }
                        },
                        x => cur.push(x),
                    }
                }
            }
            '\\' => {
                in_word = true;
                match chars.next()? {
                    '\n' => {}
                    x => cur.push(x),
                }
            }
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            c => {
                in_word = true;
                cur.push(c);
            }
        }
    }
    if in_word {
        words.push(cur);
    }
    Some(words)
}

/// Monotonic time and sleeping, injectable for tests.
pub trait Clock {
    /// Time since an arbitrary fixed start.
    fn now(&self) -> Duration;
    /// Sleep (may return early when a stop was requested).
    fn sleep(&self, d: Duration);
}

/// Real clock whose sleep wakes up early when `stop` is set.
struct RealClock<'a> {
    start: Instant,
    stop: &'a AtomicBool,
}

impl Clock for RealClock<'_> {
    fn now(&self) -> Duration {
        self.start.elapsed()
    }

    fn sleep(&self, d: Duration) {
        let until = Instant::now() + d;
        while !self.stop.load(Ordering::SeqCst) {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return;
            }
            std::thread::sleep(left.min(Duration::from_millis(200)));
        }
    }
}

/// The main loop of agent.py: pull (blocking up to the pull timeout), back off on errors,
/// reconcile on a message (after 3 s to let a burst settle) or when the interval is due —
/// immediately at start — and never spin faster than once per 5 s.
pub fn run_loop(
    clock: &dyn Clock,
    stop: &AtomicBool,
    interval: Duration,
    pull: &mut dyn FnMut() -> Result<usize, String>,
    reconcile: &mut dyn FnMut(),
) {
    let mut last_reconcile: Option<Duration> = None;
    let mut failures: u32 = 0;
    while !stop.load(Ordering::SeqCst) {
        let started = clock.now();
        let mut got = 0;
        match pull() {
            Ok(n) => {
                got = n;
                failures = 0;
            }
            Err(e) => {
                failures = failures.saturating_add(1);
                let wait = backoff(failures);
                tracing::error!("{e} (retry in {}s)", wait.as_secs());
                clock.sleep(wait);
            }
        }
        let due = last_reconcile.is_none_or(|l| clock.now().saturating_sub(l) >= interval);
        if got > 0 || due {
            if got > 0 {
                clock.sleep(Duration::from_secs(3));
            }
            reconcile();
            last_reconcile = Some(clock.now());
        }
        let elapsed = clock.now().saturating_sub(started);
        if elapsed < Duration::from_secs(5) && !stop.load(Ordering::SeqCst) {
            clock.sleep(Duration::from_secs(5) - elapsed);
        }
    }
}

struct Agent<'a> {
    r: &'a dyn Runner,
    cfg: &'a Config,
    stop: &'a AtomicBool,
}

impl Agent<'_> {
    fn cmd(&self, program: &str) -> Cmd {
        Cmd::new(program).env(
            "CLOUDSDK_CONFIG",
            self.cfg.deploy.gcloud_config_dir.to_string_lossy(),
        )
    }

    fn git(&self, git_dir: &Path) -> Cmd {
        self.cmd("git")
            .arg(format!("--git-dir={}", git_dir.to_string_lossy()))
    }

    fn read_state(&self, name: &str, kind: &str) -> String {
        std::fs::read_to_string(self.cfg.deploy.state_dir().join(format!("{name}.{kind}")))
            .map(|s| s.trim().to_owned())
            .unwrap_or_default()
    }

    fn fetch_tags(&self, url: &str, git_dir: &Path) -> Result<(), String> {
        let ten_min = Duration::from_secs(600);
        if !git_dir.join("HEAD").exists() {
            tracing::info!("cloning {url}");
            if let Some(parent) = git_dir.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("creating {}: {e}", parent.display()))?;
            }
            let out = self
                .r
                .run(
                    &self
                        .cmd("git")
                        .args(GIT_AUTH)
                        .args(["clone", "--bare", "--", url])
                        .arg(git_dir.to_string_lossy())
                        .timeout(ten_min),
                )
                .map_err(|e| format!("git clone of {url} failed: {e}"))?;
            if !out.ok() {
                return Err(format!("git clone of {url} failed: {}", out.stderr.trim()));
            }
        }
        let out = self
            .r
            .run(
                &self
                    .git(git_dir)
                    .args(GIT_AUTH)
                    .args([
                        "fetch",
                        "--prune",
                        "--force",
                        "origin",
                        "+refs/tags/*:refs/tags/*",
                    ])
                    .timeout(ten_min),
            )
            .map_err(|e| format!("git fetch of {url} failed: {e}"))?;
        if !out.ok() {
            return Err(format!("git fetch of {url} failed: {}", out.stderr.trim()));
        }
        Ok(())
    }

    fn latest_tag(&self, t: &Target) -> Option<String> {
        let out = self
            .r
            .run(
                &self
                    .git(&t.git_dir)
                    .args(["tag", "-l"])
                    .arg(format!("{}v*", t.prefix))
                    .arg("--sort=-v:refname")
                    .timeout(Duration::from_secs(60)),
            )
            .ok()?;
        if !out.ok() {
            return None;
        }
        vars::select_latest(&t.prefix, &out.stdout)
    }

    fn notify(&self, subject: &str, body: &str) {
        tracing::info!("NOTIFY: {subject}");
        let line = self.cfg.agent.notify_cmd.trim();
        if line.is_empty() {
            return;
        }
        // A broken notifier must never stop deploys.
        let Some(argv) = split_command(line).filter(|a| !a.is_empty()) else {
            tracing::warn!("notify command failed: cannot parse agent.notify_cmd");
            return;
        };
        let cmd = Cmd::new(&argv[0])
            .args(&argv[1..])
            .stdin(format!("{subject}\n\n{body}\n"))
            .timeout(Duration::from_secs(30));
        match self.r.run(&cmd) {
            Ok(out) if out.timed_out => tracing::warn!("notify command failed: timed out"),
            Ok(_) => {}
            Err(e) => tracing::warn!("notify command failed: {e}"),
        }
    }

    fn deploy(&self, name: &str, tag: &str) {
        tracing::info!("deploying {name} at {tag}");
        let deadline = deploy::deadline_after(self.cfg.agent.deploy_timeout_secs);
        let code = deploy::deploy_until(self.r, self.cfg, name, tag, false, deadline);
        if code == DeployCode::Busy {
            tracing::info!("{name} is busy (another deploy running); will retry next cycle");
        }
        if let Some((subject, body)) = notification(name, tag, code) {
            self.notify(&subject, &body);
        }
    }

    fn reconcile(&self, wanted: &[Target]) {
        // The infra repo serves several shared services: fetch it once.
        let mut fetched: HashMap<PathBuf, bool> = HashMap::new();
        for t in wanted {
            if self.stop.load(Ordering::SeqCst) {
                return;
            }
            let ok = *fetched.entry(t.git_dir.clone()).or_insert_with(|| {
                match self.fetch_tags(&t.url, &t.git_dir) {
                    Ok(()) => true,
                    Err(e) => {
                        // One broken repo must not block the others.
                        tracing::error!("{e}");
                        false
                    }
                }
            });
            if !ok {
                continue;
            }
            let name = t.name.as_str();
            match self.latest_tag(t) {
                None => tracing::info!("{name}: no tag {}v* yet", t.prefix),
                Some(tag) if self.read_state(name, "deployed") == tag => {
                    tracing::info!("{name}: {tag} already deployed");
                }
                Some(tag) if self.read_state(name, "failed") == tag => tracing::warn!(
                    "{name}: {tag} failed earlier; waiting for a newer tag \
                     (or delete state/{name}.failed to retry)"
                ),
                Some(tag) => self.deploy(name, &tag),
            }
        }
    }

    /// Pull and acknowledge pending messages; returns how many.
    fn pull_messages(&self) -> Result<usize, String> {
        let g = &self.cfg.gcp;
        let cmd = self
            .cmd("gcloud")
            .args(["pubsub", "subscriptions", "pull"])
            .arg(g.subscription.as_str())
            .args(["--project", g.project.as_str()])
            .args(["--limit", "10", "--auto-ack", "--format", "json", "--quiet"])
            .timeout(Duration::from_secs(self.cfg.agent.pull_timeout_secs));
        let out = self
            .r
            .run(&cmd)
            .map_err(|e| format!("gcloud pull failed: {e}"))?;
        if out.timed_out {
            return Ok(0);
        }
        if !out.ok() {
            let err: String = out.stderr.trim().chars().take(300).collect();
            return Err(format!("gcloud pull failed: {err}"));
        }
        Ok(count_messages(&out.stdout))
    }
}

/// Count the messages of a `gcloud … pull --format json` answer, logging their ids only (the
/// content is not trusted or needed).
pub fn count_messages(json: &str) -> usize {
    let text = if json.trim().is_empty() { "[]" } else { json };
    let Ok(serde_json::Value::Array(items)) = serde_json::from_str::<serde_json::Value>(text)
    else {
        return 0;
    };
    for item in &items {
        let id = item
            .get("message")
            .and_then(|m| m.get("messageId"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("?");
        tracing::info!("wake-up message {}", id.escape_debug());
    }
    items.len()
}

/// `agent run [--once]`.
pub fn run(r: &dyn Runner, cfg: &Config, once: bool) -> anyhow::Result<()> {
    let wanted = targets(cfg);
    if wanted.is_empty() {
        anyhow::bail!("repos and shared.apps name nothing to deploy");
    }
    std::fs::create_dir_all(cfg.deploy.state_dir())?;
    let stop = Arc::new(AtomicBool::new(false));
    let agent = Agent {
        r,
        cfg,
        stop: &stop,
    };
    if once {
        agent.reconcile(&wanted);
        return Ok(());
    }

    // SIGTERM / SIGINT: finish the current step, then exit. A second Ctrl-C exits at once.
    for sig in [signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT] {
        signal_hook::flag::register_conditional_shutdown(sig, 1, Arc::clone(&stop))?;
        signal_hook::flag::register(sig, Arc::clone(&stop))?;
    }
    let names: Vec<&str> = wanted.iter().map(|t| t.name.as_str()).collect();
    tracing::info!(
        "agent started: {names:?} subscription={} interval={}s",
        cfg.gcp.subscription,
        cfg.agent.reconcile_interval_secs
    );
    let clock = RealClock {
        start: Instant::now(),
        stop: &stop,
    };
    run_loop(
        &clock,
        &stop,
        Duration::from_secs(cfg.agent.reconcile_interval_secs),
        &mut || agent.pull_messages(),
        &mut || agent.reconcile(&wanted),
    );
    if stop.load(Ordering::SeqCst) {
        tracing::info!("signal received: finished the current step");
    }
    tracing::info!("agent stopped");
    Ok(())
}

/// `status`: what is deployed and what failed, per app.
pub fn status(cfg: &Config) -> anyhow::Result<()> {
    let state = cfg.deploy.state_dir();
    let read = |name: &str, kind: &str| {
        std::fs::read_to_string(state.join(format!("{name}.{kind}")))
            .map(|s| s.trim().to_owned())
            .unwrap_or_default()
    };
    let rows: Vec<[String; 4]> = targets(cfg)
        .into_iter()
        .map(|t| {
            let commit: String = read(&t.name, "commit").chars().take(12).collect();
            let dash = |s: String| if s.is_empty() { "-".to_owned() } else { s };
            [
                t.name.clone(),
                dash(read(&t.name, "deployed")),
                dash(commit),
                dash(read(&t.name, "failed")),
            ]
        })
        .collect();
    print!("{}", status_table(&rows));
    Ok(())
}

fn status_table(rows: &[[String; 4]]) -> String {
    let head = ["NAME", "DEPLOYED", "COMMIT", "FAILED"];
    let mut w = head.map(str::len);
    for r in rows {
        for (i, c) in r.iter().enumerate() {
            w[i] = w[i].max(c.len());
        }
    }
    let line = |c: [&str; 4]| {
        format!(
            "{:<a$}  {:<b$}  {:<c$}  {}\n",
            c[0],
            c[1],
            c[2],
            c[3],
            a = w[0],
            b = w[1],
            c = w[2]
        )
    };
    let mut out = line(head);
    for r in rows {
        out.push_str(&line([&r[0], &r[1], &r[2], &r[3]]));
    }
    out
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::sync::Mutex;

    use super::*;
    use crate::exec::{ExecError, Output};

    #[test]
    fn targets_shared_first_then_repos_in_order() {
        let cfg = Config::blp_example();
        let t = targets(&cfg);
        let names: Vec<&str> = t.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(
            names[..3],
            ["platform", "testcameras", "srf-blp-kafka-init"]
        );
        assert_eq!(t[0].prefix, "platform-");
        assert_eq!(t[0].git_dir, Path::new("/data/blp/deploy/repo.git"));
        assert_eq!(
            t[0].url,
            "https://source.developers.google.com/p/visual-analytics-1/r/srf-blp-infra-configuration"
        );
        assert_eq!(t[2].prefix, "");
        assert_eq!(
            t[2].git_dir,
            Path::new("/data/blp/deploy/repos/srf-blp-kafka-init.git")
        );
    }

    #[test]
    fn notification_subjects_per_exit_code() {
        let s = |c| notification("ui", "v1", c).map(|(s, _)| s);
        assert_eq!(
            s(DeployCode::Ok).as_deref(),
            Some("fornax deploy OK: ui v1")
        );
        assert_eq!(s(DeployCode::Busy), None);
        assert_eq!(
            s(DeployCode::FailedKept).as_deref(),
            Some("fornax deploy FAILED, previous version kept: ui v1")
        );
        for (c, n) in [
            (DeployCode::FailedNotRestored, 4),
            (DeployCode::Usage, 64),
            (DeployCode::Error, 1),
            (DeployCode::TimedOut, 124),
        ] {
            assert_eq!(
                s(c),
                Some(format!("fornax deploy FAILED: ui v1 (exit {n})"))
            );
        }
        let (_, body) = notification("ui", "v1", DeployCode::FailedKept).expect("some");
        assert!(body.contains("state/ui.failed"));
    }

    #[test]
    fn backoff_doubles_up_to_five_minutes() {
        let secs: Vec<u64> = (1..=7).map(|f| backoff(f).as_secs()).collect();
        assert_eq!(secs, [30, 60, 120, 240, 300, 300, 300]);
        assert_eq!(backoff(0).as_secs(), 15);
        assert_eq!(backoff(u32::MAX).as_secs(), 300);
    }

    #[test]
    fn split_command_like_shlex() {
        assert_eq!(
            split_command(r#"/usr/bin/mail -s 'a b' "c \"d\"" e\ f"#),
            Some(vec![
                "/usr/bin/mail".into(),
                "-s".into(),
                "a b".into(),
                "c \"d\"".into(),
                "e f".into()
            ])
        );
        assert_eq!(split_command("  "), Some(vec![]));
        assert_eq!(split_command("''"), Some(vec![String::new()]));
        assert_eq!(split_command("'open"), None);
    }

    #[test]
    fn count_messages_reads_ids_only() {
        assert_eq!(count_messages(""), 0);
        assert_eq!(count_messages("[]"), 0);
        assert_eq!(count_messages("not json"), 0);
        assert_eq!(count_messages("{\"a\":1}"), 0);
        assert_eq!(
            count_messages(r#"[{"message":{"messageId":"1"}},{"ackId":"x"}]"#),
            2
        );
    }

    /// Fake time: sleeping advances the clock; records every sleep.
    #[derive(Default)]
    struct FakeClock {
        t: Cell<Duration>,
        sleeps: RefCell<Vec<u64>>,
    }
    impl Clock for FakeClock {
        fn now(&self) -> Duration {
            self.t.get()
        }
        fn sleep(&self, d: Duration) {
            self.sleeps.borrow_mut().push(d.as_secs());
            self.t.set(self.t.get() + d);
        }
    }

    #[test]
    fn loop_backs_off_and_reconciles_on_message_or_interval() {
        let clock = FakeClock::default();
        let stop = AtomicBool::new(false);
        // Script of pull results; each pull takes 1 s (except errors, instant).
        let script: Vec<Result<usize, String>> = vec![
            Ok(0),               // start: reconcile immediately (due)
            Err("boom".into()),  // backoff 30
            Err("boom".into()),  // backoff 60
            Ok(0),               // reset failures; t ~ 100 < 300 -> no reconcile
            Ok(2),               // message: sleep 3, reconcile
            Err("again".into()), // backoff 30 again (counter was reset)
        ];
        let mut i = 0;
        let reconciles = RefCell::new(Vec::new());
        let mut pull = || {
            let r = script.get(i).cloned().unwrap_or(Ok(0));
            i += 1;
            if i >= script.len() {
                stop.store(true, Ordering::SeqCst);
            }
            if r.is_ok() {
                clock.t.set(clock.t.get() + Duration::from_secs(1));
            }
            r
        };
        let mut reconcile = || reconciles.borrow_mut().push(clock.now().as_secs());
        run_loop(
            &clock,
            &stop,
            Duration::from_secs(300),
            &mut pull,
            &mut reconcile,
        );
        // 4: never spin; 30/60: backoff; 3: burst settle, then 1 to fill up the 5 s minimum.
        assert_eq!(*clock.sleeps.borrow(), [4, 30, 60, 4, 3, 1, 30]);
        assert_eq!(reconciles.borrow().len(), 2);
        assert_eq!(reconciles.borrow()[0], 1);
    }

    #[test]
    fn loop_reconciles_when_interval_is_due() {
        let clock = FakeClock::default();
        let stop = AtomicBool::new(false);
        let mut n = 0;
        let mut pull = || {
            // A pull that blocks the full 120 s timeout and returns nothing.
            clock.t.set(clock.t.get() + Duration::from_secs(120));
            n += 1;
            if n == 6 {
                stop.store(true, Ordering::SeqCst);
            }
            Ok(0)
        };
        let count = Cell::new(0);
        let mut reconcile = || count.set(count.get() + 1);
        run_loop(
            &clock,
            &stop,
            Duration::from_secs(300),
            &mut pull,
            &mut reconcile,
        );
        // t=120 (start, due), 240, 360, 480 (>=300 after 120 -> due), 600, 720
        assert_eq!(count.get(), 2);
        assert!(clock.sleeps.borrow().is_empty(), "long pulls never sleep");
    }

    #[test]
    fn stop_before_start_does_nothing() {
        let clock = FakeClock::default();
        let stop = AtomicBool::new(true);
        let mut pull = || -> Result<usize, String> { panic!("no pull after stop") };
        let mut reconcile = || panic!("no reconcile after stop");
        run_loop(
            &clock,
            &stop,
            Duration::from_secs(300),
            &mut pull,
            &mut reconcile,
        );
    }

    /// Answers git tag listing and fails everything else; records commands.
    #[derive(Default)]
    struct Fake(Mutex<Vec<Cmd>>);
    impl Runner for Fake {
        fn run(&self, cmd: &Cmd) -> Result<Output, ExecError> {
            self.0.lock().expect("lock").push(cmd.clone());
            if cmd.program == "gcloud" {
                return Ok(Output {
                    code: 0,
                    stdout: r#"[{"message":{"messageId":"42"}}]"#.into(),
                    ..Output::default()
                });
            }
            Ok(Output {
                code: 1,
                stderr: "fatal: unreachable".into(),
                ..Output::default()
            })
        }
    }

    #[test]
    fn pull_and_fetch_commands_match_agent_py() {
        let cfg = Config::blp_example();
        let stop = AtomicBool::new(false);
        let f = Fake::default();
        let a = Agent {
            r: &f,
            cfg: &cfg,
            stop: &stop,
        };
        assert_eq!(a.pull_messages(), Ok(1));
        let dir = tempfile::tempdir().expect("tempdir");
        let err = a
            .fetch_tags(
                "https://example.invalid/r/x",
                &dir.path().join("repos/x.git"),
            )
            .expect_err("fails");
        assert!(err.starts_with("git clone of https://example.invalid/r/x failed"));
        let cmds = f.0.lock().expect("lock");
        assert_eq!(
            cmds[0].args,
            [
                "pubsub",
                "subscriptions",
                "pull",
                "srf-onprem-pull-subscription",
                "--project",
                "visual-analytics-1",
                "--limit",
                "10",
                "--auto-ack",
                "--format",
                "json",
                "--quiet"
            ]
        );
        assert_eq!(cmds[0].timeout, Some(Duration::from_secs(120)));
        assert_eq!(cmds[1].args[..4], GIT_AUTH);
        assert_eq!(cmds[1].args[4..7], ["clone", "--bare", "--"]);
        for c in cmds.iter() {
            assert_eq!(
                c.env.get("CLOUDSDK_CONFIG").map(String::as_str),
                Some("/data/blp/deploy/gcloud")
            );
        }
    }

    #[test]
    fn status_table_aligns() {
        let rows = vec![[
            "demo-ui".to_owned(),
            "v0.1.0".to_owned(),
            "abc".to_owned(),
            "-".to_owned(),
        ]];
        let t = status_table(&rows);
        assert_eq!(
            t,
            "NAME     DEPLOYED  COMMIT  FAILED\ndemo-ui  v0.1.0    abc     -\n"
        );
    }
}
