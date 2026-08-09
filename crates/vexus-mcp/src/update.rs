//! Release-drift check: tell the user when the installed binary is behind
//! the latest published release.
//!
//! A stale install is invisible — vexus keeps working, just without whatever
//! the newer version fixed, so nothing ever prompts an upgrade.
//!
//! Two hard rules shape the design:
//!
//! 1. **The read path never touches the network.** `status` is called by
//!    agents mid-task and `serve`'s banner runs before the MCP handshake, so
//!    both only read a cache file. Refreshing happens elsewhere: a detached
//!    thread after `serve` is past its handshake-critical section, and
//!    inline at the end of `vexus index` (already a long command).
//! 2. **Failure is silent.** Offline, rate-limited, or behind a proxy is not
//!    an error worth interrupting anyone over — a failed check writes a
//!    fresh timestamp with no version, so it backs off for the full TTL
//!    instead of retrying on every invocation.
//!
//! `VEXUS_NO_UPDATE_CHECK=1` disables both the refresh and the notice.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

const RELEASES_URL: &str = "https://api.github.com/repos/faique43/vexus/releases/latest";
const INSTALL_CMD: &str =
    "curl -fsSL https://raw.githubusercontent.com/faique43/vexus/main/install.sh | sh";
/// How long a check (successful or not) is trusted before another network
/// call is made. Long enough that the unauthenticated GitHub API rate limit
/// (60/hour/IP) is never a consideration.
const TTL_SECS: u64 = 24 * 60 * 60;

