// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! Port of `pub-sub-agent/test/run-tests.sh`: the 47 offline checks, run against the real
//! `fornax-gcp` binary (`agent run --once` and `deploy`). No GCP, no Docker, no network: local
//! git repos play the app repo and the infra repo (reached through an `insteadOf` rewrite of
//! the configured https base URL), and a stub `docker` (the original stub, unchanged) plays
//! Docker:
//!   - "pulls" an image the first time it sees a tag; a name containing "::" is invalid
//!   - recreates the container only when compose.yml or the image changes
//!   - `up --wait` fails when release.vars has FAIL=1, or the synced code has no build/healthz
//!
//! The scenario is sequential (each section builds on the previous state), so it runs once;
//! every original check is its own `#[test]` that reads the recorded result.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

const STUB_DOCKER: &str = r#"#!/usr/bin/env bash
echo "docker $*" >> "$STUB_LOG"
S="$STUB_STATE"
load() {  # load <id>: the "container" reads its code now (create / restart)
  local code; code=$(cat "$S/$1.appdir")
  if [[ -z $code || -f $code/build/healthz ]]; then echo ok > "$S/$1.health"; else echo bad > "$S/$1.health"; fi
}
if [[ $1 == restart ]]; then
  shift; for id in "$@"; do echo "RESTART $id" >> "$STUB_LOG"; load "$id"; done; exit 0
fi
shift  # "compose"
proj=""; dir=""
while [[ $1 == -p || $1 == -f || $1 == --project-directory || $1 == --env-file ]]; do
  [[ $1 == -p ]] && proj=$2
  [[ $1 == -f ]] && dir=$(dirname "$2")
  shift 2
done
sub=$1; shift
img=$(sed -n 's/^[A-Z_]*IMAGE=//p' "$dir/release.vars")
idfile="$S/$proj.container"
case "$sub" in
  pull)
    [[ $img == *::* ]] && { echo "invalid reference format" >&2; exit 1; }
    if ! grep -qxF "$img" "$S/images" 2>/dev/null; then echo "PULL $img" >> "$STUB_LOG"; echo "$img" >> "$S/images"; fi ;;
  ps) [[ " $* " == *" -q "* && -f $idfile ]] && cat "$idfile" ;;
  up)
    sig=$( { cat "$dir/compose.yml"; echo "$img"; } | md5sum | cut -c1-12 )
    id="$proj-$sig"
    if [[ $(cat "$idfile" 2>/dev/null) != "$id" ]]; then
      echo "$APP_DIR" > "$S/$id.appdir"; load "$id"; echo "$id" > "$idfile"; echo "CREATE $id" >> "$STUB_LOG"
    fi
    if [[ " $* " == *" --wait "* ]]; then
      if grep -q '^FAIL=1' "$dir/release.vars" || [[ $(cat "$S/$id.health") != ok ]]; then echo unhealthy >&2; exit 1; fi
      echo "UP $img" >> "$STUB_LOG"
    fi ;;
  run)
    echo "RUN ${*: -1}" >> "$STUB_LOG"
    grep -q '^FAIL=1' "$dir/release.vars" && exit 1 ;;
  down) rm -f "$idfile" ;;
esac
exit 0
"#;

/// `gcloud` stand-in: `--version` for the doctor check, empty answer for a pull.
const STUB_GCLOUD: &str = "#!/bin/sh\nexit 0\n";

const BASE_URL: &str = "https://git.example.invalid/r";

/// Temp world: stub bin dir, remotes, base/apps dirs, config file.
struct World {
    _tmp: tempfile::TempDir,
    t: PathBuf,
    transcript: String,
}

fn write_exe(path: &Path, text: &str) {
    use std::os::unix::fs::PermissionsExt;
    fs::write(path, text).expect("write stub");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("chmod stub");
}

