// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! HostExecutor integration tests (real processes, real git).
#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::time::{Duration, Instant};

use fornax_agent::{Executor, HostExecutor, LogSink, SinkEvent, job_mask_phrases};
use fornax_proto::runner::v1::{Job, JobResult, Step, StepState};
use tokio_util::sync::CancellationToken;

fn step(name: &str, run: &str) -> Step {
    Step {
        name: name.into(),
        run: run.into(),
        ..Default::default()
    }
}

fn job(steps: Vec<Step>) -> Job {
    Job {
        attempt_id: "att-1".into(),
        name: "build".into(),
        repo_full_name: "alice/demo".into(),
        sha: "0123456789abcdef0123456789abcdef01234567".into(),
        r#ref: "refs/heads/main".into(),
        steps,
        ..Default::default()
    }
}

struct Run {
    log: String,
    steps: Vec<(u32, StepState)>,
    result: JobResult,
    error: String,
}

async fn run_job(exec: &HostExecutor, job: &Job, cancel: CancellationToken) -> Run {
    let (sink, mut events) = LogSink::new(job_mask_phrases(job), 64);
    let collector = tokio::spawn(async move {
        let mut log = Vec::new();
        let mut steps = Vec::new();
        while let Some(e) = events.recv().await {
            match e {
                SinkEvent::Log { data, .. } => log.extend(data),
                SinkEvent::Step(u) => steps.push((u.index, u.state)),
            }
        }
        (String::from_utf8_lossy(&log).into_owned(), steps)
    });
    let outcome = exec.run(job, sink, cancel).await;
    let (log, steps) = collector.await.expect("collector");
    Run {
        log,
        steps,
        result: outcome.result,
        error: outcome.error,
    }
}

fn executor(dir: &Path) -> HostExecutor {
    HostExecutor::new(dir.join("work")).with_kill_grace(Duration::from_millis(500))
}

fn final_states(r: &Run) -> Vec<StepState> {
    let mut v: Vec<(u32, StepState)> = r
        .steps
        .iter()
        .filter(|(_, s)| *s != StepState::Running)
        .copied()
        .collect();
    v.sort_by_key(|(i, _)| *i);
    v.into_iter().map(|(_, s)| s).collect()
}

#[tokio::test]
async fn steps_run_in_order_with_env_and_workspace_is_removed() {
    let tmp = tempfile::tempdir().unwrap();
    let exec = executor(tmp.path());
    let mut j = job(vec![
        step("one", "echo first > f; echo \"A=$A B=$B CI=$CI\""),
        step(
            "two",
            "cat f; echo \"sha=$FORNAX_SHA ref=$FORNAX_REF repo=$FORNAX_REPO job=$FORNAX_JOB\"; test \"$PWD\" = \"$FORNAX_WORKSPACE\"",
        ),
    ]);
    j.env.insert("A".into(), "job".into());
    j.env.insert("B".into(), "job".into());
    j.steps[0].env.insert("B".into(), "step".into());
    let r = run_job(&exec, &j, CancellationToken::new()).await;
    assert_eq!(r.result, JobResult::Success, "log: {}", r.log);
    assert!(r.log.contains("$ one\nA=job B=step CI=true\n"), "{}", r.log);
    let first = r.log.find("$ one").unwrap();
    let second = r.log.find("$ two").unwrap();
    assert!(first < second);
    assert!(r.log.contains("first\n"));
    assert!(r.log.contains(
        "sha=0123456789abcdef0123456789abcdef01234567 ref=refs/heads/main repo=alice/demo job=build"
    ));
    assert_eq!(
        final_states(&r),
        vec![StepState::Success, StepState::Success]
    );
    assert!(!exec.workspace_for("att-1").exists());
}

