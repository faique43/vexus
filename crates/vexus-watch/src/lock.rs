//! Advisory writer lock using flock (per-fd locking).
//!
//! This module provides a WriterLock that ensures at most one process
//! (or "vexus serve" instance) writes the index at a time.

use std::fs::File;
use std::path::Path;

use anyhow::{Context, Result};

#[cfg(unix)]
use std::os::unix::io::AsRawFd;

/// Advisory writer lock on `.vexus/lock`. Held while alive; released on drop
/// (explicit funlock + fd close both release it).
///
/// Unix only: uses flock for per-fd locking. Non-unix platforms return
/// Some() always (no locking yet; document as a todo).
pub struct WriterLock {
    #[cfg(unix)]
    file: File,
    #[cfg(not(unix))]
    _marker: std::marker::PhantomData<()>,
}

impl WriterLock {
    /// Try to acquire an exclusive write lock on `root/.vexus/lock`.
    ///
    /// Returns:
    /// - `Ok(Some(WriterLock))` if we acquired the lock (we're the writer)
    /// - `Ok(None)` if another process/fd holds the lock (we're a reader)
    /// - `Err(_)` on I/O or other errors
    pub fn try_acquire(root: &Path) -> Result<Option<WriterLock>> {
        #[cfg(unix)]
        {
            let dir = root.join(".vexus");
            std::fs::create_dir_all(&dir).with_context(|| {
                format!("failed to create .vexus directory at {}", dir.display())
            })?;

            let path = dir.join("lock");
            let file = File::create(&path)
                .with_context(|| format!("failed to create lock file at {}", path.display()))?;

            // LOCK_EX | LOCK_NB: exclusive, non-blocking
            let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };

            if rc == 0 {
                Ok(Some(WriterLock { file }))
            } else {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
                    Ok(None)
                } else {
                    Err(err).context("flock .vexus/lock")
                }
            }
        }

        #[cfg(not(unix))]
        {
            // Non-unix platforms: no locking yet. TODO: implement Windows equivalent.
            let _ = root;
            Ok(Some(WriterLock {
                _marker: std::marker::PhantomData,
            }))
        }
    }
}

#[cfg(unix)]
impl Drop for WriterLock {
    fn drop(&mut self) {
        unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
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