impl World {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let t = tmp.path().to_path_buf();
        for d in ["bin", "stubstate", "remote"] {
            fs::create_dir_all(t.join(d)).expect("mkdir");
        }
        fs::write(t.join("docker.log"), "").expect("log");
        write_exe(&t.join("bin/docker"), STUB_DOCKER);
        write_exe(&t.join("bin/gcloud"), STUB_GCLOUD);
        Self {
            _tmp: tmp,
            t,
            transcript: String::new(),
        }
    }

    fn base(&self) -> PathBuf {
        self.t.join("base")
    }
    fn apps(&self) -> PathBuf {
        self.t.join("apps")
    }
    fn stub_log(&self) -> PathBuf {
        self.t.join("docker.log")
    }
    fn config_path(&self) -> PathBuf {
        self.t.join("config.toml")
    }

    /// Equivalent of the exported REPOS / APPS / KEEP_RELEASES of the shell test.
    fn write_config(&self, repos: &[&str], apps: &[&str], keep: usize) {
        let q = |p: PathBuf| format!("{:?}", p.to_string_lossy());
        let list = |v: &[&str]| {
            v.iter()
                .map(|s| format!("{s:?}"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let mut c = format!("repos = [{}]\n\n", list(repos));
        c.push_str(&format!(
            "[gcp]\nproject = \"fornax-test-1\"\ntopic = \"fornax-events\"\n\
             subscription = \"fornax-pull\"\nservice_account = \"fornax-agent\"\n\
             repo_base_url = \"{BASE_URL}\"\n\n"
        ));
        if !apps.is_empty() {
            c.push_str(&format!(
                "[shared]\nrepo = \"infra\"\napps = [{}]\napps_dir = \"apps\"\n\
                 project_prefix = \"srf-blp-\"\n\n",
                list(apps)
            ));
        }
        c.push_str(&format!(
            "[deploy]\ndeployment_dir = \".deployment\"\nuser = \"fornax\"\nbase_dir = {}\n\
             apps_dir = {}\nsecrets_dir = {}\nshared_env_file = {}\n\
             service_account_key = {}\ngcloud_config_dir = {}\n\n\
             [agent]\nkeep_releases = {keep}\n",
            q(self.base()),
            q(self.apps()),
            q(self.base().join("secrets")),
            q(self.base().join("secrets/ppe.env")),
            q(self.base().join("secrets/key.json")),
            q(self.t.join("gcloud")),
        ));
        fs::write(self.config_path(), c).expect("config");
    }

    fn bin(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_fornax-gcp"));
        let path = std::env::var("PATH").unwrap_or_default();
        c.env("PATH", format!("{}:{path}", self.t.join("bin").display()))
            .env("STUB_LOG", self.stub_log())
            .env("STUB_STATE", self.t.join("stubstate"))
            .env("GIT_CONFIG_COUNT", "1")
            .env(
                "GIT_CONFIG_KEY_0",
                format!("url.file://{}/.insteadOf", self.t.join("remote").display()),
            )
            .env("GIT_CONFIG_VALUE_0", format!("{BASE_URL}/"))
            .env("RUST_LOG", "info")
            .env("NO_COLOR", "1")
            .env_remove("FORNAX_GCP_CONFIG")
            .stdin(Stdio::null());
        c
    }

    fn run_bin(&mut self, args: &[&str]) -> (i32, String) {
        let out = self
            .bin()
            .args(args)
            .arg("--config")
            .arg(self.config_path())
            .output()
            .expect("run fornax-gcp");
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        self.transcript
            .push_str(&format!("$ fornax-gcp {}\n{text}", args.join(" ")));
        (out.status.code().unwrap_or(-1), text)
    }

    /// `agent run --once`; returns its output (`$T/agent.out`).
    fn agent(&mut self) -> String {
        self.run_bin(&["agent", "run", "--once"]).1
    }

    /// `fornax-gcp deploy …`; returns the exit code.
    fn deploy(&mut self, args: &[&str]) -> i32 {
        let mut a = vec!["deploy"];
        a.extend_from_slice(args);
        self.run_bin(&a).0
    }

    fn deployed(&self, name: &str) -> String {
        fs::read_to_string(self.base().join(format!("state/{name}.deployed")))
            .map(|s| s.trim_end().to_owned())
            .unwrap_or_default()
    }

    fn code(&self) -> String {
        fs::read_to_string(self.apps().join("demo-ui/build/index.html"))
            .map(|s| s.trim_end().to_owned())
            .unwrap_or_default()
    }

    fn log_text(&self) -> String {
        fs::read_to_string(self.stub_log()).unwrap_or_default()
    }

    fn count(&self, word: &str) -> usize {
        let p = format!("{word} ");
        self.log_text()
            .lines()
            .filter(|l| l.starts_with(&p))
            .count()
    }

    fn snap(&self) -> [usize; 3] {
        [
            self.count("PULL"),
            self.count("CREATE"),
            self.count("RESTART"),
        ]
    }

    /// "pulls creates restarts" since `snap`.
    fn delta(&self, s: [usize; 3]) -> String {
        let n = self.snap();
        format!("{} {} {}", n[0] - s[0], n[1] - s[1], n[2] - s[2])
    }
}

fn git(dir: &Path, args: &[&str]) {
    let st = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdout(Stdio::null())
        .status()
        .expect("git");
    assert!(st.success(), "git {args:?} in {}", dir.display());
}

fn newrepo(dir: &Path) {
    fs::create_dir_all(dir).expect("mkdir repo");
    git(dir, &["init", "-q", "-b", "master"]);
    for (k, v) in [
        ("user.email", "t@t"),
        ("user.name", "t"),
        ("commit.gpgsign", "false"),
        ("tag.gpgsign", "false"),
    ] {
        git(dir, &["config", k, v]);
    }
}

fn save(dir: &Path, tag: &str) {
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "--allow-empty", "-m", tag]);
    git(dir, &["tag", tag]);
}