#[tokio::test]
async fn failure_skips_later_steps_and_pipefail_applies() {
    let tmp = tempfile::tempdir().unwrap();
    let exec = executor(tmp.path());
    let j = job(vec![
        step("ok", "true"),
        step("bad", "false | cat; echo unreachable"),
        step("later", "echo never"),
    ]);
    let r = run_job(&exec, &j, CancellationToken::new()).await;
    assert_eq!(r.result, JobResult::Failure);
    assert!(!r.log.contains("unreachable"));
    assert!(!r.log.contains("never"));
    assert_eq!(
        final_states(&r),
        vec![StepState::Success, StepState::Failure, StepState::Skipped]
    );
    assert!(!exec.workspace_for("att-1").exists());
}

#[tokio::test]
async fn working_directory_escape_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let exec = executor(tmp.path());
    let mut j = job(vec![
        step("mk", "mkdir sub"),
        step("esc", "touch escaped"),
        step("x", "true"),
    ]);
    j.steps[1].working_directory = "../".into();
    let r = run_job(&exec, &j, CancellationToken::new()).await;
    assert_eq!(r.result, JobResult::Failure);
    assert!(r.error.contains("working_directory"), "{}", r.error);
    assert!(!tmp.path().join("work/escaped").exists());
    assert_eq!(
        final_states(&r),
        vec![StepState::Success, StepState::Failure, StepState::Skipped]
    );

    // A valid sub directory works.
    let mut j = job(vec![
        step("mk", "mkdir sub"),
        step("in", "basename \"$PWD\""),
    ]);
    j.steps[1].working_directory = "sub".into();
    let r = run_job(&exec, &j, CancellationToken::new()).await;
    assert_eq!(r.result, JobResult::Success, "{}", r.log);
    assert!(r.log.contains("sub\n"));
}

fn alive(pid: i32) -> bool {
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok()
}

async fn wait_dead(pid: i32) -> bool {
    for _ in 0..100 {
        if !alive(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

#[tokio::test]
async fn step_timeout_kills_process_group_including_grandchildren() {
    let tmp = tempfile::tempdir().unwrap();
    let exec = executor(tmp.path());
    let pidfile = tmp.path().join("bg.pid");
    let mut j = job(vec![
        step(
            "slow",
            &format!(
                "bash -c 'sleep 30 & echo $! > {}; sleep 30'",
                pidfile.display()
            ),
        ),
        step("after", "echo never"),
    ]);
    j.steps[0].timeout_secs = 1;
    let start = Instant::now();
    let r = run_job(&exec, &j, CancellationToken::new()).await;
    assert!(
        start.elapsed() < Duration::from_secs(10),
        "took {:?}",
        start.elapsed()
    );
    assert_eq!(r.result, JobResult::Failure);
    assert!(r.log.contains("step timed out"), "{}", r.log);
    assert_eq!(
        final_states(&r),
        vec![StepState::Failure, StepState::Skipped]
    );
    let pid: i32 = std::fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(wait_dead(pid).await, "grandchild {pid} still alive");
}

#[tokio::test]
async fn job_timeout_applies() {
    let tmp = tempfile::tempdir().unwrap();
    let exec = executor(tmp.path());
    let mut j = job(vec![step("slow", "sleep 30")]);
    j.timeout_secs = 1;
    let start = Instant::now();
    let r = run_job(&exec, &j, CancellationToken::new()).await;
    assert!(start.elapsed() < Duration::from_secs(10));
    assert_eq!(r.result, JobResult::Failure);
    assert!(r.log.contains("job timed out"), "{}", r.log);
}

#[tokio::test]
async fn cancel_stops_job_and_cleans_up() {
    let tmp = tempfile::tempdir().unwrap();
    let exec = executor(tmp.path());
    let pidfile = tmp.path().join("c.pid");
    let j = job(vec![
        step(
            "wait",
            &format!("sleep 30 & echo $! > {}; wait", pidfile.display()),
        ),
        step("after", "echo never"),
    ]);
    let cancel = CancellationToken::new();
    let c = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(700)).await;
        c.cancel();
    });
    let start = Instant::now();
    let r = run_job(&exec, &j, cancel).await;
    assert!(start.elapsed() < Duration::from_secs(10));
    assert_eq!(r.result, JobResult::Canceled);
    assert_eq!(
        final_states(&r),
        vec![StepState::Failure, StepState::Skipped]
    );
    let pid: i32 = std::fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(wait_dead(pid).await);
    assert!(!exec.workspace_for("att-1").exists());
}

