// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! Running external commands (`gcloud`, `git`, `docker`, `systemctl` …) — never through a
//! shell. Every call goes through a [`Runner`], so `--dry-run` can print instead of execute
//! and tests can record calls.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// One command line plus its environment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Cmd {
    /// Program name or path.
    pub program: String,
    /// Arguments (passed as-is, no shell).
    pub args: Vec<String>,
    /// Extra environment variables.
    pub env: BTreeMap<String, String>,
    /// Working directory.
    pub cwd: Option<PathBuf>,
    /// Bytes written to stdin.
    pub stdin: Option<Vec<u8>>,
    /// Kill the process after this long.
    pub timeout: Option<Duration>,
}

impl Cmd {
    /// `program` with no arguments.
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            ..Self::default()
        }
    }

    /// Append one argument.
    pub fn arg(mut self, a: impl Into<String>) -> Self {
        self.args.push(a.into());
        self
    }

    /// Append several arguments.
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Set an environment variable.
    pub fn env(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.env.insert(k.into(), v.into());
        self
    }

    /// Run in `dir`.
    pub fn cwd(mut self, dir: impl Into<PathBuf>) -> Self {
        self.cwd = Some(dir.into());
        self
    }

    /// Feed `data` to stdin.
    pub fn stdin(mut self, data: impl Into<Vec<u8>>) -> Self {
        self.stdin = Some(data.into());
        self
    }

    /// Kill after `t`.
    pub fn timeout(mut self, t: Duration) -> Self {
        self.timeout = Some(t);
        self
    }

    /// Shell-quoted rendering for logs and `--dry-run` (never executed by a shell).
    pub fn display(&self) -> String {
        let mut parts: Vec<String> = self
            .env
            .iter()
            .map(|(k, v)| format!("{k}={}", quote(v)))
            .collect();
        parts.push(quote(&self.program));
        parts.extend(self.args.iter().map(|a| quote(a)));
        parts.join(" ")
    }
}

fn quote(s: &str) -> String {
    if !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_./:=@,+%".contains(&b))
    {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// Result of a finished command.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Output {
    /// Exit code (`-1` when killed by a signal or the timeout).
    pub code: i32,
    /// Captured stdout (lossy UTF-8).
    pub stdout: String,
    /// Captured stderr (lossy UTF-8).
    pub stderr: String,
    /// True when the timeout fired.
    pub timed_out: bool,
}

impl Output {
    /// Exit code 0 and no timeout.
    pub fn ok(&self) -> bool {
        self.code == 0 && !self.timed_out
    }
}

/// Errors starting a command.
#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    /// The program is not installed / not on `PATH`.
    #[error("'{0}' is not installed or not on PATH")]
    NotFound(String),
    /// Spawning or I/O failed.
    #[error("running {cmd}: {source}")]
    Io {
        /// Rendered command.
        cmd: String,
        /// Underlying error.
        source: std::io::Error,
    },
}

/// Something that runs commands.
pub trait Runner: Send + Sync {
    /// Run `cmd` to completion and capture its output.
    fn run(&self, cmd: &Cmd) -> Result<Output, ExecError>;
    /// True when commands are only printed (`--dry-run`).
    fn is_dry_run(&self) -> bool {
        false
    }
}

/// Runs commands for real.
#[derive(Debug, Default, Clone, Copy)]
pub struct RealRunner;

