// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! End to end through the real binary: `fornax serve` + `fornax agent`, a `git push` with a
//! `.fornax/` pipeline, the agent checks out the commit with its job token, runs both jobs,
//! and the API shows a successful pipeline with logs (M2 acceptance).

use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const CI: &str = r#"kind: Pipeline
version: 1
on:
  push:
    branches: [main]
variables:
  GREETING: hello from fornax
jobs:
  build:
    runs-on: [linux]
    steps:
      - name: greet
        run: echo "$GREETING" && echo "sha=$FORNAX_SHA"
      - name: checkout is present
        run: cat README.md
  test:
    needs: [build]
    runs-on: [linux]
    steps:
      - run: test "$CI" = true && echo tested
"#;

struct Kill(Child);
impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn fornax(data: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_fornax"));
    c.arg("--data-dir").arg(data).env("RUST_LOG", "warn");
    c
}

fn ok(out: Output) -> String {
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn token(out: &str, prefix: &str) -> String {
    out.split_whitespace()
        .find(|w| w.starts_with(prefix))
        .unwrap_or_else(|| panic!("no {prefix} token in {out}"))
        .to_owned()
}

fn git(dir: &Path, args: &[&str]) {
    let st = Command::new("git")
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .args([
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@e",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .status()
        .expect("git");
    assert!(st.success(), "git {args:?}");
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port()
}

#[tokio::test(flavor = "multi_thread")]
async fn push_runs_pipeline_on_agent() {
    let tmp = tempfile::tempdir().expect("tmp");
    let data = tmp.path().join("data");
    let port = free_port();
    let base = format!("http://127.0.0.1:{port}");

    let alice = token(
        &ok(fornax(&data)
            .args(["admin", "create-user", "alice", "--admin"])
            .output()
            .expect("run")),
        "fnx_",
    );
    let runner = token(
        &ok(fornax(&data)
            .args(["admin", "create-runner", "r1", "--labels", "linux"])
            .output()
            .expect("run")),
        "fnr_",
    );

    let _server = Kill(
        fornax(&data)
            .args([
                "serve",
                "--listen",
                &format!("127.0.0.1:{port}"),
                "--public-url",
                &base,
            ])
            .stdout(Stdio::null())
            .spawn()
            .expect("serve"),
    );
    let http = reqwest::Client::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    while http
        .get(format!("{base}/api/v1/health"))
        .send()
        .await
        .is_err()
    {
        assert!(Instant::now() < deadline, "server did not start");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let res = http
        .post(format!("{base}/api/v1/user/repos"))
        .bearer_auth(&alice)
        .json(&json!({ "name": "app" }))
        .send()
        .await
        .expect("create repo");
    assert_eq!(res.status(), 201);

    let work = tmp.path().join("agent-work");
    std::fs::create_dir_all(&work).expect("mkdir");
    let _agent = Kill(
        fornax(&data)
            .args(["agent", "--server", &base, "--work-dir"])
            .arg(&work)
            .env("FORNAX_RUNNER_TOKEN", &runner)
            .stdout(Stdio::null())
            .spawn()
            .expect("agent"),
    );

    let src = tmp.path().join("src");
    std::fs::create_dir_all(src.join(".fornax")).expect("mkdir");
    std::fs::write(src.join(".fornax/ci.yaml"), CI).expect("write");
    std::fs::write(src.join("README.md"), "checked out by the agent\n").expect("write");
    git(&src, &["init", "-q"]);
    git(&src, &["add", "."]);
    git(&src, &["commit", "-q", "-m", "add pipeline"]);
    let url = format!("http://alice:{alice}@127.0.0.1:{port}/alice/app.git");
    git(&src, &["push", "-q", &url, "main"]);

    // Wait for the pipeline to finish.
    let deadline = Instant::now() + Duration::from_secs(90);
    let detail: Value = loop {
        let res = http
            .get(format!("{base}/api/v1/repos/alice/app/pipelines/1"))
            .bearer_auth(&alice)
            .send()
            .await
            .expect("get");
        if res.status() == 200 {
            let v: Value = res.json().await.expect("json");
            if matches!(
                v["status"].as_str(),
                Some("success" | "failed" | "canceled")
            ) {
                break v;
            }
        }
        assert!(Instant::now() < deadline, "pipeline did not finish in time");
        tokio::time::sleep(Duration::from_millis(250)).await;
    };
    assert_eq!(detail["status"], "success", "{detail:#}");
    let jobs = detail["jobs"].as_array().expect("jobs");
    assert_eq!(
        jobs.iter()
            .map(|j| j["status"].as_str().unwrap_or(""))
            .collect::<Vec<_>>(),
        ["success", "success"]
    );

    let log_of = |id: i64| {
        let http = http.clone();
        let (base, alice) = (base.clone(), alice.clone());
        async move {
            http.get(format!(
                "{base}/api/v1/repos/alice/app/pipelines/1/jobs/{id}/log"
            ))
            .bearer_auth(&alice)
            .send()
            .await
            .expect("log")
            .text()
            .await
            .expect("text")
        }
    };
    let build = log_of(jobs[0]["id"].as_i64().expect("id")).await;
    let sha = String::from_utf8(
        Command::new("git")
            .current_dir(&src)
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("git")
            .stdout,
    )
    .expect("utf8");
    assert!(build.contains("hello from fornax"), "{build}");
    assert!(build.contains(&format!("sha={}", sha.trim())), "{build}");
    assert!(
        build.contains("checked out by the agent"),
        "checkout via job token: {build}"
    );
    assert!(!build.contains("fnj_"), "job token must be masked: {build}");
    assert!(
        log_of(jobs[1]["id"].as_i64().expect("id"))
            .await
            .contains("tested")
    );

    // The agent cleaned up its workspace.
    assert_eq!(std::fs::read_dir(&work).expect("read").count(), 0);
}
