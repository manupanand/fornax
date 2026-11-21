// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! The real Runner service (mounted on an axum router, h2c on localhost TCP) against the real
//! `fornax_agent::run` and against a raw gRPC client.

use std::time::Duration;

use fornax_core::ci::{JobStatus, PipelineStatus};
use fornax_core::now_unix;
use fornax_db::{Db, NewJob, NewPipeline, NewRepo, Pipeline, Repo};
use fornax_proto::runner::v1::runner_client::RunnerClient;
use fornax_proto::runner::v1::{
    DoneRequest, ExtendRequest, ExtendState, Job, JobResult, LogChunk, NextRequest,
    RegisterRequest, StepState, UpdateRequest,
};
use fornax_runner::{QueueNotifier, RunnerConfig, RunnerService, create_runner, hash_token};
use serde_json::json;
use tokio::task::JoinHandle;
use tonic::service::Interceptor;
use tonic::service::interceptor::InterceptedService;
use tonic::transport::Channel;
use tonic::{Code, Request, Status};

const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

struct Env {
    tmp: tempfile::TempDir,
    db: Db,
    svc: RunnerService,
    url: String,
    repo: Repo,
    user_id: i64,
}

async fn start_with(edit: impl FnOnce(&mut RunnerConfig)) -> Env {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db = Db::open(&tmp.path().join("fornax.db")).await.expect("db");
    let user = db.create_user("alice", false).await.expect("user");
    let repo = db
        .create_repo(&NewRepo {
            owner_id: user.id,
            name: "demo",
            description: "",
            is_private: true,
            default_branch: "main",
        })
        .await
        .expect("repo");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("addr"));
    let mut config = RunnerConfig::new(url.clone());
    config.checkout = false;
    edit(&mut config);
    let svc = RunnerService::new(db.clone(), config, QueueNotifier::new());
    let app = svc.clone().router();
    tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
    Env {
        tmp,
        db,
        svc,
        url,
        repo,
        user_id: user.id,
    }
}

async fn start() -> Env {
    start_with(|_| {}).await
}

fn payload(name: &str, steps: &[&str]) -> String {
    let steps: Vec<_> = steps
        .iter()
        .enumerate()
        .map(|(i, run)| json!({ "name": format!("step{i}"), "run": run }))
        .collect();
    json!({
        "v": 1,
        "name": name,
        "steps": steps,
        "env": { "GREETING": "hi" },
        "timeout_secs": 120,
        "repo_full_name": "alice/demo",
        "sha": SHA,
        "ref": "refs/heads/main",
    })
    .to_string()
}

fn job(name: &str, needs: &[&str], steps: &[&str]) -> NewJob {
    NewJob {
        name: name.into(),
        labels: vec!["linux".into()],
        payload: payload(name, steps),
        needs: needs.iter().map(|s| (*s).to_owned()).collect(),
        max_attempts: 1,
    }
}

impl Env {
    async fn runner(&self, name: &str) -> String {
        let (_, token) = create_runner(&self.db, name, &["linux"], true, now_unix())
            .await
            .expect("runner");
        token
    }

    /// Create a pipeline and wake waiting runners (what the push path does).
    async fn pipeline(&self, jobs: Vec<NewJob>) -> Pipeline {
        let p = self
            .db
            .create_pipeline(
                &NewPipeline {
                    repo_id: self.repo.id,
                    file: ".fornax/ci.yaml".into(),
                    name: "ci".into(),
                    git_ref: "refs/heads/main".into(),
                    sha: SHA.into(),
                    event: "push".into(),
                    trusted: true,
                    triggered_by: Some(self.user_id),
                    jobs,
                },
                now_unix(),
            )
            .await
            .expect("pipeline");
        self.svc.notifier().notify();
        p
    }