fn put(path: &Path, text: &str) {
    if let Some(p) = path.parent() {
        fs::create_dir_all(p).expect("mkdir");
    }
    fs::write(path, text).expect("write");
}

fn append(path: &Path, text: &str) {
    let mut s = fs::read_to_string(path).unwrap_or_default();
    s.push_str(text);
    fs::write(path, s).expect("append");
}

/// `vars <image-version> [extra lines...]`.
fn vars(app: &Path, v: &str, extra: &[&str]) {
    let mut s = format!("IMAGE=reg.example/demo-ui-base:{v}\nWATCH_PATHS=\"build\"\n");
    for l in extra {
        s.push_str(l);
        s.push('\n');
    }
    put(&app.join(".deployment/release.vars"), &s);
}

struct Results {
    checks: HashMap<String, bool>,
    transcript: String,
}

struct Recorder {
    checks: HashMap<String, bool>,
}

impl Recorder {
    fn check(&mut self, w: &mut World, key: &str, ok: bool) {
        w.transcript
            .push_str(&format!("  {} {key}\n", if ok { "ok  " } else { "FAIL" }));
        self.checks.insert(key.to_owned(), ok);
    }
}

#[allow(clippy::too_many_lines)]
fn scenario(w: &mut World, r: &mut Recorder) {
    let t = w.t.clone();
    let app = t.join("remote/demo-ui");
    let apps = w.apps();
    let base = w.base();
    newrepo(&app);
    put(
        &app.join(".deployment/compose.yml"),
        "services:\n  ui:\n    image: ${IMAGE}\n",
    );
    put(&app.join("build/index.html"), "one\n");
    put(&app.join("build/healthz"), "ok\n");
    put(&app.join("README.md"), "hello\n");
    vars(&app, "v1.0.0", &[]);
    w.write_config(&["demo-ui"], &[], 5);

    // 1. empty repo, then first release
    git(&app, &["add", "-A"]);
    git(&app, &["commit", "-q", "-m", "init"]);
    let out = w.agent();
    let ok = out.contains("no tag v* yet") && w.deployed("demo-ui").is_empty();
    r.check(w, "1 no tag yet => nothing deployed", ok);
    let s = w.snap();
    save(&app, "v0.1.0");
    w.agent();
    let ok = w.deployed("demo-ui") == "v0.1.0";
    r.check(w, "1 state says v0.1.0", ok);
    let ok = w.code() == "one";
    r.check(w, "1 code synced to APPS_DIR", ok);
    let ok = !apps.join("demo-ui/.git").exists();
    r.check(w, "1 no .git inside the app folder", ok);
    let ok = w.delta(s) == "1 1 0";
    r.check(w, "1 pulled and created once, no restart", ok);

    // 2. nothing new => nothing happens
    let s = w.snap();
    let out = w.agent();
    let ok = w.delta(s) == "0 0 0" && out.contains("already deployed");
    r.check(w, "2 no docker action", ok);

    // 3. code-only change => restart, no pull, no recreate
    put(&app.join("build/index.html"), "two\n");
    put(&app.join("build/app.js"), "new\n");
    save(&app, "v0.2.0");
    let s = w.snap();
    w.agent();
    let ok = w.deployed("demo-ui") == "v0.2.0" && w.code() == "two";
    r.check(w, "3 state says v0.2.0", ok);
    let ok = w.delta(s) == "0 0 1";
    r.check(w, "3 one restart only", ok);

    // 4. a file removed in git is removed on the server
    git(&app, &["rm", "-q", "build/app.js"]);
    save(&app, "v0.3.0");
    w.agent();
    let ok = !apps.join("demo-ui/build/app.js").exists();
    r.check(w, "4 app.js is gone", ok);

    // 5. change outside WATCH_PATHS => synced, nothing restarted
    put(&app.join("README.md"), "changed\n");
    save(&app, "v0.4.0");
    let s = w.snap();
    w.agent();
    let ok = w.deployed("demo-ui") == "v0.4.0";
    r.check(w, "5 state says v0.4.0", ok);
    let ok = fs::read_to_string(apps.join("demo-ui/README.md")).unwrap_or_default() == "changed\n";
    r.check(w, "5 README synced", ok);
    let ok = w.delta(s) == "0 0 0";
    r.check(w, "5 no docker action", ok);

    // 6. image change => pull and recreate, no extra restart
    vars(&app, "v1.1.0", &[]);
    save(&app, "v0.5.0");
    let s = w.snap();
    w.agent();
    let ok = w.deployed("demo-ui") == "v0.5.0" && w.delta(s) == "1 1 0";
    r.check(w, "6 pulled and recreated", ok);

    // 7. image and code change together => new container, not restarted twice
    vars(&app, "v1.2.0", &[]);
    put(&app.join("build/index.html"), "three\n");
    save(&app, "v0.6.0");
    let s = w.snap();
    w.agent();
    let ok = w.deployed("demo-ui") == "v0.6.0" && w.delta(s) == "1 1 0";
    r.check(w, "7 pulled, recreated, no restart", ok);

    // 8. broken code => rolled back (code and container)
    git(&app, &["rm", "-q", "build/healthz"]);
    put(&app.join("build/index.html"), "broken\n");
    save(&app, "v0.7.0");
    let s = w.snap();
    let out = w.agent();
    let ok = w.deployed("demo-ui") == "v0.6.0";
    r.check(w, "8 still on v0.6.0", ok);
    let ok = w.code() == "three" && apps.join("demo-ui/build/healthz").is_file();
    r.check(w, "8 old code is back", ok);
    let ok = w.delta(s) == "0 0 2";
    r.check(w, "8 restarted for the release and back", ok);
    let ok = out.contains("ROLLED BACK to v0.6.0");
    r.check(w, "8 log shows rollback", ok);
    let ok =
        fs::read_to_string(base.join("state/demo-ui.failed")).unwrap_or_default() == "v0.7.0\n";
    r.check(w, "8 failed tag recorded", ok);
    let s = w.snap();
    let out = w.agent();
    let ok = out.contains("failed earlier") && w.delta(s) == "0 0 0";
    r.check(w, "8 failed tag is not retried", ok);

    // 9. bad image name => running stack and code untouched
    put(&app.join("build/healthz"), "ok\n");
    put(&app.join("build/index.html"), "four\n");
    vars(&app, ":v1.3.0", &[]);
    save(&app, "v0.8.0");
    let s = w.snap();
    let out = w.agent();
    let ok = w.deployed("demo-ui") == "v0.6.0" && w.code() == "three";
    r.check(w, "9 still on v0.6.0 with old code", ok);
    let ok = w.delta(s) == "0 0 0";
    r.check(w, "9 no docker change at all", ok);
    let ok = out.contains("nothing was changed on this server; v0.6.0 keeps running");
    r.check(w, "9 log says nothing was changed", ok);

    // 10. a missing WATCH_PATHS folder is refused before anything changes
    vars(&app, "v1.2.0", &[]);
    append(
        &app.join(".deployment/release.vars"),
        "WATCH_PATHS=\"dist\"\n",
    );
    save(&app, "v0.9.0");
    let s = w.snap();
    w.agent();
    let ok = w.deployed("demo-ui") == "v0.6.0" && w.code() == "three" && w.delta(s) == "0 0 0";
    r.check(w, "10 still on v0.6.0, untouched", ok);

    // 11. bad new image => rolled back to the old image
    vars(&app, "v1.4.0", &["FAIL=1"]);
    save(&app, "v0.10.0");
    w.agent();
    let ok = w.deployed("demo-ui") == "v0.6.0";
    r.check(w, "11 still on v0.6.0", ok);
    let container = fs::read_to_string(t.join("stubstate/demo-ui.container")).unwrap_or_default();
    let last_up = w
        .log_text()
        .lines()
        .rfind(|l| l.starts_with("UP "))
        .map(str::to_owned)
        .unwrap_or_default();
    let ok = container.starts_with("demo-ui-") && last_up.ends_with(":v1.2.0");
    r.check(w, "11 old image running again", ok);

    // 12. a good newer tag recovers; version order is numeric
    vars(&app, "v1.2.0", &[]);
    save(&app, "v0.11.0");
    w.agent();
    let ok = w.deployed("demo-ui") == "v0.11.0" && w.code() == "four";
    r.check(w, "12 state says v0.11.0 (above v0.9.0)", ok);
    let ok = !base.join("state/demo-ui.failed").exists();
    r.check(w, "12 failed marker cleared", ok);

    // 13. redeploy the same tag by hand: restores edited files, no pull, no recreate
    put(&apps.join("demo-ui/build/index.html"), "hacked\n");
    fs::remove_file(apps.join("demo-ui/README.md")).expect("rm README");
    let s = w.snap();
    w.deploy(&["demo-ui", "v0.11.0"]);
    let ok = w.code() == "four" && apps.join("demo-ui/README.md").is_file();
    r.check(w, "13 files restored", ok);
    let ok = w.delta(s) == "0 0 0";
    r.check(w, "13 no docker change", ok);

    // 14. --restart restarts without pulling
    let p = w.count("PULL");
    w.deploy(&["demo-ui", "v0.11.0", "--restart"]);
    let ok = w.log_text().lines().any(|l| l.ends_with(" restart")) && w.count("PULL") == p;
    r.check(w, "14 compose restart called, no pull", ok);

    // 15. a deleted old tag does not stop a rollback
    git(&app, &["tag", "-d", "v0.11.0"]);
    git(&app, &["rm", "-q", "build/healthz"]);
    save(&app, "v0.12.0");
    w.agent();
    let ok = w.deployed("demo-ui") == "v0.11.0" && apps.join("demo-ui/build/healthz").is_file();
    r.check(w, "15 rolled back to the removed tag", ok);
    put(&app.join("build/healthz"), "ok\n");
    save(&app, "v0.13.0");
    w.agent();

    // 16. hostile or malformed tags and names are ignored
    for tag in ["v9.9;touch-pwned", "vfoo", "v1.0.0-rc1"] {
        git(&app, &["tag", tag]);
    }
    w.agent();
    let ok = w.deployed("demo-ui") == "v0.13.0";
    r.check(w, "16 still on v0.13.0", ok);
    let rc = w.deploy(&["demo-ui", "v1.0;id"]);
    r.check(w, "16 bad tag rejected (64)", rc == 64);
    let rc = w.deploy(&["../x", "v1"]);
    r.check(w, "16 bad name rejected (64)", rc == 64);
    let rc = w.deploy(&["demo-ui", "v99.0.0"]);
    let ok = rc == 64 && w.deployed("demo-ui") == "v0.13.0";
    r.check(w, "16 unknown tag rejected (64)", ok);

    // 17. a second deploy while one runs is refused
    let lock_file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(base.join("locks/demo-ui.lock"))
        .expect("open lock");
    let held = nix::fcntl::Flock::lock(lock_file, nix::fcntl::FlockArg::LockExclusiveNonblock)
        .map_err(|(_, e)| e)
        .expect("flock");
    let rc = w.deploy(&["demo-ui", "v0.13.0"]);
    drop(held);
    r.check(w, "17 busy => exit 2", rc == 2);

    // 18. release dirs are pruned
    w.write_config(&["demo-ui"], &[], 2);
    for v in ["v0.14.0", "v0.15.0", "v0.16.0"] {
        save(&app, v);
        w.agent();
    }
    let n = fs::read_dir(base.join("releases/demo-ui"))
        .map(|it| {
            it.filter_map(Result::ok)
                .filter(|e| e.file_name().to_string_lossy().starts_with('v') && e.path().is_dir())
                .count()
        })
        .unwrap_or(usize::MAX);
    let ok = n <= 3 && base.join("releases/demo-ui/v0.16.0").is_dir();
    r.check(w, "18 old releases removed", ok);

    // 19. one-shot job runs once per tag; a failure is not rolled back
    let job = t.join("remote/demo-job");
    newrepo(&job);
    put(
        &job.join(".deployment/compose.yml"),
        "services:\n  migrate:\n    image: ${IMAGE}\n",
    );
    put(
        &job.join(".deployment/release.vars"),
        "IMAGE=reg.example/job:v1\nJOB_SERVICE=migrate\n",
    );
    save(&job, "v0.1.0");
    w.write_config(&["demo-ui", "demo-job"], &[], 2);
    w.agent();
    w.agent();
    let ok = w.count("RUN") == 1 && w.deployed("demo-job") == "v0.1.0";
    r.check(w, "19 job ran exactly once", ok);
    append(&job.join(".deployment/release.vars"), "FAIL=1\n");
    save(&job, "v0.2.0");
    let out = w.agent();
    let ok = w.count("RUN") == 2 && w.deployed("demo-job") == "v0.1.0";
    r.check(w, "19 failed job recorded, not re-run", ok);
    r.check(
        w,
        "19 says it cannot be rolled back",
        out.contains("cannot be rolled back"),
    );

    // 20. a repo that cannot be read does not block the others
    w.write_config(&["no-such-repo", "demo-ui"], &[], 2);
    save(&app, "v0.17.0");
    w.agent();
    let ok = w.deployed("demo-ui") == "v0.17.0";
    r.check(w, "20 demo-ui still deployed", ok);

    // 21. shared service kept in the infra repo (apps/<name>, tags <name>-vX)
    let infra = t.join("remote/infra");
    newrepo(&infra);
    put(
        &infra.join("apps/cache/compose.yml"),
        "services:\n  cache:\n    image: ${IMAGE}\n",
    );
    put(
        &infra.join("apps/cache/release.vars"),
        "IMAGE=reg.example/cache:v1\n",
    );
    save(&infra, "cache-v0.1.0");
    w.write_config(&["demo-ui"], &["cache"], 2);
    w.agent();
    let ok = w.deployed("cache") == "cache-v0.1.0";
    r.check(w, "21 deployed from the infra repo", ok);
    let ok = !apps.join("cache").exists();
    r.check(w, "21 no code folder for it", ok);

    // 22. shared services deploy before app repos; the shared secrets file reaches compose
    put(&base.join("secrets/ppe.env"), "PPE_X=1\n");
    save(&app, "v0.18.0");
    put(
        &infra.join("apps/cache/release.vars"),
        "IMAGE=reg.example/cache:v2\n",
    );
    save(&infra, "cache-v0.2.0");
    fs::write(w.stub_log(), "").expect("truncate log");
    let out = w.agent();
    let first = out.find("deploying ").map(|i| {
        let rest = &out[i + "deploying ".len()..];
        let end = rest
            .find(|c: char| !(c.is_ascii_lowercase() || c == '-'))
            .unwrap_or(rest.len());
        format!("deploying {}", &rest[..end])
    });
    r.check(
        w,
        "22 cache deployed before demo-ui",
        first.as_deref() == Some("deploying cache"),
    );
    let log = w.log_text();
    let want = format!("--env-file {}", base.join("secrets/ppe.env").display());
    r.check(w, "22 ppe.env passed as env file", log.contains(&want));
    r.check(
        w,
        "22 shared project is srf-blp-<name>",
        log.contains("-p srf-blp-cache "),
    );
}