fn disabled() -> bool {
    std::env::var_os("VEXUS_NO_UPDATE_CHECK").is_some_and(|v| !v.is_empty() && v != "0")
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// `~/.vexus/update-check.json` — beside the model cache, not in the repo's
/// `.vexus/`: the answer is about the binary, not about any one index.
fn cache_path() -> Option<PathBuf> {
    Some(home_dir()?.join(".vexus").join("update-check.json"))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// What the cache file holds: when the last check ran, and what it found
/// (`None` when it failed — which still counts as "checked" for the TTL).
#[derive(Debug, Clone, PartialEq)]
struct Cached {
    checked_at: u64,
    latest: Option<String>,
}

/// Hand-rolled so a corrupt or partially-written file degrades to "no cache"
/// rather than pulling a JSON parser into the read path.
fn parse_cache(text: &str) -> Option<Cached> {
    let checked_at: u64 = field(text, "checked_at")?.parse().ok()?;
    let latest = field(text, "latest").filter(|v| v != "null" && !v.is_empty());
    Some(Cached { checked_at, latest })
}

/// The value of `"name":` as a bare token — quotes stripped, whitespace
/// trimmed. Enough for the two flat shapes this module reads (its own cache
/// file and GitHub's release JSON) without a parser.
fn field(text: &str, name: &str) -> Option<String> {
    let key = format!("\"{name}\"");
    let after = text.split_once(&key)?.1;
    let after = after.trim_start().strip_prefix(':')?.trim_start();
    if let Some(rest) = after.strip_prefix('"') {
        rest.split('"').next().map(|s| s.to_string())
    } else {
        Some(
            after
                .split([',', '}', '\n'])
                .next()?
                .trim()
                .trim_matches('"')
                .to_string(),
        )
    }
}

fn render_cache(c: &Cached) -> String {
    match &c.latest {
        Some(v) => format!(
            "{{\"checked_at\": {}, \"latest\": \"{}\"}}\n",
            c.checked_at, v
        ),
        None => format!("{{\"checked_at\": {}, \"latest\": null}}\n", c.checked_at),
    }
}

fn read_cache() -> Option<Cached> {
    parse_cache(&std::fs::read_to_string(cache_path()?).ok()?)
}

/// Compare dotted numeric versions component-wise. `None` when either side
/// isn't purely numeric (a pre-release or oddly-shaped tag) — an unknown
/// shape must never produce an upgrade notice.
fn is_newer(latest: &str, current: &str) -> Option<bool> {
    fn parts(v: &str) -> Option<Vec<u64>> {
        v.trim_start_matches('v')
            .split('.')
            .map(|p| p.parse::<u64>().ok())
            .collect()
    }
    let (l, c) = (parts(latest)?, parts(current)?);
    for i in 0..l.len().max(c.len()) {
        let (a, b) = (
            l.get(i).copied().unwrap_or(0),
            c.get(i).copied().unwrap_or(0),
        );
        if a != b {
            return Some(a > b);
        }
    }
    Some(false)
}

/// The one-line notice, or `None` when the install is current, the check is
/// disabled, or nothing has been cached yet. Reads only the cache file —
/// never the network.
pub fn notice() -> Option<String> {
    if disabled() {
        return None;
    }
    let latest = read_cache()?.latest?;
    let current = env!("CARGO_PKG_VERSION");
    if !is_newer(&latest, current)? {
        return None;
    }
    Some(format!(
        "update: vexus {latest} available (installed {current}) — {INSTALL_CMD}"
    ))
}

/// Fetch the latest release tag and rewrite the cache, unless the check is
/// disabled or a previous result is still inside its TTL. Never returns an
/// error: every failure path caches "checked, found nothing" so the next
/// call backs off rather than retrying.
///
/// Callers must invoke this only where a network round-trip is already
/// acceptable — never from `status` or before the MCP handshake.
pub fn refresh_if_stale() {
    if disabled() {
        return;
    }
    let Some(path) = cache_path() else {
        return;
    };
    let now = now_secs();
    if let Some(c) = read_cache() {
        if now.saturating_sub(c.checked_at) < TTL_SECS {
            return;
        }
    }
    let latest = fetch_latest_tag();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(
        &path,
        render_cache(&Cached {
            checked_at: now,
            latest,
        }),
    );
}

fn fetch_latest_tag() -> Option<String> {
    // GitHub rejects requests without a User-Agent.
    let body = ureq::get(RELEASES_URL)
        .header("User-Agent", concat!("vexus/", env!("CARGO_PKG_VERSION")))
        .call()
        .ok()?
        .into_body()
        .read_to_string()
        .ok()?;
    field(&body, "tag_name").map(|t| t.trim_start_matches('v').to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_compare_handles_components_and_widths() {
        assert_eq!(is_newer("0.3.0", "0.2.1"), Some(true));
        assert_eq!(is_newer("0.2.2", "0.2.10"), Some(false));
        assert_eq!(is_newer("1.0", "0.9.9"), Some(true));
        assert_eq!(is_newer("0.2.1", "0.2.1"), Some(false));
        assert_eq!(is_newer("v0.3.0", "0.2.1"), Some(true));
        // Non-numeric shapes must never claim an upgrade.
        assert_eq!(is_newer("0.3.0-rc1", "0.2.1"), None);
        assert_eq!(is_newer("nightly", "0.2.1"), None);
    }

    #[test]
    fn cache_round_trips_and_tolerates_garbage() {
        let with = Cached {
            checked_at: 1_700_000_000,
            latest: Some("0.9.1".into()),
        };
        assert_eq!(parse_cache(&render_cache(&with)), Some(with));

        let without = Cached {
            checked_at: 42,
            latest: None,
        };
        assert_eq!(parse_cache(&render_cache(&without)), Some(without));

        // A truncated or nonsense file reads as "no cache", not a panic.
        assert_eq!(parse_cache("{\"checked_at\": "), None);
        assert_eq!(parse_cache("not json at all"), None);
        assert_eq!(parse_cache(""), None);
    }

    #[test]
    fn tag_name_is_extracted_from_a_release_payload() {
        let body = r#"{"url":"https://api.github.com/x","tag_name":"v1.2.3","name":"v1.2.3"}"#;
        assert_eq!(field(body, "tag_name").as_deref(), Some("v1.2.3"));
    }
}
