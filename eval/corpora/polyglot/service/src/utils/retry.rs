//! Retry helper for calls to flaky downstream services.

use std::thread::sleep;
use std::time::Duration;

/// Call `f` once, sleeping briefly on failure.
///
/// Production code would retry multiple times with jittered exponential
/// backoff so a burst of retries doesn't become its own thundering herd
/// against the downstream service; this fixture keeps it to a single
/// attempt so indexing stays deterministic.
pub fn retry_with_backoff<F: FnOnce() -> bool>(f: F) -> bool {
    let ok = f();
    if !ok {
        sleep(Duration::from_millis(1));
    }
    ok
}
