//! Pure debounce logic for the file watcher: no threads, no I/O, no wall
//! clock — every `Instant` is supplied by the caller, so this module is
//! fully unit-testable with a synthetic clock (just `Instant::now() +
//! Duration::from_millis(n)` arithmetic, never an actual sleep).
//!
//! The watcher (`crate::watcher`) feeds raw filesystem events in as they
//! arrive via `push`, then on each poll tick asks `drain_ready` for whatever
//! paths have gone quiet for at least [`DEBOUNCE_WINDOW`] — coalescing a
//! burst of saves (editors often write a file 2-3 times per "save", plus
//! temp-file renames) into a single `update_file` call per path.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// How long a path must go quiet (no new event) before `drain_ready` will
/// emit it. Fixed per the spec rather than configurable in production; tests
/// still exercise the exact boundary via injected `Instant`s.
pub const DEBOUNCE_WINDOW: Duration = Duration::from_millis(500);

/// Tracks pending paths awaiting their debounce window. A path is "pending"
/// from its first `push` until a `drain_ready` call observes its most recent
/// event is at least `window` old, at which point it's emitted and forgotten
/// (a later `push` starts it pending again from scratch).
pub struct Debouncer {
    window: Duration,
    /// Unique pending paths in first-seen (arrival) order. A repeat `push`
    /// of an already-pending path does *not* move it — only its entry in
    /// `last_event` changes — so `drain_ready` emits ready paths in the
    /// order they first arrived, not the order their timers last reset.
    order: VecDeque<PathBuf>,
    last_event: HashMap<PathBuf, Instant>,
}

impl Default for Debouncer {
    fn default() -> Self {
        Self::new(DEBOUNCE_WINDOW)
    }
}

impl Debouncer {
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            order: VecDeque::new(),
            last_event: HashMap::new(),
        }
    }

    /// Record an event for `path` at `now`. If `path` isn't already pending
    /// it's appended to the arrival-order queue; either way its last-event
    /// timestamp is (re)set to `now`, restarting its debounce window.
    pub fn push(&mut self, path: PathBuf, now: Instant) {
        if !self.last_event.contains_key(&path) {
            self.order.push_back(path.clone());
        }
        self.last_event.insert(path, now);
    }

    /// Remove and return every pending path whose last event is `>= window`
    /// old as of `now`, in arrival order. Paths not yet ready stay pending,
    /// preserving their relative order for the next call — so a still-warm
    /// path never blocks an unrelated, already-ready path behind it in the
    /// queue.
    pub fn drain_ready(&mut self, now: Instant) -> Vec<PathBuf> {
        let mut ready = Vec::new();
        let mut still_pending = VecDeque::with_capacity(self.order.len());
        while let Some(path) = self.order.pop_front() {
            let last = *self
                .last_event
                .get(&path)
                .expect("every queued path has a last_event entry");
            if now.duration_since(last) >= self.window {
                self.last_event.remove(&path);
                ready.push(path);
            } else {
                still_pending.push_back(path);
            }
        }
        self.order = still_pending;
        ready
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn burst_of_five_same_path_dedupes_to_one_drain_after_window() {
        let mut d = Debouncer::new(Duration::from_millis(500));
        let t0 = Instant::now();
        let path = PathBuf::from("a.py");

        // A rapid burst of 5 events for the same path, all effectively at t0.
        for _ in 0..5 {
            d.push(path.clone(), t0);
        }

        // Not yet ready just under the window.
        assert_eq!(
            d.drain_ready(t0 + Duration::from_millis(499)),
            Vec::<PathBuf>::new()
        );

        // Ready once the window has elapsed, and only a single entry despite
        // 5 pushes.
        let drained = d.drain_ready(t0 + Duration::from_millis(500));
        assert_eq!(drained, vec![path]);

        // Nothing left pending.
        assert_eq!(
            d.drain_ready(t0 + Duration::from_millis(1000)),
            Vec::<PathBuf>::new()
        );
    }

    #[test]
    fn interleaved_paths_drain_in_arrival_order() {
        let mut d = Debouncer::new(Duration::from_millis(500));
        let t0 = Instant::now();
        let a = PathBuf::from("a.py");
        let b = PathBuf::from("b.py");
        let c = PathBuf::from("c.py");

        d.push(a.clone(), t0);
        d.push(b.clone(), t0 + Duration::from_millis(50));
        // A repeat event for `a` arrives after `b`'s first event but must not
        // move `a`'s position in arrival order — only reset its timer.
        d.push(a.clone(), t0 + Duration::from_millis(100));
        d.push(c.clone(), t0 + Duration::from_millis(150));

        // At t0+700ms every path's last event is well past the 500ms window
        // (the latest, c's at t0+150, is ready at t0+650).
        let drained = d.drain_ready(t0 + Duration::from_millis(700));
        assert_eq!(drained, vec![a, b, c], "must drain in first-seen order");
    }

    #[test]
    fn event_during_pending_window_resets_the_timer() {
        let mut d = Debouncer::new(Duration::from_millis(500));
        let t0 = Instant::now();
        let path = PathBuf::from("a.py");

        d.push(path.clone(), t0);
        // Not ready yet at t0+300ms.
        assert_eq!(
            d.drain_ready(t0 + Duration::from_millis(300)),
            Vec::<PathBuf>::new()
        );

        // A second event arrives during the pending window, resetting the
        // clock to t0+300ms.
        d.push(path.clone(), t0 + Duration::from_millis(300));

        // Without the reset this would be ready (t0+600ms is 600ms after the
        // *original* event); with the reset it's only 300ms past the new
        // last-event time, so it must still be pending.
        assert_eq!(
            d.drain_ready(t0 + Duration::from_millis(600)),
            Vec::<PathBuf>::new(),
            "a repeat event mid-window must push the ready time out"
        );

        // 500ms after the *reset* timestamp (t0+300ms), it's finally ready.
        let drained = d.drain_ready(t0 + Duration::from_millis(800));
        assert_eq!(drained, vec![path]);
    }

    #[test]
    fn not_yet_ready_path_does_not_block_a_ready_path_behind_it() {
        let mut d = Debouncer::new(Duration::from_millis(500));
        let t0 = Instant::now();
        let a = PathBuf::from("a.py");
        let b = PathBuf::from("b.py");

        d.push(a.clone(), t0);
        d.push(b.clone(), t0 + Duration::from_millis(100));
        // Refresh `a` so it's no longer ready at the same time `b` becomes
        // ready, while `a` stays ahead of `b` in arrival order.
        d.push(a.clone(), t0 + Duration::from_millis(400));

        // At t0+600ms: b (last event t0+100) is ready; a (last event
        // t0+400) is not.
        let drained = d.drain_ready(t0 + Duration::from_millis(600));
        assert_eq!(drained, vec![b], "only the ready path should drain");

        // a becomes ready later, and is still returned once it is.
        let drained = d.drain_ready(t0 + Duration::from_millis(900));
        assert_eq!(drained, vec![a]);
    }
}
