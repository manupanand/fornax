// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! Per-app deploy lock: `flock(2)` on `<base_dir>/locks/<name>.lock`, the same lock deploy.sh
//! takes with `flock -n 9`, so `fornax-gcp deploy`, the agent and `flock(1)` exclude each other.

use std::fs::{File, OpenOptions};
use std::path::Path;

use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg};

/// Held lock; released when dropped (or when the process exits).
#[derive(Debug)]
pub struct AppLock {
    _lock: Flock<File>,
}

/// Why the lock was not taken.
#[derive(Debug, thiserror::Error)]
pub enum LockError {
    /// Another process (or another open of the file) holds it.
    #[error("lock is held by another deploy")]
    Busy,
    /// The lock file could not be opened or locked.
    #[error("lock file {path}: {source}")]
    Io {
        /// Lock file.
        path: String,
        /// Underlying error.
        source: std::io::Error,
    },
}

impl AppLock {
    /// Take the lock without waiting.
    pub fn try_lock(path: &Path) -> Result<Self, LockError> {
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(path)
            .map_err(|source| LockError::Io {
                path: path.display().to_string(),
                source,
            })?;
        match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
            Ok(lock) => Ok(Self { _lock: lock }),
            Err((_, Errno::EWOULDBLOCK)) => Err(LockError::Busy),
            Err((_, e)) => Err(LockError::Io {
                path: path.display().to_string(),
                source: std::io::Error::from(e),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_is_exclusive_and_released_on_drop() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("app.lock");
        let first = AppLock::try_lock(&p).expect("first lock");
        assert!(matches!(AppLock::try_lock(&p), Err(LockError::Busy)));
        drop(first);
        let again = AppLock::try_lock(&p).expect("free after drop");
        drop(again);
    }

    #[test]
    fn lock_excludes_flock_1() {
        // The external `flock(1)` used by deploy.sh / humans sees the same lock.
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("app.lock");
        let held = AppLock::try_lock(&p).expect("lock");
        let st = std::process::Command::new("flock")
            .args(["-n"])
            .arg(&p)
            .arg("true")
            .status();
        if let Ok(st) = st {
            assert_eq!(st.code(), Some(1), "flock(1) must be refused");
        }
        drop(held);
    }

    #[test]
    fn missing_folder_is_an_io_error() {
        assert!(matches!(
            AppLock::try_lock(Path::new("/nonexistent-dir/x.lock")),
            Err(LockError::Io { .. })
        ));
    }
}
