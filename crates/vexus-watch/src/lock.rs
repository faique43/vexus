//! Advisory writer lock using OS file locking (per-handle).
//!
//! This module provides a WriterLock that ensures at most one process
//! (or "vexus serve" instance) writes the index at a time.

use std::fs::{File, TryLockError};
use std::path::Path;

use anyhow::{Context, Result};

/// Advisory writer lock on `.vexus/lock`. Held while alive; released on drop
/// (explicit unlock + handle close both release it).
///
/// Uses `std::fs::File::try_lock` — flock(2) on Unix, LockFileEx on Windows —
/// so the same single code path covers every supported platform.
pub struct WriterLock {
    file: File,
}

impl WriterLock {
    /// Try to acquire an exclusive write lock on `root/.vexus/lock`.
    ///
    /// Returns:
    /// - `Ok(Some(WriterLock))` if we acquired the lock (we're the writer)
    /// - `Ok(None)` if another process/handle holds the lock (we're a reader)
    /// - `Err(_)` on I/O or other errors
    pub fn try_acquire(root: &Path) -> Result<Option<WriterLock>> {
        let dir = root.join(".vexus");
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create .vexus directory at {}", dir.display()))?;

        let path = dir.join("lock");
        let file = File::create(&path)
            .with_context(|| format!("failed to create lock file at {}", path.display()))?;

        match file.try_lock() {
            Ok(()) => Ok(Some(WriterLock { file })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(err)) => Err(err).context("lock .vexus/lock"),
        }
    }
}

impl Drop for WriterLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

/// Returns the role line for the status output.
///
/// - For writer (is_writer=true): `Some("role: writer")`
/// - For reader (is_writer=false): `Some("role: reader (another vexus serve owns the index)")`
pub fn role_line(is_writer: bool) -> Option<String> {
    if is_writer {
        Some("role: writer".to_string())
    } else {
        Some("role: reader (another vexus serve owns the index)".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn try_acquire_succeeds_first_time() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        let lock1 = WriterLock::try_acquire(root).unwrap();
        assert!(lock1.is_some(), "first try_acquire should succeed");
    }

    // flock and LockFileEx are both per-handle, so two try_acquire calls in
    // one process open two handles and genuinely contend — this asserts
    // mutual exclusion on every platform.
    #[test]
    fn try_acquire_fails_when_lock_held_in_same_process() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        let _lock1 = WriterLock::try_acquire(root)
            .unwrap()
            .expect("first try_acquire should succeed");
        let lock2 = WriterLock::try_acquire(root).unwrap();
        assert!(
            lock2.is_none(),
            "second try_acquire should fail while first lock is held"
        );
    }

    #[test]
    fn try_acquire_succeeds_after_lock_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        {
            let _lock1 = WriterLock::try_acquire(root)
                .unwrap()
                .expect("first try_acquire should succeed");
            // lock1 dropped here
        }

        let lock3 = WriterLock::try_acquire(root).unwrap();
        assert!(
            lock3.is_some(),
            "third try_acquire should succeed after first was dropped"
        );
    }

    #[test]
    fn role_line_writer() {
        let line = role_line(true);
        assert_eq!(line, Some("role: writer".to_string()));
    }

    #[test]
    fn role_line_reader() {
        let line = role_line(false);
        assert_eq!(
            line,
            Some("role: reader (another vexus serve owns the index)".to_string())
        );
    }
}
