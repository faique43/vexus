//! Freshness state machine: persists the watcher/reconcile/index lifecycle
//! state in `meta('freshness')` so any reader connection (in particular
//! `vexus-mcp`'s tool handlers, which never see the writer's in-memory
//! state directly) can answer "is the index caught up with disk?" from the
//! store alone.
//!
//! State transitions themselves (spec §6 table — who calls `set_freshness`
//! with what, and when) belong to later tasks (the watcher, reconcile, and
//! advisory-lock modules); this module only owns the enum, its
//! string encoding, and the read/write/derive primitives every one of those
//! callers (and every MCP tool reader) shares.

use anyhow::Result;
use vexus_core::Store;

/// A `Degraded` state older than this (per `meta('freshness_since')`, an
/// unauthenticated unix-epoch second count) is reported as `Stale` instead —
/// "the watcher broke a while ago and nothing has fixed it" is a stronger
/// warning than a `Degraded` blip that might resolve on its own within the
/// next reconcile cycle.
const DEGRADED_TO_STALE_AFTER_SECS: u64 = 5 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    Fresh,
    Indexing,
    Reconciling,
    Degraded,
    Stale,
}

impl Freshness {
    pub fn as_str(self) -> &'static str {
        match self {
            Freshness::Fresh => "fresh",
            Freshness::Indexing => "indexing",
            Freshness::Reconciling => "reconciling",
            Freshness::Degraded => "degraded",
            Freshness::Stale => "stale",
        }
    }

    /// Unknown/corrupt values (e.g. a DB written by a future version with a
    /// state this build doesn't know) parse as `Degraded` — the safe,
    /// warn-loudly default — rather than silently treating an unrecognized
    /// string as `Fresh`.
    pub fn parse(s: &str) -> Self {
        match s {
            "fresh" => Freshness::Fresh,
            "indexing" => Freshness::Indexing,
            "reconciling" => Freshness::Reconciling,
            "degraded" => Freshness::Degraded,
            "stale" => Freshness::Stale,
            _ => Freshness::Degraded,
        }
    }
}

fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Persists `f` as `meta('freshness')`. `meta('freshness_since')` is
/// (re)stamped with the current unix-epoch second only when this call
/// actually *changes* the persisted state — including the first call ever
/// (nothing persisted yet) — so it tracks "when did the state last change"
/// rather than "when was `set_freshness` last called with this state".
///
/// Finding I2: an earlier version restamped `since` unconditionally, on
/// every call regardless of whether the state actually moved. The watcher's
/// `mark_degraded` calls `set_freshness(Degraded)` on every unrecoverable
/// error, including repeatedly while already `Degraded` (e.g. a run of
/// `notify` errors); restamping `since` on each of those kept resetting "how
/// long has this been Degraded" back to zero, so `effective_freshness`'s
/// Degraded -> Stale escalation could never actually fire no matter how
/// long the real outage lasted.
pub fn set_freshness(store: &mut Store, f: Freshness) -> Result<()> {
    let changed = store.meta("freshness")?.as_deref() != Some(f.as_str());
    store.set_meta("freshness", f.as_str())?;
    if changed {
        store.set_meta("freshness_since", &now_unix_secs().to_string())?;
    }
    Ok(())
}

/// Reads the raw persisted state. Absent `meta('freshness')` — a DB written
/// before this field existed, or a brand-new store nothing has touched yet —
/// reads as `Fresh`, so pre-Plan-4 databases (and any tool call before the
/// first `set_freshness`) don't spuriously show a warning header.
pub fn get_freshness(store: &Store) -> Result<Freshness> {
    Ok(match store.meta("freshness")? {
        Some(v) => Freshness::parse(&v),
        None => Freshness::Fresh,
    })
}