impl Runner for RealRunner {
    fn run(&self, cmd: &Cmd) -> Result<Output, ExecError> {
        let mut c = Command::new(&cmd.program);
        c.args(&cmd.args)
            .envs(&cmd.env)
            .stdin(if cmd.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(dir) = &cmd.cwd {
            c.current_dir(dir);
        }
        let mut child = c.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                ExecError::NotFound(cmd.program.clone())
            } else {
                ExecError::Io {
                    cmd: cmd.display(),
                    source: e,
                }
            }
        })?;
        let io_err = |e| ExecError::Io {
            cmd: cmd.display(),
            source: e,
        };
        if let (Some(data), Some(mut sin)) = (cmd.stdin.clone(), child.stdin.take()) {
            std::thread::spawn(move || {
                let _ = sin.write_all(&data);
            });
        }
        // Drain both pipes on threads so a chatty child can't fill a pipe and deadlock.
        let mut out = child.stdout.take();
        let mut err = child.stderr.take();
        let t_out = std::thread::spawn(move || {
            let mut b = Vec::new();
            if let Some(o) = out.as_mut() {
                let _ = o.read_to_end(&mut b);
            }
            b
        });
        let t_err = std::thread::spawn(move || {
            let mut b = Vec::new();
            if let Some(e) = err.as_mut() {
                let _ = e.read_to_end(&mut b);
            }
            b
        });
        let started = Instant::now();
        let mut timed_out = false;
        let status = loop {
            if let Some(s) = child.try_wait().map_err(io_err)? {
                break Some(s);
            }
            if let Some(t) = cmd.timeout
                && started.elapsed() >= t
            {
                let _ = child.kill();
                timed_out = true;
                break child.wait().ok();
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let stdout = String::from_utf8_lossy(&t_out.join().unwrap_or_default()).into_owned();
        let stderr = String::from_utf8_lossy(&t_err.join().unwrap_or_default()).into_owned();
        Ok(Output {
            code: status.and_then(|s| s.code()).unwrap_or(-1),
            stdout,
            stderr,
            timed_out,
        })
    }
}

/// Prints commands instead of running them; every command "succeeds" with empty output.
#[derive(Debug, Default)]
pub struct DryRunner {
    /// Every command that would have run.
    pub log: Mutex<Vec<String>>,
}

impl Runner for DryRunner {
    fn run(&self, cmd: &Cmd) -> Result<Output, ExecError> {
        let line = cmd.display();
        println!("[dry-run] {line}");
        if let Ok(mut log) = self.log.lock() {
            log.push(line);
        }
        Ok(Output::default())
    }

    fn is_dry_run(&self) -> bool {
        true
    }
}

/// Find `program` on `PATH` (like `which`).
pub fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|p| is_executable(p))
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_quotes_only_when_needed() {
        let c = Cmd::new("gcloud")
            .args(["pubsub", "topics", "create", "t1"])
            .arg("--format=json")
            .arg("it's")
            .env("CLOUDSDK_CONFIG", "/data/g c");
        assert_eq!(
            c.display(),
            "CLOUDSDK_CONFIG='/data/g c' gcloud pubsub topics create t1 --format=json 'it'\\''s'"
        );
    }

    #[test]
    fn real_runner_captures_exit_and_output() {
        let out = RealRunner
            .run(&Cmd::new("sh").args(["-c", "echo hi; echo err >&2; exit 3"]))
            .expect("run");
        assert_eq!(
            (out.code, out.stdout.as_str(), out.stderr.as_str()),
            (3, "hi\n", "err\n")
        );
        let out = RealRunner
            .run(&Cmd::new("cat").stdin("piped"))
            .expect("run");
        assert!(out.ok() && out.stdout == "piped");
    }

    #[test]
    fn timeout_kills() {
        let out = RealRunner
            .run(
                &Cmd::new("sleep")
                    .arg("5")
                    .timeout(Duration::from_millis(200)),
            )
            .expect("run");
        assert!(out.timed_out && !out.ok());
    }

    #[test]
    fn missing_program_is_reported() {
        assert!(matches!(
            RealRunner.run(&Cmd::new("definitely-not-installed-xyz")),
            Err(ExecError::NotFound(_))
        ));
        assert!(which("sh").is_some() && which("definitely-not-installed-xyz").is_none());
    }

    #[test]
    fn dry_runner_records() {
        let d = DryRunner::default();
        let out = d
            .run(&Cmd::new("systemctl").args(["enable", "--now", "x"]))
            .expect("run");
        assert!(out.ok() && d.is_dry_run());
        assert_eq!(
            d.log.lock().expect("lock").as_slice(),
            ["systemctl enable --now x"]
        );
    }
}
