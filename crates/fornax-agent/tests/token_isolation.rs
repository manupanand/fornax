// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! S1: a job step must not be able to read the agent's environment (where
//! `FORNAX_RUNNER_TOKEN` lives) through `/proc/$PPID/environ`.
//!
//! Setting a canary in our own environment would need `unsafe` (`std::env::set_var`), so the
//! outer test re-runs this test binary with the canary in the child's environment; the child
//! plays the agent: it runs a step that dumps its parent's environ, once before and once
//! after [`fornax_agent::host::harden_process`].
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used)]

use std::process::Command;

use fornax_agent::{Executor, HostExecutor, LogSink, SinkEvent};
use fornax_proto::runner::v1::{Job, Step};
use tokio_util::sync::CancellationToken;

const CANARY_VAR: &str = "FORNAX_TEST_RUNNER_TOKEN_CANARY";
const CANARY: &str = "fnr_canary_5f1d9e0b7c";

async fn step_output(exec: &HostExecutor, run: &str) -> String {
    let job = Job {
        attempt_id: "1".into(),
        name: "steal".into(),
        steps: vec![Step {
            name: "steal".into(),
            run: run.into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    let (sink, mut events) = LogSink::new(Vec::<String>::new(), 64);
    let collector = tokio::spawn(async move {
        let mut log = Vec::new();
        while let Some(e) = events.recv().await {
            if let SinkEvent::Log { data, .. } = e {
                log.extend(data);
            }
        }
        String::from_utf8_lossy(&log).into_owned()
    });
    exec.run(&job, sink, CancellationToken::new()).await;
    collector.await.unwrap()
}

/// Runs only inside the child started by `step_cannot_read_agent_environ`.
#[tokio::test]
async fn inner_agent_environ_probe() {
    if std::env::var(CANARY_VAR).as_deref() != Ok(CANARY) {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let exec = HostExecutor::new(tmp.path().join("work"));
    let steal = "tr '\\0' '\\n' < /proc/$PPID/environ || true";
    let before = step_output(&exec, steal).await;
    // Control: without hardening the step does see the canary (the test is meaningful).
    println!("PROBE-BEFORE visible={}", before.contains(CANARY));
    fornax_agent::host::harden_process().unwrap();
    let after = step_output(&exec, steal).await;
    println!("PROBE-AFTER visible={}", after.contains(CANARY));
    assert!(
        !after.contains(CANARY),
        "step read the agent environ: {after}"
    );
}

#[test]
fn step_cannot_read_agent_environ() {
    let out = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "inner_agent_environ_probe", "--nocapture"])
        .env(CANARY_VAR, CANARY)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "probe failed:\n{stdout}\n{stderr}");
    assert!(
        stdout.contains("PROBE-BEFORE visible=true"),
        "control: an unhardened agent's environ should be readable:\n{stdout}"
    );
    assert!(stdout.contains("PROBE-AFTER visible=false"), "{stdout}");
}