#[tokio::test]
async fn secrets_are_step_env_and_masked() {
    let tmp = tempfile::tempdir().unwrap();
    let exec = executor(tmp.path());
    let mut j = job(vec![step(
        "leak",
        // Print the secret in pieces so it crosses write boundaries, then whole, and to stderr.
        "printf 'x=%s' \"${TOKEN:0:3}\"; sleep 0.2; printf '%s\\n' \"${TOKEN:3}\"; echo \"$TOKEN\" >&2; echo \"plain=$PLAIN\"",
    )]);
    j.secrets.insert("TOKEN".into(), "sup3rs3cret".into());
    j.mask.push("visible-but-masked".into());
    j.env.insert("PLAIN".into(), "visible-but-masked".into());
    let r = run_job(&exec, &j, CancellationToken::new()).await;
    assert_eq!(r.result, JobResult::Success, "{}", r.log);
    assert!(!r.log.contains("sup3rs3cret"), "{}", r.log);
    assert!(!r.log.contains("sup"), "{}", r.log);
    assert!(r.log.contains("x=[MASKED]\n"), "{}", r.log);
    assert!(r.log.contains("plain=[MASKED]\n"), "{}", r.log);
}

#[tokio::test]
async fn runner_environment_is_not_inherited() {
    // The agent process may hold its runner token in its env; steps must not see it.
    let tmp = tempfile::tempdir().unwrap();
    let exec = executor(tmp.path());
    let j = job(vec![step("env", "env | sort")]);
    let r = run_job(&exec, &j, CancellationToken::new()).await;
    assert_eq!(r.result, JobResult::Success);
    assert!(!r.log.contains("CARGO_PKG_NAME"), "{}", r.log);
    assert!(r.log.contains("PATH="));
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

#[tokio::test]
async fn checkout_from_local_bare_repo() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    std::fs::create_dir(&src).unwrap();
    git(&src, &["init", "-q", "-b", "main"]);
    std::fs::write(src.join("hello.txt"), "from git\n").unwrap();
    git(&src, &["add", "."]);
    git(&src, &["commit", "-q", "-m", "c1"]);
    let sha = git(&src, &["rev-parse", "HEAD"]);
    let bare = tmp.path().join("bare.git");
    git(
        tmp.path(),
        &[
            "clone",
            "-q",
            "--bare",
            src.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    // Allow fetching an arbitrary reachable sha.
    git(&bare, &["config", "uploadpack.allowAnySHA1InWant", "true"]);

    let exec = executor(tmp.path());
    let mut j = job(vec![step("cat", "cat hello.txt; git rev-parse HEAD")]);
    j.clone_url = format!("file://{}", bare.display());
    j.sha = sha.clone();
    j.job_token = "jobtok123".into();
    let r = run_job(&exec, &j, CancellationToken::new()).await;
    assert_eq!(r.result, JobResult::Success, "{}", r.log);
    assert!(r.log.contains("from git\n"));
    assert!(r.log.contains(&sha));
    assert!(!r.log.contains("jobtok123"));

    // A bad sha fails checkout with ERROR and skips all steps.
    j.sha = "f".repeat(40);
    let r = run_job(&exec, &j, CancellationToken::new()).await;
    assert_eq!(r.result, JobResult::Error, "{}", r.log);
    assert_eq!(final_states(&r), vec![StepState::Skipped]);
    assert!(!exec.workspace_for("att-1").exists());
}