fn run_scenario() -> Results {
    let mut w = World::new();
    let mut r = Recorder {
        checks: HashMap::new(),
    };
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        scenario(&mut w, &mut r);
    }));
    if let Err(e) = res {
        let msg = e
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| e.downcast_ref::<&str>().map(|s| (*s).to_owned()))
            .unwrap_or_default();
        w.transcript.push_str(&format!("scenario aborted: {msg}\n"));
    }
    let passed = r.checks.values().filter(|v| **v).count();
    w.transcript.push_str(&format!(
        "passed: {passed}   failed: {}\n",
        r.checks.len() - passed
    ));
    Results {
        checks: r.checks,
        transcript: w.transcript.clone(),
    }
}

static RESULTS: OnceLock<Results> = OnceLock::new();

fn assert_check(key: &str) {
    let r = RESULTS.get_or_init(run_scenario);
    let tail: Vec<&str> = r.transcript.lines().collect();
    let tail = tail[tail.len().saturating_sub(400)..].join("\n");
    match r.checks.get(key) {
        Some(true) => {}
        Some(false) => panic!("check '{key}' failed; transcript:\n{tail}"),
        None => panic!("check '{key}' was not reached; transcript:\n{tail}"),
    }
}

macro_rules! checks {
    ($($name:ident => $key:literal,)*) => {
        $(
            #[test]
            fn $name() {
                assert_check($key);
            }
        )*

        #[test]
        fn all_47_checks_are_ported() {
            let keys = [$($key),*];
            assert_eq!(keys.len(), 47);
            let r = RESULTS.get_or_init(run_scenario);
            for k in keys {
                assert!(r.checks.contains_key(k), "not recorded: {k}");
            }
            assert_eq!(r.checks.len(), 47, "every recorded check has a test");
        }
    };
}

