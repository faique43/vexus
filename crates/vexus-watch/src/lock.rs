//! Advisory writer lock using fd-lock (per-fd locking).
//!
//! This module provides a WriterLock that ensures at most one process
//! (or "vexus serve" instance) writes the index at a time.

use std::fs::OpenOptions;
use std::path::Path;

use anyhow::{Context, Result};

/// A guard that holds the write lock. We store it to keep the lock alive.
struct LockGuard {
    _lock: fd_lock::RwLock<std::fs::File>,
    _guard: fd_lock::RwLockWriteGuard<'static, std::fs::File>,
}

impl LockGuard {
    /// Try to create a lock guard by acquiring a write lock on the given path.
    fn try_acquire(path: &Path) -> Result<Option<Self>> {
        // Open the lock file for writing (creates it if needed).
        // We don't truncate because we don't care about the file's contents.
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .with_context(|| format!("failed to open lock file at {}", path.display()))?;

        let mut lock = fd_lock::RwLock::new(file);

        // Try to acquire an exclusive write lock without blocking.
        // We use a nested scope to ensure the guard Result is dropped before
        // we move the lock into LockGuard.
        let got_lock = {
            if let Ok(guard) = lock.try_write() {
                // SAFETY: We're extending the lifetime of the guard to 'static.
                // This is safe because we're storing both the lock and the guard
                // in the same struct, and they'll both be dropped together when
                // the LockGuard is dropped.
                let guard = unsafe {
                    std::mem::transmute::<
                        fd_lock::RwLockWriteGuard<std::fs::File>,
                        fd_lock::RwLockWriteGuard<'static, std::fs::File>,
                    >(guard)
                };
                Some(guard)
            } else {
                None
            }
        };

        match got_lock {
            Some(guard) => Ok(Some(LockGuard {
                _lock: lock,
                _guard: guard,
            })),
            None => Ok(None),
        }
    }
}

/// Holds an exclusive write lock on `.vexus/lock`. The lock is advisory
/// (not enforced by the OS, but honored by participating processes) and
/// released when this struct is dropped.
///
/// Two `WriterLock` instances on the same path in the same process will
/// conflict: fd-lock uses flock on Unix (per-fd, not per-process), so two
/// separate File opens will block each other.
pub struct WriterLock {
    _inner: LockGuard,
}

impl WriterLock {
    /// Try to acquire an exclusive write lock on `vexus_dir/.vexus/lock`.
    ///
    /// Returns:
    /// - `Ok(Some(WriterLock))` if we acquired the lock (we're the writer)
    /// - `Ok(None)` if another process/fd holds the lock (we're a reader)
    /// - `Err(_)` on I/O or other errors
    pub fn try_acquire(vexus_dir: &Path) -> Result<Option<WriterLock>> {
        let lock_path = vexus_dir.join(".vexus/lock");

        // Ensure .vexus directory exists. If it's already a file (shouldn't happen
        // in normal use, but happens in some tests), the lock file open below will fail.
        let vexus_dir_path = lock_path.parent().unwrap();
        match std::fs::create_dir_all(vexus_dir_path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // Directory already exists; that's fine.
            }
            Err(e) => {
                // Some other error; we can't create the directory.
                return Err(e).with_context(|| {
                    format!(
                        "failed to create .vexus directory at {}",
                        vexus_dir_path.display()
                    )
                });
            }
        }

        match LockGuard::try_acquire(&lock_path)? {
            Some(inner) => Ok(Some(WriterLock { _inner: inner })),
            None => Ok(None),
        }
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
