// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! End-to-end: the agent loop against an in-process mock Runner server on real TCP.
#![allow(clippy::unwrap_used)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fornax_agent::AgentConfig;
use fornax_proto::runner::v1::runner_server::{Runner, RunnerServer};
use fornax_proto::runner::v1::{
    DoneRequest, DoneResponse, ExtendRequest, ExtendResponse, ExtendState, Job, JobResult, LogAck,
    LogChunk, NextRequest, NextResponse, RegisterRequest, RegisterResponse, Step, StepState,
    UpdateRequest, UpdateResponse,
};
use tokio::sync::Notify;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::{Request, Response, Status, Streaming};

const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

fn token() -> String {
    format!("fnr_{}", "ab".repeat(32))
}

#[derive(Default)]
struct State {
    queue: VecDeque<Job>,
    log: Vec<u8>,
    chunks: usize,
    extends: usize,
    updates: Vec<(u32, StepState)>,
    dones: Vec<DoneRequest>,
    cancel: bool,
    /// Fail the first N Done calls with UNAVAILABLE.
    fail_done: usize,
    /// Fail the first N Log calls with UNAVAILABLE.
    fail_log: usize,
    /// Refuse Log calls of this attempt permanently with NOT_FOUND.
    reject_log_for: Option<String>,
    /// Log calls refused because of `reject_log_for`.
    rejected_logs: usize,
}

#[derive(Clone, Default)]
struct Mock {
    state: Arc<Mutex<State>>,
    done: Arc<Notify>,
}

impl Mock {
    fn auth<T>(req: &Request<T>) -> Result<(), Status> {
        let want = format!("Bearer {}", token());
        match req
            .metadata()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
        {
            Some(v) if v == want => Ok(()),
            _ => Err(Status::unauthenticated("bad token")),
        }
    }
}

#[tonic::async_trait]
impl Runner for Mock {
    async fn register(
        &self,
        _req: Request<RegisterRequest>,
    ) -> Result<Response<RegisterResponse>, Status> {
        Ok(Response::new(RegisterResponse {
            runner_id: "r1".into(),
            runner_token: token(),
        }))
    }

    async fn next(&self, req: Request<NextRequest>) -> Result<Response<NextResponse>, Status> {
        Self::auth(&req)?;
        assert!(req.get_ref().capacity_free >= 1);
        let job = self.state.lock().unwrap().queue.pop_front();
        if job.is_none() {
            // Short long-poll.
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        Ok(Response::new(NextResponse { job }))
    }

    async fn extend(
        &self,
        req: Request<ExtendRequest>,
    ) -> Result<Response<ExtendResponse>, Status> {
        Self::auth(&req)?;
        let mut s = self.state.lock().unwrap();
        s.extends += 1;
        let state = if s.cancel {
            ExtendState::Cancel
        } else {
            ExtendState::Ok
        };
        Ok(Response::new(ExtendResponse {
            state: state as i32,
        }))
    }

    async fn update(
        &self,
        req: Request<UpdateRequest>,
    ) -> Result<Response<UpdateResponse>, Status> {
        Self::auth(&req)?;
        let u = req.into_inner();
        self.state
            .lock()
            .unwrap()
            .updates
            .push((u.step_index, u.step_state()));
        Ok(Response::new(UpdateResponse {}))
    }

    async fn log(&self, req: Request<Streaming<LogChunk>>) -> Result<Response<LogAck>, Status> {
        Self::auth(&req)?;
        {
            let mut s = self.state.lock().unwrap();
            if s.fail_log > 0 {
                s.fail_log -= 1;
                return Err(Status::unavailable("try again"));
            }
        }
        let mut stream = req.into_inner();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            let mut s = self.state.lock().unwrap();
            if s.reject_log_for.as_deref() == Some(chunk.attempt_id.as_str()) {
                s.rejected_logs += 1;
                return Err(Status::not_found("unknown attempt"));
            }
            s.chunks += 1;
            // Idempotent append by offset.
            let have = s.log.len() as u64;
            let end = chunk.offset + chunk.data.len() as u64;
            if chunk.offset <= have && end > have {
                let skip = usize::try_from(have - chunk.offset).unwrap();
                let tail = chunk.data[skip..].to_vec();
                s.log.extend(tail);
            }
        }
        let committed = self.state.lock().unwrap().log.len() as u64;
        Ok(Response::new(LogAck {
            committed_offset: committed,
        }))
    }