/// The state tool responses/status should actually report: `get_freshness`,
/// except a `Degraded` state that has persisted past
/// `DEGRADED_TO_STALE_AFTER_SECS` escalates to `Stale`. A missing or
/// unparseable `freshness_since` is treated as "not old enough yet" (0
/// elapsed) rather than escalating — the timestamp is always written
/// alongside the state by `set_freshness`, so its absence means the DB
/// predates this field entirely, in which case `get_freshness` already
/// returns `Fresh` and this branch never runs.
pub fn effective_freshness(store: &Store) -> Result<Freshness> {
    let state = get_freshness(store)?;
    if state != Freshness::Degraded {
        return Ok(state);
    }
    let since: u64 = store
        .meta("freshness_since")?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let elapsed = now_unix_secs().saturating_sub(since);
    if elapsed > DEGRADED_TO_STALE_AFTER_SECS {
        Ok(Freshness::Stale)
    } else {
        Ok(Freshness::Degraded)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_store(dir: &std::path::Path) -> Store {
        Store::open(&dir.join(".vexus/index.db")).unwrap()
    }

    #[test]
    fn as_str_and_parse_round_trip_for_every_state() {
        for f in [
            Freshness::Fresh,
            Freshness::Indexing,
            Freshness::Reconciling,
            Freshness::Degraded,
            Freshness::Stale,
        ] {
            assert_eq!(Freshness::parse(f.as_str()), f, "round trip for {f:?}");
        }
    }

    #[test]
    fn parse_unknown_string_is_degraded() {
        assert_eq!(Freshness::parse("banana"), Freshness::Degraded);
        assert_eq!(Freshness::parse(""), Freshness::Degraded);
    }

    #[test]
    fn set_then_get_round_trips_for_every_state() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open_store(dir.path());

        for f in [
            Freshness::Fresh,
            Freshness::Indexing,
            Freshness::Reconciling,
            Freshness::Degraded,
            Freshness::Stale,
        ] {
            set_freshness(&mut store, f).unwrap();
            assert_eq!(get_freshness(&store).unwrap(), f, "round trip for {f:?}");
        }
    }

    #[test]
    fn absent_freshness_key_reads_as_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let store = open_store(dir.path());
        assert_eq!(
            get_freshness(&store).unwrap(),
            Freshness::Fresh,
            "a store nothing has ever called set_freshness on (or a pre-Plan-4 DB) must read Fresh"
        );
        assert_eq!(effective_freshness(&store).unwrap(), Freshness::Fresh);
    }

    #[test]
    fn set_freshness_writes_a_since_timestamp() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open_store(dir.path());
        assert_eq!(store.meta("freshness_since").unwrap(), None);

        set_freshness(&mut store, Freshness::Indexing).unwrap();

        let since = store.meta("freshness_since").unwrap();
        assert!(since.is_some(), "expected freshness_since to be stamped");
        // Sane unix-epoch-seconds value: after 2020-01-01, before some far
        // future date — guards against e.g. accidentally storing millis.
        let since: u64 = since.unwrap().parse().unwrap();
        assert!(
            since > 1_577_836_800,
            "looks too small to be unix seconds: {since}"
        );
        assert!(
            since < 4_000_000_000,
            "looks too large to be unix seconds: {since}"
        );
    }

    #[test]
    fn degraded_within_five_minutes_stays_degraded() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open_store(dir.path());
        set_freshness(&mut store, Freshness::Degraded).unwrap();
        // Backdate to just under the 5-minute threshold.
        let since = now_unix_secs() - 299;
        store
            .set_meta("freshness_since", &since.to_string())
            .unwrap();

        assert_eq!(effective_freshness(&store).unwrap(), Freshness::Degraded);
    }

    #[test]
    fn degraded_past_five_minutes_becomes_stale() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open_store(dir.path());
        set_freshness(&mut store, Freshness::Degraded).unwrap();
        // Backdate freshness_since directly (as a watcher outage would leave
        // it) well past the 5-minute threshold.
        let since = now_unix_secs() - 3600;
        store
            .set_meta("freshness_since", &since.to_string())
            .unwrap();

        assert_eq!(effective_freshness(&store).unwrap(), Freshness::Stale);
    }

    #[test]
    fn non_degraded_states_never_escalate_to_stale_regardless_of_age() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open_store(dir.path());
        for f in [
            Freshness::Fresh,
            Freshness::Indexing,
            Freshness::Reconciling,
        ] {
            set_freshness(&mut store, f).unwrap();
            let since = now_unix_secs() - 100_000;
            store
                .set_meta("freshness_since", &since.to_string())
                .unwrap();
            assert_eq!(
                effective_freshness(&store).unwrap(),
                f,
                "{f:?} must never escalate to Stale via age"
            );
        }
    }
}
