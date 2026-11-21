// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! `on_push` against a real bare repository, real `git` pushes and an in-memory database.

use std::path::{Path, PathBuf};
use std::process::Command;

use fornax_ci::{JobPayload, MAX_REF_UPDATES, PushOutcome, RefUpdate, diff_refs, on_push};
use fornax_core::ci::{JobStatus, WaitingReason};
use fornax_db::{Db, NewRepo, Repo};
use fornax_git::RepoStore;

const NOW: i64 = 1_800_000_000;

/// Run git in `dir` with an isolated config; returns trimmed stdout.
fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

struct Fixture {
    _tmp: tempfile::TempDir,
    db: Db,
    store: RepoStore,
    repo: Repo,
    user_id: i64,
    bare: PathBuf,
    work: PathBuf,
}

impl Fixture {
    async fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db = Db::open_in_memory().await.expect("db");
        let user = db.create_user("alice", false).await.expect("user");
        let repo = db
            .create_repo(&NewRepo {
                owner_id: user.id,
                name: "demo",
                description: "",
                is_private: false,
                default_branch: "main",
            })
            .await
            .expect("repo");
        let store = RepoStore::new(tmp.path().join("repos"));
        let bare = store.create("alice", "demo", "main").await.expect("create");
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work).expect("mkdir");
        git(&work, &["init", "-q"]);
        Self {
            _tmp: tmp,
            db,
            store,
            repo,
            user_id: user.id,
            bare,
            work,
        }
    }

    /// Write files (relative path, content) and commit them; returns the commit ID.
    fn commit(&self, files: &[(&str, &str)]) -> String {
        for (path, content) in files {
            let full = self.work.join(path);
            std::fs::create_dir_all(full.parent().expect("parent")).expect("mkdir");
            std::fs::write(full, content).expect("write");
        }
        git(&self.work, &["add", "-A"]);
        git(
            &self.work,
            &["commit", "-q", "--allow-empty", "-m", "change"],
        );
        git(&self.work, &["rev-parse", "HEAD"])
    }

    fn checkout_new(&self, branch: &str) {
        git(&self.work, &["checkout", "-q", "-b", branch]);
    }

    /// `git push <bare> <refspecs>` bracketed by ref snapshots, then `on_push`.
    async fn push(&self, refspecs: &[&str]) -> (Vec<RefUpdate>, PushOutcome) {
        let before = self.store.snapshot_refs(&self.bare).await.expect("refs");
        let mut args = vec!["push", "-q", self.bare.to_str().expect("utf8")];
        args.extend_from_slice(refspecs);
        git(&self.work, &args);
        let after = self.store.snapshot_refs(&self.bare).await.expect("refs");
        let updates = diff_refs(&before, &after);
        let outcome = on_push(
            &self.db,
            &self.store,
            &self.repo,
            Some(self.user_id),
            &updates,
            NOW,
        )
        .await
        .expect("on_push");
        (updates, outcome)
    }

    async fn job_names(&self, pipeline_id: i64) -> Vec<String> {
        self.db
            .jobs_for_pipeline(pipeline_id)
            .await
            .expect("jobs")
            .into_iter()
            .map(|j| j.name)
            .collect()
    }
}

const CI: &str = "\
kind: Pipeline
version: 1
on:
  push: { branches: [main] }