    async fn done(&self, req: Request<DoneRequest>) -> Result<Response<DoneResponse>, Status> {
        Self::auth(&req)?;
        {
            let mut s = self.state.lock().unwrap();
            if s.fail_done > 0 {
                s.fail_done -= 1;
                return Err(Status::unavailable("try again"));
            }
            s.dones.push(req.into_inner());
        }
        self.done.notify_waiters();
        Ok(Response::new(DoneResponse {}))
    }
}

async fn start(mock: Mock) -> (String, tokio::sync::oneshot::Sender<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(RunnerServer::new(mock))
            .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async {
                let _ = rx.await;
            })
            .await
            .unwrap();
    });
    (format!("http://{addr}"), tx)
}

fn job(run: &str) -> Job {
    Job {
        attempt_id: "att-e2e".into(),
        job_id: "j1".into(),
        pipeline_id: "p1".into(),
        name: "hello".into(),
        repo_full_name: "alice/demo".into(),
        sha: SHA.into(),
        r#ref: "refs/heads/main".into(),
        steps: vec![Step {
            name: "greet".into(),
            run: run.into(),
            ..Default::default()
        }],
        mask: vec!["topsecret".into()],
        ..Default::default()
    }
}

fn config(url: String, dir: &std::path::Path) -> AgentConfig {
    let mut c = AgentConfig::new(url, token(), dir.join("work"));
    c.poll_timeout = Duration::from_secs(5);
    c.heartbeat_interval = Duration::from_millis(200);
    c.shutdown_timeout = Duration::from_secs(5);
    c
}

async fn wait_done(mock: &Mock) -> DoneRequest {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let notified = mock.done.notified();
            if let Some(d) = mock.state.lock().unwrap().dones.first().cloned() {
                return d;
            }
            notified.await;
        }
    })
    .await
    .expect("Done within 30 s")
}

#[tokio::test]
async fn agent_runs_job_streams_logs_and_reports_success() {
    let mock = Mock::default();
    {
        let mut s = mock.state.lock().unwrap();
        s.queue.push_back(job(
            "echo hello && echo $FORNAX_SHA && sleep 0.7 && echo topsecret",
        ));
        s.fail_done = 2;
        s.fail_log = 1;
    }
    let (url, stop_server) = start(mock.clone()).await;
    let tmp = tempfile::tempdir().unwrap();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let agent = tokio::spawn(fornax_agent::run(config(url, tmp.path()), async {
        let _ = stop_rx.await;
    }));

    let done = wait_done(&mock).await;
    assert_eq!(done.attempt_id, "att-e2e");
    assert_eq!(done.result(), JobResult::Success, "{done:?}");
    {
        let s = mock.state.lock().unwrap();
        let log = String::from_utf8_lossy(&s.log);
        assert!(log.contains("$ greet\n"), "{log}");
        assert!(log.contains("hello\n"), "{log}");
        assert!(log.contains(&format!("{SHA}\n")), "{log}");
        assert!(log.contains("[MASKED]\n"), "{log}");
        assert!(!log.contains("topsecret"), "{log}");
        assert!(s.extends >= 1, "heartbeats: {}", s.extends);
        assert!(s.updates.contains(&(0, StepState::Running)));
        assert!(s.updates.contains(&(0, StepState::Success)));
        assert_eq!(s.dones.len(), 1);
    }

    stop_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), agent)
        .await
        .expect("agent stops")
        .unwrap()
        .unwrap();
    assert!(!tmp.path().join("work/att-e2e").exists());
    let _ = stop_server.send(());
}

#[tokio::test]
async fn server_cancel_via_extend_reports_canceled() {
    let mock = Mock::default();
    mock.state
        .lock()
        .unwrap()
        .queue
        .push_back(job("echo started; sleep 30; echo never"));
    let (url, stop_server) = start(mock.clone()).await;
    let tmp = tempfile::tempdir().unwrap();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let agent = tokio::spawn(fornax_agent::run(config(url, tmp.path()), async {
        let _ = stop_rx.await;
    }));

    // Let the job start, then cancel through the heartbeat.
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if String::from_utf8_lossy(&mock.state.lock().unwrap().log).contains("started") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("job started");
    mock.state.lock().unwrap().cancel = true;

    let done = wait_done(&mock).await;
    assert_eq!(done.result(), JobResult::Canceled, "{done:?}");
    {
        let s = mock.state.lock().unwrap();
        let log = String::from_utf8_lossy(&s.log);
        assert!(!log.contains("never"), "{log}");
    }
    stop_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), agent)
        .await
        .expect("agent stops")
        .unwrap()
        .unwrap();
    let _ = stop_server.send(());
}

