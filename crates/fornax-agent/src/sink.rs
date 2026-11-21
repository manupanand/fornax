// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! The executor-facing output channel of a job: log bytes and step-state events.
//!
//! [`LogSink`] masks secrets **before** anything leaves it (one streaming [`Masker`] per log
//! stream), then forwards events over a bounded channel (back-pressure on the writer). In the
//! agent the receiving end is the uploader, which packs the masked bytes into offset-tracked
//! chunks and retries them until the server acknowledges them; tests read the events directly
//! via [`LogEvents`].

use std::sync::{Arc, Mutex, MutexGuard};

use fornax_proto::runner::v1::{LogStream, StepState};
use tokio::sync::mpsc;

use crate::masker::Masker;

/// The state of one step, reported to the server with `Update`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepUpdate {
    /// Index of the step in `Job.steps`.
    pub index: u32,
    /// New state.
    pub state: StepState,
    /// Unix time in milliseconds when the step started (0 = not started).
    pub started_at: i64,
    /// Unix time in milliseconds when the step finished (0 = not finished).
    pub finished_at: i64,
    /// Exit code of the step process (0 unless the step ran and finished).
    pub exit_code: i32,
}

/// One event produced by a job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SinkEvent {
    /// Already-masked log bytes of one stream.
    Log {
        /// Which stream the bytes belong to.
        stream: LogStream,
        /// Masked bytes.
        data: Vec<u8>,
    },
    /// A step changed state.
    Step(StepUpdate),
}

/// Receiving end of a [`LogSink`]. Yields `None` once every sink clone is dropped.
#[derive(Debug)]
pub struct LogEvents {
    rx: mpsc::Receiver<SinkEvent>,
}

impl LogEvents {
    /// Receives the next event.
    pub async fn recv(&mut self) -> Option<SinkEvent> {
        self.rx.recv().await
    }
}

#[derive(Debug)]
struct Maskers {
    stdout: Mutex<Masker>,
    stderr: Mutex<Masker>,
    system: Mutex<Masker>,
}

/// Cloneable handle through which an executor emits logs and step updates.
#[derive(Debug, Clone)]
pub struct LogSink {
    tx: mpsc::Sender<SinkEvent>,
    maskers: Arc<Maskers>,
}

fn lock(m: &Mutex<Masker>) -> MutexGuard<'_, Masker> {
    // A poisoned masker still holds consistent state (all updates are single assignments).
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl LogSink {
    /// Creates a sink masking `phrases` and a receiver; `buffer` is the channel capacity in
    /// events (writers wait when it is full).
    pub fn new<I, S>(phrases: I, buffer: usize) -> (Self, LogEvents)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<[u8]>,
    {
        let masker = Masker::new(phrases);
        let (tx, rx) = mpsc::channel(buffer.max(1));
        let maskers = Arc::new(Maskers {
            stdout: Mutex::new(masker.clone()),
            stderr: Mutex::new(masker.clone()),
            system: Mutex::new(masker),
        });
        (Self { tx, maskers }, LogEvents { rx })
    }

    fn masker(&self, stream: LogStream) -> &Mutex<Masker> {
        match stream {
            LogStream::Stdout => &self.maskers.stdout,
            LogStream::Stderr => &self.maskers.stderr,
            LogStream::System | LogStream::Unspecified => &self.maskers.system,
        }
    }

    /// Writes raw bytes of `stream`; they are masked before being forwarded. Up to
    /// `longest_secret - 1` bytes may be held back until more output or [`flush`](Self::flush).
    pub async fn write(&self, stream: LogStream, data: &[u8]) {
        let stream = normalize(stream);
        let out = lock(self.masker(stream)).write(data);
        self.send_log(stream, out).await;
    }

    /// Writes an agent line (`line` + `\n`) to the system stream, flushed immediately.
    pub async fn system(&self, line: &str) {
        let mut data = line.as_bytes().to_vec();
        data.push(b'\n');
        let out = {
            let mut m = lock(&self.maskers.system);
            let mut out = m.write(&data);
            out.extend(m.flush());
            out
        };
        self.send_log(LogStream::System, out).await;
    }

    /// Flushes the bytes held back by every stream's masker (call at the end of a step).
    pub async fn flush(&self) {
        for stream in [LogStream::Stdout, LogStream::Stderr, LogStream::System] {
            let out = lock(self.masker(stream)).flush();
            self.send_log(stream, out).await;
        }
    }

    /// Reports a step state change.
    pub async fn step(&self, update: StepUpdate) {
        // A closed receiver means the job is being torn down; dropping the event is correct.
        let _ = self.tx.send(SinkEvent::Step(update)).await;
    }

    async fn send_log(&self, stream: LogStream, data: Vec<u8>) {
        if !data.is_empty() {
            let _ = self.tx.send(SinkEvent::Log { stream, data }).await;
        }
    }
}

fn normalize(stream: LogStream) -> LogStream {
    match stream {
        LogStream::Unspecified => LogStream::System,
        s => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn masks_per_stream_and_flushes() {
        let (sink, mut ev) = LogSink::new(["secret"], 16);
        sink.write(LogStream::Stdout, b"a sec").await;
        sink.write(LogStream::Stderr, b"err").await;
        sink.write(LogStream::Stdout, b"ret b").await;
        sink.flush().await;
        drop(sink);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        while let Some(e) = ev.recv().await {
            match e {
                SinkEvent::Log {
                    stream: LogStream::Stdout,
                    data,
                } => stdout.extend(data),
                SinkEvent::Log {
                    stream: LogStream::Stderr,
                    data,
                } => stderr.extend(data),
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(stdout, b"a [MASKED] b");
        assert_eq!(stderr, b"err");
    }
}