variables: { MODE: ci, KEEP: yes }
jobs:
  build:
    runs-on: [linux, host]
    env: { MODE: build }
    timeout-minutes: 10
    steps:
      - name: compile
        run: make
        timeout-minutes: 5
        working-directory: src
  test:
    needs: [build]
    steps:
      - run: make test
        env: { X: \"1\" }
";

#[tokio::test]
async fn push_to_main_creates_pipeline_with_queued_and_waiting_jobs() {
    let f = Fixture::new().await;
    let sha = f.commit(&[(".fornax/ci.yaml", CI), ("README.md", "hi\n")]);
    let (updates, outcome) = f.push(&["main"]).await;
    assert_eq!(updates.len(), 1);
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert_eq!(outcome.created.len(), 1);

    let p = &outcome.created[0];
    assert_eq!(
        (
            p.file.as_str(),
            p.name.as_str(),
            p.git_ref.as_str(),
            p.event.as_str()
        ),
        (".fornax/ci.yaml", "ci", "refs/heads/main", "push")
    );
    assert_eq!(p.sha, sha);
    assert!(p.trusted);
    assert_eq!(p.triggered_by, Some(f.user_id));
    assert_eq!(p.number, 1);

    let jobs = f.db.jobs_for_pipeline(p.id).await.expect("jobs");
    assert_eq!(jobs.len(), 2);
    let (build, test) = (&jobs[0], &jobs[1]);
    assert_eq!(
        (build.name.as_str(), build.status),
        ("build", JobStatus::Queued)
    );
    assert_eq!(build.labels, ["host", "linux"]);
    assert_eq!(build.max_attempts, 1);
    assert!(build.trusted);
    assert_eq!(
        (test.name.as_str(), test.status, test.waiting_reason),
        ("test", JobStatus::Waiting, Some(WaitingReason::Dependency))
    );
    assert!(test.labels.is_empty());
    let needs = f.db.job_needs(p.id).await.expect("needs");
    assert_eq!(needs, vec![(test.id, build.id)]);

    let payload = JobPayload::from_json(&build.payload).expect("payload");
    assert_eq!(payload.v, 1);
    assert_eq!(payload.name, "build");
    assert_eq!(payload.sha, sha);
    assert_eq!(payload.git_ref, "refs/heads/main");
    assert_eq!(payload.repo_full_name, "alice/demo");
    assert_eq!(payload.timeout_secs, 600);
    assert_eq!(payload.env.get("MODE").map(String::as_str), Some("build"));
    assert_eq!(payload.env.get("KEEP").map(String::as_str), Some("yes"));
    assert_eq!(payload.steps.len(), 1);
    let step = &payload.steps[0];
    assert_eq!(
        (step.name.as_str(), step.run.as_str(), step.timeout_secs),
        ("compile", "make", Some(300))
    );
    assert_eq!(step.working_directory.as_deref(), Some("src"));
    assert_eq!(
        JobPayload::from_json(&payload.to_json().expect("encode")).expect("decode"),
        payload
    );

    let test_payload = JobPayload::from_json(&test.payload).expect("payload");
    assert_eq!(test_payload.timeout_secs, 3600);
    assert_eq!(test_payload.env.get("MODE").map(String::as_str), Some("ci"));
    assert_eq!(test_payload.steps[0].name, "step 1");
    assert_eq!(
        test_payload.steps[0].env.get("X").map(String::as_str),
        Some("1")
    );

    // A second push to main gets the next pipeline number.
    f.commit(&[("README.md", "hello\n")]);
    let (_, outcome) = f.push(&["main"]).await;
    assert_eq!(outcome.created.len(), 1);
    assert_eq!(outcome.created[0].number, 2);
}

#[tokio::test]
async fn branch_filter_excludes_other_branches() {
    let f = Fixture::new().await;
    f.commit(&[(".fornax/ci.yaml", CI)]);
    f.checkout_new("feature/x");
    let (updates, outcome) = f.push(&["feature/x"]).await;
    assert_eq!(updates[0].ref_name, "refs/heads/feature/x");
    assert!(outcome.created.is_empty());
    assert!(outcome.errors.is_empty());
    assert!(
        f.db.pipelines_for_repo(f.repo.id, 10)
            .await
            .expect("list")
            .is_empty()
    );
}

#[tokio::test]
async fn path_filter_uses_diff_for_updates_and_passes_for_new_branches() {
    let f = Fixture::new().await;
    let spec = "kind: Pipeline\nversion: 1\non:\n  push: { paths: [\"src/**\"] }\njobs:\n  build:\n    steps: [{ run: make }]\n";
    f.commit(&[(".fornax/ci.yaml", spec), ("README.md", "a\n")]);
    // New branch: no path information, the filter passes.
    let (_, outcome) = f.push(&["main"]).await;
    assert_eq!(outcome.created.len(), 1);

    // Update touching only README.md: filtered out.
    f.commit(&[("README.md", "b\n")]);
    let (updates, outcome) = f.push(&["main"]).await;
    assert!(updates[0].old.is_some());
    assert!(outcome.created.is_empty());
    assert!(outcome.errors.is_empty());

    // Update touching src/: triggers.
    f.commit(&[("src/lib.rs", "// x\n")]);
    let (_, outcome) = f.push(&["main"]).await;
    assert_eq!(outcome.created.len(), 1);

    // A new branch whose last commit only touched README.md still triggers.
    f.checkout_new("feature/y");
    f.commit(&[("README.md", "c\n")]);
    let (_, outcome) = f.push(&["feature/y"]).await;
    assert_eq!(outcome.created.len(), 1);
    assert_eq!(outcome.created[0].git_ref, "refs/heads/feature/y");
}

#[tokio::test]
async fn false_condition_drops_job_and_its_dependents() {
    let f = Fixture::new().await;
    let spec = "\
kind: Pipeline
version: 1
on: push
jobs:
  a:
    steps: [{ run: a }]
  b:
    if: ${{ branch == 'main' }}
    steps: [{ run: b }]
  c:
    needs: [b]
    steps: [{ run: c }]
  d:
    needs: [a]
    if: ${{ sha != '' && repo == 'alice/demo' }}
    steps: [{ run: d }]
  e:
    needs: [c, d]
    steps: [{ run: e }]
";
    let only_main = "\
kind: Pipeline
version: 1
on: push
jobs:
  deploy:
    if: ${{ ref == 'refs/heads/main' }}
    steps: [{ run: deploy }]
";
    f.commit(&[
        (".fornax/ci.yaml", spec),
        (".fornax/deploy.yaml", only_main),
    ]);
    let (_, outcome) = f.push(&["main"]).await;
    assert_eq!(outcome.created.len(), 2);
    assert_eq!(
        f.job_names(outcome.created[0].id).await,
        ["a", "b", "c", "d", "e"]
    );
    assert_eq!(f.job_names(outcome.created[1].id).await, ["deploy"]);

    f.checkout_new("feature/x");
    let (_, outcome) = f.push(&["feature/x"]).await;
    // deploy.yaml: every job left out → no pipeline at all.
    assert_eq!(outcome.created.len(), 1);
    assert!(outcome.errors.is_empty());
    let p = &outcome.created[0];
    assert_eq!(p.file, ".fornax/ci.yaml");
    // b is false; c needs b; e needs c (transitively dropped).
    assert_eq!(f.job_names(p.id).await, ["a", "d"]);
    let jobs = f.db.jobs_for_pipeline(p.id).await.expect("jobs");
    assert_eq!(jobs[0].status, JobStatus::Queued);
    assert_eq!(jobs[1].status, JobStatus::Waiting);
}

#[tokio::test]
async fn invalid_spec_reports_diagnostics_and_creates_nothing() {
    let f = Fixture::new().await;
    let bad = "kind: Pipeline\nversion: 1\non: push\njobs:\n  build:\n    stepz: []\n";
    let good = "kind: Pipeline\nversion: 1\non: push\njobs:\n  ok:\n    steps: [{ run: ok }]\n";
    f.commit(&[(".fornax/bad.yaml", bad), (".fornax/good.yaml", good)]);
    let (_, outcome) = f.push(&["main"]).await;
    assert!(outcome.created.is_empty());
    assert_eq!(outcome.errors.len(), 1);
    let (ref_name, diags) = &outcome.errors[0];
    assert_eq!(ref_name, "refs/heads/main");
    assert!(!diags.is_empty());
    assert!(
        diags
            .iter()
            .all(|d| d.to_string().starts_with(".fornax/bad.yaml:")),
        "{diags:?}"
    );
    assert!(
        diags
            .iter()
            .any(|d| d.line == 6 && d.message.contains("stepz")),
        "{diags:?}"
    );

    // A non-UTF-8 file is reported with its path, too.
    std::fs::remove_file(f.work.join(".fornax/bad.yaml")).expect("rm");
    std::fs::write(f.work.join(".fornax/binary.yml"), [0xff_u8, 0xfe, b'\n']).expect("write");
    f.commit(&[]);
    let (_, outcome) = f.push(&["main"]).await;
    assert!(outcome.created.is_empty());
    let (_, diags) = &outcome.errors[0];
    assert_eq!(diags.len(), 1);
    assert_eq!(diags[0].file, ".fornax/binary.yml");
    assert!(diags[0].message.contains("UTF-8"));

    // Fixed: the good file runs again.
    std::fs::remove_file(f.work.join(".fornax/binary.yml")).expect("rm");
    f.commit(&[]);
    let (_, outcome) = f.push(&["main"]).await;
    assert!(outcome.errors.is_empty());
    assert_eq!(outcome.created.len(), 1);
}

#[tokio::test]
async fn several_files_and_tag_pushes() {
    let f = Fixture::new().await;
    let on_main = |name: &str| {
        format!(
            "kind: Pipeline\nversion: 1\nname: {name}\non:\n  push: {{ branches: [main] }}\njobs:\n  j:\n    steps: [{{ run: x }}]\n"
        )
    };
    let lint = on_main("lint");
    let build = on_main("build");
    let release = "kind: Pipeline\nversion: 1\non:\n  push: { tags: [\"v*\"] }\njobs:\n  publish:\n    if: ${{ tag == 'v1.0' }}\n    steps: [{ run: publish }]\n";
    let sha = f.commit(&[
        (".fornax/pipelines/lint.yaml", lint.as_str()),
        (".fornax/build.yml", build.as_str()),
        (".fornax/release.yaml", release),
        ("not-a-spec.yaml", "garbage: ["),
    ]);
    let (_, outcome) = f.push(&["main"]).await;
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    let names: Vec<_> = outcome.created.iter().map(|p| p.name.as_str()).collect();
    // Files are read in path order: .fornax/build.yml, .fornax/pipelines/lint.yaml.
    assert_eq!(names, ["build", "lint"]);

    // Annotated tag matching `v*`.
    git(&f.work, &["tag", "-a", "v1.0", "-m", "release"]);
    let (updates, outcome) = f.push(&["v1.0"]).await;
    assert_eq!(updates[0].ref_name, "refs/tags/v1.0");
    assert_eq!(outcome.created.len(), 1);
    let p = &outcome.created[0];
    assert_eq!(
        (p.name.as_str(), p.git_ref.as_str()),
        ("release", "refs/tags/v1.0")
    );
    assert_eq!(p.sha, sha);
    let jobs = f.db.jobs_for_pipeline(p.id).await.expect("jobs");
    let payload = JobPayload::from_json(&jobs[0].payload).expect("payload");
    assert_eq!(payload.git_ref, "refs/tags/v1.0");

    // A tag not matching `v*`, and one matching whose `if:` is false.
    git(&f.work, &["tag", "nightly"]);
    git(&f.work, &["tag", "v2.0"]);
    let (updates, outcome) = f.push(&["nightly", "v2.0"]).await;
    assert_eq!(updates.len(), 2);
    assert!(outcome.created.is_empty());
    assert!(outcome.errors.is_empty());
}

#[tokio::test]
async fn deleted_refs_and_no_spec_do_nothing() {
    let f = Fixture::new().await;
    let spec = "kind: Pipeline\nversion: 1\non: push\njobs:\n  j:\n    steps: [{ run: x }]\n";
    f.commit(&[("README.md", "no spec yet\n")]);
    let (_, outcome) = f.push(&["main"]).await;
    assert!(outcome.created.is_empty() && outcome.errors.is_empty());

    f.commit(&[(".fornax/ci.yaml", spec)]);
    let (_, outcome) = f.push(&["main", "main:refs/heads/doomed"]).await;
    assert_eq!(outcome.created.len(), 2);

    let (updates, outcome) = f.push(&[":refs/heads/doomed"]).await;
    assert_eq!(updates.len(), 1);
    assert!(updates[0].is_delete());
    assert!(outcome.created.is_empty() && outcome.errors.is_empty());

    // Refs outside refs/heads and refs/tags are ignored too.
    let other = RefUpdate {
        ref_name: "refs/notes/commits".into(),
        old: None,
        new: updates[0].old.clone(),
    };
    let outcome = on_push(&f.db, &f.store, &f.repo, None, &[other], NOW)
        .await
        .expect("on_push");
    assert!(outcome.created.is_empty());
}

#[tokio::test]
async fn ref_update_limit() {
    let f = Fixture::new().await;
    let spec = "kind: Pipeline\nversion: 1\non: push\njobs:\n  j:\n    steps: [{ run: x }]\n";
    f.commit(&[(".fornax/ci.yaml", spec)]);
    let refspecs: Vec<String> = (0..=MAX_REF_UPDATES)
        .map(|i| format!("HEAD:refs/heads/b{i:02}"))
        .collect();
    let refspecs: Vec<&str> = refspecs.iter().map(String::as_str).collect();
    let (updates, outcome) = f.push(&refspecs).await;
    assert_eq!(updates.len(), MAX_REF_UPDATES + 1);
    assert_eq!(outcome.created.len(), MAX_REF_UPDATES);
    assert_eq!(
        outcome.skipped_refs,
        [format!("refs/heads/b{MAX_REF_UPDATES:02}")]
    );
}

#[tokio::test]
async fn oversized_job_payload_is_reported_and_creates_nothing() {
    let f = Fixture::new().await;
    // One 300 KB script: the file is fine, the encoded job payload is not.
    let spec = format!(
        "kind: Pipeline\nversion: 1\non: push\njobs:\n  big:\n    steps:\n      - run: \"{}\"\n",
        "x".repeat(300 * 1024)
    );
    f.commit(&[(".fornax/ci.yaml", &spec)]);
    let (_, outcome) = f.push(&["main"]).await;
    assert!(outcome.created.is_empty());
    let (_, diags) = &outcome.errors[0];
    assert!(
        diags.iter().any(|d| d.message.contains("payload")),
        "{diags:?}"
    );
}

#[tokio::test]
async fn jobs_per_push_limit() {
    let f = Fixture::new().await;
    let vals: Vec<String> = (0..256).map(|i| i.to_string()).collect();
    let spec = format!(
        "kind: Pipeline\nversion: 1\non: push\njobs:\n  b:\n    strategy:\n      matrix:\n        n: [{}]\n    steps: [{{ run: x }}]\n",
        vals.join(", ")
    );
    let files: Vec<(String, String)> = (0..4)
        .map(|i| (format!(".fornax/p{i}.yaml"), spec.clone()))
        .collect();
    let refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(p, s)| (p.as_str(), s.as_str()))
        .collect();
    f.commit(&refs);
    let (_, outcome) = f.push(&["main"]).await;
    // 3 × 256 = 768 jobs fit; a 4th pipeline would make 1024 > 1000.
    assert_eq!(outcome.created.len(), 3);
    let (_, diags) = &outcome.errors[0];
    assert!(
        diags
            .iter()
            .any(|d| d.file == ".fornax/p3.yaml" && d.message.contains("1000 jobs")),
        "{diags:?}"
    );
}