/// C2: a permanent `Log` refusal stops the job, drops its logs, reports Done and frees the
/// capacity slot for the next job (instead of retrying forever).
#[tokio::test]
async fn permanent_log_error_stops_job_and_frees_slot() {
    let mock = Mock::default();
    {
        let mut s = mock.state.lock().unwrap();
        let mut doomed = job("echo doomed; sleep 30; echo never");
        doomed.attempt_id = "11".into();
        let mut next = job("echo second");
        next.attempt_id = "12".into();
        s.queue.push_back(doomed);
        s.queue.push_back(next);
        s.reject_log_for = Some("11".into());
    }
    let (url, stop_server) = start(mock.clone()).await;
    let tmp = tempfile::tempdir().unwrap();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let agent = tokio::spawn(fornax_agent::run(config(url, tmp.path()), async {
        let _ = stop_rx.await;
    }));

    // Capacity is 1: the second job only runs once the first one released its slot.
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let notified = mock.done.notified();
            if mock.state.lock().unwrap().dones.len() >= 2 {
                return;
            }
            notified.await;
        }
    })
    .await
    .expect("both jobs reported well before the 30 s sleep ends");
    {
        let s = mock.state.lock().unwrap();
        assert_eq!(s.dones[0].attempt_id, "11");
        assert_eq!(s.dones[0].result(), JobResult::Error, "{:?}", s.dones[0]);
        assert_eq!(s.dones[1].attempt_id, "12");
        assert_eq!(s.dones[1].result(), JobResult::Success);
        assert_eq!(s.rejected_logs, 1, "no retries after a permanent refusal");
        let log = String::from_utf8_lossy(&s.log);
        assert!(!log.contains("doomed") && log.contains("second\n"), "{log}");
    }
    stop_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), agent)
        .await
        .expect("agent stops")
        .unwrap()
        .unwrap();
    let _ = stop_server.send(());
}

/// C5: leftover attempt workspaces (agent killed with SIGKILL) are swept at start; other
/// entries are kept.
#[tokio::test]
async fn agent_sweeps_leftover_workspaces_at_start() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    std::fs::create_dir_all(work.join("42/src")).unwrap();
    std::fs::write(work.join("42/src/file"), b"stale").unwrap();
    std::fs::create_dir_all(work.join("cache")).unwrap();
    std::fs::write(work.join("7"), b"a file, not a workspace").unwrap();
    let mock = Mock::default();
    let (url, stop_server) = start(mock.clone()).await;
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let agent = tokio::spawn(fornax_agent::run(config(url, tmp.path()), async {
        let _ = stop_rx.await;
    }));
    tokio::time::timeout(Duration::from_secs(10), async {
        while work.join("42").exists() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("stale workspace removed");
    assert!(work.join("cache").is_dir());
    assert!(work.join("7").is_file());
    stop_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), agent)
        .await
        .expect("agent stops")
        .unwrap()
        .unwrap();
    let _ = stop_server.send(());
}

#[tokio::test]
async fn agent_retries_while_server_is_down() {
    // Reserve a port, start the agent first, then the server on that port.
    let tmp = tempfile::tempdir().unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let agent = tokio::spawn(fornax_agent::run(
        config(format!("http://{addr}"), tmp.path()),
        async {
            let _ = stop_rx.await;
        },
    ));
    tokio::time::sleep(Duration::from_millis(800)).await;

    let mock = Mock::default();
    mock.state.lock().unwrap().queue.push_back(job("echo late"));
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    let (srv_tx, srv_rx) = tokio::sync::oneshot::channel::<()>();
    let m = mock.clone();
    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(RunnerServer::new(m))
            .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async {
                let _ = srv_rx.await;
            })
            .await
            .unwrap();
    });

    let done = wait_done(&mock).await;
    assert_eq!(done.result(), JobResult::Success);
    assert!(String::from_utf8_lossy(&mock.state.lock().unwrap().log).contains("late\n"));
    stop_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), agent)
        .await
        .expect("agent stops")
        .unwrap()
        .unwrap();
    let _ = srv_tx.send(());
}