checks! {
    s01_no_tag_yet_nothing_deployed => "1 no tag yet => nothing deployed",
    s01_state_says_v0_1_0 => "1 state says v0.1.0",
    s01_code_synced_to_apps_dir => "1 code synced to APPS_DIR",
    s01_no_git_inside_the_app_folder => "1 no .git inside the app folder",
    s01_pulled_and_created_once_no_restart => "1 pulled and created once, no restart",
    s02_no_docker_action => "2 no docker action",
    s03_state_says_v0_2_0 => "3 state says v0.2.0",
    s03_one_restart_only => "3 one restart only",
    s04_app_js_is_gone => "4 app.js is gone",
    s05_state_says_v0_4_0 => "5 state says v0.4.0",
    s05_readme_synced => "5 README synced",
    s05_no_docker_action => "5 no docker action",
    s06_pulled_and_recreated => "6 pulled and recreated",
    s07_pulled_recreated_no_restart => "7 pulled, recreated, no restart",
    s08_still_on_v0_6_0 => "8 still on v0.6.0",
    s08_old_code_is_back => "8 old code is back",
    s08_restarted_for_the_release_and_back => "8 restarted for the release and back",
    s08_log_shows_rollback => "8 log shows rollback",
    s08_failed_tag_recorded => "8 failed tag recorded",
    s08_failed_tag_is_not_retried => "8 failed tag is not retried",
    s09_still_on_v0_6_0_with_old_code => "9 still on v0.6.0 with old code",
    s09_no_docker_change_at_all => "9 no docker change at all",
    s09_log_says_nothing_was_changed => "9 log says nothing was changed",
    s10_still_on_v0_6_0_untouched => "10 still on v0.6.0, untouched",
    s11_still_on_v0_6_0 => "11 still on v0.6.0",
    s11_old_image_running_again => "11 old image running again",
    s12_state_says_v0_11_0_above_v0_9_0 => "12 state says v0.11.0 (above v0.9.0)",
    s12_failed_marker_cleared => "12 failed marker cleared",
    s13_files_restored => "13 files restored",
    s13_no_docker_change => "13 no docker change",
    s14_compose_restart_called_no_pull => "14 compose restart called, no pull",
    s15_rolled_back_to_the_removed_tag => "15 rolled back to the removed tag",
    s16_still_on_v0_13_0 => "16 still on v0.13.0",
    s16_bad_tag_rejected_64 => "16 bad tag rejected (64)",
    s16_bad_name_rejected_64 => "16 bad name rejected (64)",
    s16_unknown_tag_rejected_64 => "16 unknown tag rejected (64)",
    s17_busy_exit_2 => "17 busy => exit 2",
    s18_old_releases_removed => "18 old releases removed",
    s19_job_ran_exactly_once => "19 job ran exactly once",
    s19_failed_job_recorded_not_re_run => "19 failed job recorded, not re-run",
    s19_says_it_cannot_be_rolled_back => "19 says it cannot be rolled back",
    s20_demo_ui_still_deployed => "20 demo-ui still deployed",
    s21_deployed_from_the_infra_repo => "21 deployed from the infra repo",
    s21_no_code_folder_for_it => "21 no code folder for it",
    s22_cache_deployed_before_demo_ui => "22 cache deployed before demo-ui",
    s22_ppe_env_passed_as_env_file => "22 ppe.env passed as env file",
    s22_shared_project_is_srf_blp_name => "22 shared project is srf-blp-<name>",
}

/// Not in run-tests.sh: the long-running loop reconciles at start and stops cleanly on
/// SIGTERM (finishes the current step, then exits 0).
#[test]
fn agent_run_stops_on_sigterm() {
    let mut w = World::new();
    w.write_config(&["demo-ui"], &[], 5);
    let mut child = w
        .bin()
        .args(["agent", "run", "--config"])
        .arg(w.config_path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn agent");
    std::thread::sleep(Duration::from_millis(1500));
    let pid = nix::unistd::Pid::from_raw(i32::try_from(child.id()).expect("pid"));
    nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGTERM).expect("kill");
    let started = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait().expect("wait") {
            break s;
        }
        if started.elapsed() > Duration::from_secs(10) {
            let _ = child.kill();
            panic!("agent did not stop within 10 s of SIGTERM");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let out = child.wait_with_output().expect("output");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    w.transcript.push_str(&text);
    assert!(status.success(), "exit status {status:?}\n{text}");
    assert!(text.contains("agent started"), "{text}");
    // The empty remote cannot be cloned: logged, the loop keeps going.
    assert!(text.contains("git clone of"), "{text}");
    assert!(text.contains("agent stopped"), "{text}");
}