    async fn wait_pipeline(&self, id: i64) -> Pipeline {
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                let p = self.db.pipeline(id).await.expect("read").expect("exists");
                if p.status.is_terminal() {
                    return p;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .expect("pipeline finishes within 60 s")
    }

    async fn job_log(&self, pipeline_id: i64, name: &str) -> String {
        let jobs = self.db.jobs_for_pipeline(pipeline_id).await.expect("jobs");
        let job = jobs.iter().find(|j| j.name == name).expect("job");
        let attempts = self.db.attempts_for_job(job.id).await.expect("attempts");
        let attempt = attempts.first().expect("attempt");
        let (data, _) = self.db.read_log(attempt.id, 0, 1 << 20).await.expect("log");
        String::from_utf8_lossy(&data).into_owned()
    }

    fn agent(&self, token: &str, poll: Duration) -> Agent {
        let mut c = fornax_agent::AgentConfig::new(&self.url, token, self.tmp.path().join("work"));
        c.poll_timeout = poll;
        c.heartbeat_interval = Duration::from_millis(300);
        c.shutdown_timeout = Duration::from_secs(5);
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let handle = tokio::spawn(fornax_agent::run(c, async {
            let _ = rx.await;
        }));
        Agent { stop: tx, handle }
    }
}

struct Agent {
    stop: tokio::sync::oneshot::Sender<()>,
    handle: JoinHandle<Result<(), fornax_agent::AgentError>>,
}

impl Agent {
    async fn stop(self) {
        let _ = self.stop.send(());
        tokio::time::timeout(Duration::from_secs(15), self.handle)
            .await
            .expect("agent stops")
            .expect("join")
            .expect("agent ok");
    }
}

#[derive(Clone)]
struct Bearer(Option<String>);

impl Interceptor for Bearer {
    fn call(&mut self, mut req: Request<()>) -> Result<Request<()>, Status> {
        if let Some(t) = &self.0 {
            let v = format!("Bearer {t}").parse().expect("ascii");
            req.metadata_mut().insert("authorization", v);
        }
        Ok(req)
    }
}

type Client = RunnerClient<InterceptedService<Channel, Bearer>>;

async fn client(url: &str, token: Option<&str>) -> Client {
    let channel = Channel::from_shared(url.to_owned())
        .expect("url")
        .connect()
        .await
        .expect("connect");
    RunnerClient::with_interceptor(channel, Bearer(token.map(str::to_owned)))
}

async fn next(c: &mut Client, timeout: Duration) -> Result<Option<Job>, Status> {
    let mut req = Request::new(NextRequest { capacity_free: 1 });
    req.set_timeout(timeout);
    c.next(req).await.map(|r| r.into_inner().job)
}

fn chunk(attempt: &str, offset: u64, data: &[u8]) -> LogChunk {
    LogChunk {
        attempt_id: attempt.into(),
        offset,
        data: data.to_vec(),
        stream: 1,
    }
}

// ---- agent end to end -----------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn agent_runs_job_and_logs_are_stored() {
    let env = start().await;
    let token = env.runner("r1").await;
    let agent = env.agent(&token, Duration::from_secs(5));
    let p = env
        .pipeline(vec![job(
            "build",
            &[],
            &["echo hello from fornax; echo \"greeting=$GREETING\""],
        )])
        .await;
    let done = env.wait_pipeline(p.id).await;
    assert_eq!(done.status, PipelineStatus::Success);
    let log = env.job_log(p.id, "build").await;
    assert!(log.contains("hello from fornax"), "{log}");
    assert!(log.contains("greeting=hi"), "{log}");
    let jobs = env.db.jobs_for_pipeline(p.id).await.expect("jobs");
    assert_eq!(jobs[0].status, JobStatus::Success);
    let runners = env.db.list_runners().await.expect("runners");
    assert!(runners[0].last_seen_at.is_some());
    agent.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn failing_step_fails_pipeline() {
    let env = start().await;
    let token = env.runner("r1").await;
    let agent = env.agent(&token, Duration::from_secs(5));
    let p = env
        .pipeline(vec![job("build", &[], &["echo before-fail; exit 3"])])
        .await;
    let done = env.wait_pipeline(p.id).await;
    assert_eq!(done.status, PipelineStatus::Failed);
    let log = env.job_log(p.id, "build").await;
    assert!(log.contains("before-fail"), "{log}");
    let jobs = env.db.jobs_for_pipeline(p.id).await.expect("jobs");
    let attempts = env.db.attempts_for_job(jobs[0].id).await.expect("attempts");
    assert_eq!(attempts[0].exit_code, Some(3));
    agent.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn dependent_job_runs_after_its_need() {
    let env = start().await;
    let token = env.runner("r1").await;
    let agent = env.agent(&token, Duration::from_secs(5));
    let p = env
        .pipeline(vec![
            job("first", &[], &["echo first-ran"]),
            job("second", &["first"], &["echo second-ran"]),
        ])
        .await;
    let done = env.wait_pipeline(p.id).await;
    assert_eq!(done.status, PipelineStatus::Success);
    assert!(env.job_log(p.id, "first").await.contains("first-ran"));
    assert!(env.job_log(p.id, "second").await.contains("second-ran"));
    let jobs = env.db.jobs_for_pipeline(p.id).await.expect("jobs");
    let first = jobs.iter().find(|j| j.name == "first").expect("first");
    let second = jobs.iter().find(|j| j.name == "second").expect("second");
    let a1 = env.db.attempts_for_job(first.id).await.expect("a1");
    let a2 = env.db.attempts_for_job(second.id).await.expect("a2");
    assert!(a1[0].id < a2[0].id, "second claimed after first");
    assert!(a2[0].started_at >= a1[0].finished_at.expect("finished"));
    agent.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn long_poll_wakes_promptly_on_new_pipeline() {
    let env = start().await;
    let token = env.runner("r1").await;
    // Server holds Next for 18 s (20 s client deadline minus margin).
    let agent = env.agent(&token, Duration::from_secs(20));
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let started = tokio::time::Instant::now();
    let p = env.pipeline(vec![job("build", &[], &["echo woke"])]).await;
    let jobs = env.db.jobs_for_pipeline(p.id).await.expect("jobs");
    loop {
        let j = env.db.job(jobs[0].id).await.expect("job").expect("exists");
        if j.status != JobStatus::Queued {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "job not claimed within 2 s of creation"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(
        env.wait_pipeline(p.id).await.status,
        PipelineStatus::Success
    );
    agent.stop().await;
}

// ---- protocol checks with a raw client -------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn unauthenticated_calls_are_rejected() {
    let env = start().await;
    let token = env.runner("r1").await;
    let fast = Duration::from_secs(2);

    let mut anon = client(&env.url, None).await;
    let err = next(&mut anon, fast).await.expect_err("no token");
    assert_eq!(err.code(), Code::Unauthenticated);
    let err = anon
        .done(DoneRequest {
            attempt_id: "1".into(),
            ..Default::default()
        })
        .await
        .expect_err("no token");
    assert_eq!(err.code(), Code::Unauthenticated);

    let mut bad = client(&env.url, Some("fnr_nothex")).await;
    assert_eq!(
        next(&mut bad, fast).await.expect_err("bad").code(),
        Code::Unauthenticated
    );
    let unknown = format!("fnr_{}", "ab".repeat(32));
    let mut unknown = client(&env.url, Some(&unknown)).await;
    assert_eq!(
        next(&mut unknown, fast).await.expect_err("unknown").code(),
        Code::Unauthenticated
    );

    let mut ok = client(&env.url, Some(&token)).await;
    assert!(next(&mut ok, fast).await.expect("valid").is_none());

    let runner = env
        .db
        .runner_by_token_hash(&hash_token(&token))
        .await
        .expect("db")
        .expect("runner");
    env.db
        .set_runner_disabled(runner.id, true)
        .await
        .expect("disable");
    assert_eq!(
        next(&mut ok, fast).await.expect_err("disabled").code(),
        Code::Unauthenticated
    );

    // Self-registration is off without a configured registration token.
    let err = anon
        .register(RegisterRequest {
            registration_token: "anything".into(),
            name: "x".into(),
            ..Default::default()
        })
        .await
        .expect_err("disabled");
    assert_eq!(err.code(), Code::PermissionDenied);
}

#[tokio::test(flavor = "multi_thread")]
async fn register_with_configured_token() {
    let env = start_with(|c| c.registration_token = Some("reg-secret".into())).await;
    let mut anon = client(&env.url, None).await;
    let err = anon
        .register(RegisterRequest {
            registration_token: "wrong".into(),
            name: "r".into(),
            ..Default::default()
        })
        .await
        .expect_err("wrong token");
    assert_eq!(err.code(), Code::PermissionDenied);
    let err = anon
        .register(RegisterRequest {
            registration_token: "reg-secret".into(),
            name: "bad name".into(),
            ..Default::default()
        })
        .await
        .expect_err("bad name");
    assert_eq!(err.code(), Code::InvalidArgument);

    let resp = fornax_agent::register(&env.url, "reg-secret", "builder-1", &["linux".into()])
        .await
        .expect("register");
    assert!(fornax_proto::is_runner_token_format(&resp.runner_token));
    let runner = env
        .db
        .runner_by_token_hash(&hash_token(&resp.runner_token))
        .await
        .expect("db")
        .expect("stored");
    assert_eq!(runner.id.to_string(), resp.runner_id);
    assert!(
        runner.trusted_only,
        "self-registered runners are trusted-only"
    );
    assert_eq!(runner.labels, vec!["linux".to_string()]);
    let mut c = client(&env.url, Some(&resp.runner_token)).await;
    assert!(
        next(&mut c, Duration::from_secs(2))
            .await
            .expect("ok")
            .is_none()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn runner_cannot_use_another_runners_attempt() {
    let env = start_with(|c| c.checkout = true).await;
    let (ta, tb) = (env.runner("ra").await, env.runner("rb").await);
    let p = env.pipeline(vec![job("build", &[], &["true"])]).await;
    let mut a = client(&env.url, Some(&ta)).await;
    let mut b = client(&env.url, Some(&tb)).await;

    let job = next(&mut a, Duration::from_secs(5))
        .await
        .expect("next")
        .expect("job");
    assert_eq!(job.clone_url, format!("{}/alice/demo.git", env.url));
    assert_eq!(job.repo_full_name, "alice/demo");
    assert_eq!(job.sha, SHA);
    assert_eq!(job.r#ref, "refs/heads/main");
    assert_eq!(job.steps.len(), 1);
    assert_eq!(job.env["GREETING"], "hi");
    assert_eq!(job.timeout_secs, 120);
    assert!(job.job_token.starts_with("fnj_") && job.job_token.len() == 68);
    assert!(job.mask.contains(&job.job_token));
    let token_hash = hash_token(&job.job_token);
    let grant = env
        .db
        .job_token_repo(&token_hash, now_unix())
        .await
        .expect("db");
    let attempt_id: i64 = job.attempt_id.parse().expect("numeric id");
    assert_eq!(grant, Some((attempt_id, env.repo.id)));

    // Runner B can't touch A's attempt.
    let err = b
        .log(tokio_stream_iter(vec![chunk(&job.attempt_id, 0, b"evil")]))
        .await
        .expect_err("foreign log");
    assert_eq!(err.code(), Code::PermissionDenied);
    let err = b
        .update(UpdateRequest {
            attempt_id: job.attempt_id.clone(),
            step_index: 0,
            step_state: StepState::Running as i32,
            ..Default::default()
        })
        .await
        .expect_err("foreign update");
    assert_eq!(err.code(), Code::PermissionDenied);
    let st = b
        .extend(ExtendRequest {
            attempt_id: job.attempt_id.clone(),
        })
        .await
        .expect("extend")
        .into_inner()
        .state();
    assert_eq!(st, ExtendState::Lost);
    let err = b
        .done(DoneRequest {
            attempt_id: job.attempt_id.clone(),
            result: JobResult::Success as i32,
            ..Default::default()
        })
        .await
        .expect_err("foreign done");
    assert_eq!(err.code(), Code::FailedPrecondition);
    let err = b
        .done(DoneRequest {
            attempt_id: "999999".into(),
            result: JobResult::Success as i32,
            ..Default::default()
        })
        .await
        .expect_err("unknown");
    assert_eq!(err.code(), Code::NotFound);
    assert_eq!(env.db.log_len(attempt_id).await.expect("len"), 0);

    // Runner A: heartbeat, step state, logs (idempotent, gap → committed offset), done.
    let st = a
        .extend(ExtendRequest {
            attempt_id: job.attempt_id.clone(),
        })
        .await
        .expect("extend")
        .into_inner()
        .state();
    assert_eq!(st, ExtendState::Ok);
    a.update(UpdateRequest {
        attempt_id: job.attempt_id.clone(),
        step_index: 0,
        step_state: StepState::Success as i32,
        exit_code: 0,
        ..Default::default()
    })
    .await
    .expect("update");
    assert_eq!(
        env.svc.step_states(attempt_id)[&0].state,
        StepState::Success
    );
    let ack = a
        .log(tokio_stream_iter(vec![
            chunk(&job.attempt_id, 0, b"hello "),
            chunk(&job.attempt_id, 6, b"world\n"),
        ]))
        .await
        .expect("log")
        .into_inner();
    assert_eq!(ack.committed_offset, 12);
    let ack = a
        .log(tokio_stream_iter(vec![
            chunk(&job.attempt_id, 6, b"world\n"),
            chunk(&job.attempt_id, 100, b"gap"),
            chunk(&job.attempt_id, 12, b"never"),
        ]))
        .await
        .expect("log")
        .into_inner();
    assert_eq!(
        ack.committed_offset, 12,
        "gap answers with committed offset"
    );
    a.done(DoneRequest {
        attempt_id: job.attempt_id.clone(),
        result: JobResult::Success as i32,
        exit_code: 0,
        error: String::new(),
    })
    .await
    .expect("done");
    // Retried Done after success: rejected so the agent stops retrying.
    let err = a
        .done(DoneRequest {
            attempt_id: job.attempt_id.clone(),
            result: JobResult::Success as i32,
            ..Default::default()
        })
        .await
        .expect_err("second done");
    assert_eq!(err.code(), Code::FailedPrecondition);

    assert_eq!(
        env.wait_pipeline(p.id).await.status,
        PipelineStatus::Success
    );
    let (log, _) = env.db.read_log(attempt_id, 0, 1024).await.expect("log");
    assert_eq!(log, b"hello world\n");
    assert_eq!(
        env.db
            .job_token_repo(&token_hash, now_unix())
            .await
            .expect("db"),
        None,
        "job token revoked on Done"
    );
    assert!(env.svc.step_states(attempt_id).is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn reaper_requeues_expired_lease_and_wakes_runner() {
    let env = start_with(|c| c.lease_secs = 1).await;
    let token = env.runner("r1").await;
    let mut c = client(&env.url, Some(&token)).await;
    let mut p_job = job("build", &[], &["true"]);
    p_job.max_attempts = 2;
    let p = env.pipeline(vec![p_job]).await;
    let first = next(&mut c, Duration::from_secs(5))
        .await
        .expect("next")
        .expect("job");
    let hash = hash_token(&first.job_token);
    // The runner shows life, so the expiry below counts as a lost attempt.
    c.extend(ExtendRequest {
        attempt_id: first.attempt_id.clone(),
    })
    .await
    .expect("extend");

    // A waiting poll is woken by the reaper's requeue.
    let mut waiter = c.clone();
    let poll = tokio::spawn(async move { next(&mut waiter, Duration::from_secs(20)).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let reaped = env.svc.reap_once(now_unix() + 5).await.expect("reap");
    assert_eq!((reaped.lost, reaped.requeued), (1, 1));
    let second = tokio::time::timeout(Duration::from_secs(3), poll)
        .await
        .expect("woken promptly")
        .expect("join")
        .expect("next")
        .expect("requeued job");
    assert_ne!(second.attempt_id, first.attempt_id);
    assert_eq!(
        env.db.job_token_repo(&hash, now_unix()).await.expect("db"),
        None,
        "lost attempt's token pruned"
    );
    // The lost attempt can no longer report.
    let err = c
        .done(DoneRequest {
            attempt_id: first.attempt_id.clone(),
            result: JobResult::Success as i32,
            ..Default::default()
        })
        .await
        .expect_err("lost");
    assert_eq!(err.code(), Code::FailedPrecondition);
    c.done(DoneRequest {
        attempt_id: second.attempt_id,
        result: JobResult::Success as i32,
        ..Default::default()
    })
    .await
    .expect("done");
    assert_eq!(
        env.wait_pipeline(p.id).await.status,
        PipelineStatus::Success
    );
}

/// C1: the `Next` reply is lost after the claim (the runner never shows life); the reaper
/// requeues the job without consuming its only attempt.
#[tokio::test(flavor = "multi_thread")]
async fn lost_next_reply_requeues_without_consuming_attempt() {
    let env = start_with(|c| c.lease_secs = 1).await;
    let token = env.runner("r1").await;
    let mut c = client(&env.url, Some(&token)).await;
    let p = env.pipeline(vec![job("build", &[], &["true"])]).await; // max_attempts = 1
    let lost = next(&mut c, Duration::from_secs(5))
        .await
        .expect("next")
        .expect("job");
    // ... the reply never reaches the agent: no Extend/Log/Update/Done.
    let reaped = env.svc.reap_once(now_unix() + 5).await.expect("reap");
    assert_eq!((reaped.lost, reaped.requeued, reaped.released), (0, 1, 1));
    let again = next(&mut c, Duration::from_secs(5))
        .await
        .expect("next")
        .expect("requeued job");
    assert_ne!(again.attempt_id, lost.attempt_id);
    c.done(DoneRequest {
        attempt_id: again.attempt_id.clone(),
        result: JobResult::Success as i32,
        ..Default::default()
    })
    .await
    .expect("done");
    assert_eq!(
        env.wait_pipeline(p.id).await.status,
        PipelineStatus::Success
    );
    let jobs = env.db.jobs_for_pipeline(p.id).await.expect("jobs");
    assert_eq!(jobs[0].attempts, 1);
}

/// S1: the job token is revoked once the checkout is over (first step `Update`), while the
/// attempt keeps running.
#[tokio::test(flavor = "multi_thread")]
async fn job_token_revoked_after_checkout() {
    let env = start_with(|c| c.checkout = true).await;
    let token = env.runner("r1").await;
    let mut c = client(&env.url, Some(&token)).await;
    let _p = env.pipeline(vec![job("build", &[], &["true"])]).await;
    let job = next(&mut c, Duration::from_secs(5))
        .await
        .expect("next")
        .expect("job");
    let hash = hash_token(&job.job_token);
    assert!(
        env.db
            .job_token_repo(&hash, now_unix())
            .await
            .expect("db")
            .is_some()
    );
    c.update(UpdateRequest {
        attempt_id: job.attempt_id.clone(),
        step_index: 0,
        step_state: StepState::Running as i32,
        ..Default::default()
    })
    .await
    .expect("update");
    assert_eq!(
        env.db.job_token_repo(&hash, now_unix()).await.expect("db"),
        None,
        "job token must not outlive the checkout"
    );
    let attempt_id: i64 = job.attempt_id.parse().expect("id");
    let attempt = env.db.attempt(attempt_id).await.expect("q").expect("a");
    assert_eq!(attempt.status, fornax_core::ci::AttemptStatus::Running);
}

/// S5: no log bytes are accepted once the attempt finished.
#[tokio::test(flavor = "multi_thread")]
async fn log_after_done_is_rejected() {
    let env = start().await;
    let token = env.runner("r1").await;
    let mut c = client(&env.url, Some(&token)).await;
    let _p = env.pipeline(vec![job("build", &[], &["true"])]).await;
    let job = next(&mut c, Duration::from_secs(5))
        .await
        .expect("next")
        .expect("job");
    c.log(tokio_stream_iter(vec![chunk(&job.attempt_id, 0, b"ok\n")]))
        .await
        .expect("log");
    c.done(DoneRequest {
        attempt_id: job.attempt_id.clone(),
        result: JobResult::Success as i32,
        ..Default::default()
    })
    .await
    .expect("done");
    let err = c
        .log(tokio_stream_iter(vec![chunk(
            &job.attempt_id,
            3,
            b"more\n",
        )]))
        .await
        .expect_err("closed");
    assert_eq!(err.code(), Code::FailedPrecondition);
    let attempt_id: i64 = job.attempt_id.parse().expect("id");
    assert_eq!(env.db.log_len(attempt_id).await.expect("len"), 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_payload_fails_job_instead_of_dispatching() {
    let env = start().await;
    let token = env.runner("r1").await;
    let mut c = client(&env.url, Some(&token)).await;
    let mut bad = job("build", &[], &[]);
    bad.payload = "{not json".into();
    let p = env.pipeline(vec![bad]).await;
    assert!(
        next(&mut c, Duration::from_secs(3))
            .await
            .expect("next")
            .is_none()
    );
    let done = env.wait_pipeline(p.id).await;
    assert_eq!(done.status, PipelineStatus::Failed);
    let jobs = env.db.jobs_for_pipeline(p.id).await.expect("jobs");
    let attempts = env.db.attempts_for_job(jobs[0].id).await.expect("attempts");
    assert!(
        attempts[0]
            .error
            .as_deref()
            .is_some_and(|e| e.contains("invalid job payload"))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_releases_long_polls() {
    let env = start().await;
    let token = env.runner("r1").await;
    let mut c = client(&env.url, Some(&token)).await;
    let poll = tokio::spawn(async move { next(&mut c, Duration::from_secs(30)).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    env.svc.shutdown();
    let res = tokio::time::timeout(Duration::from_secs(2), poll)
        .await
        .expect("released")
        .expect("join")
        .expect("next");
    assert!(res.is_none());
}

/// A finished stream of chunks for one client-streaming `Log` call.
fn tokio_stream_iter(
    chunks: Vec<LogChunk>,
) -> Request<tokio_stream::Iter<std::vec::IntoIter<LogChunk>>> {
    Request::new(tokio_stream::iter(chunks))
}

/// S5: the agent truncates its logs below the server cap (two crates, one limit).
#[test]
fn agent_log_limit_matches_server_cap() {
    assert_eq!(
        fornax_agent::agent::MAX_LOG_BYTES,
        fornax_db::LogAppend::MAX_BYTES
    );
}
