//! Structural ranking priors: multiplicative penalties applied to the fused
//! RRF score in `search_hybrid_scored`, so that chunks which are rarely the
//! answer — test code, module import preambles, near-empty fragments — rank
//! below production code that matched equally well.
//!
//! Why multiplicative, and why these magnitudes: adjacent RRF ranks differ
//! by ~1.6% (`1/(60+r)` steps), and appearing in both candidate lists vs one
//! roughly doubles a score. A 0.4 factor therefore pushes a rank-1
//! single-list test hit below essentially every single-list production hit
//! in the candidate window while keeping it retrievable; 0.7 demotes by a
//! few ranks without hiding. `1.0` disables a penalty. Penalties stack
//! (an import preamble is typically preamble+tiny ⇒ 0.49).
//!
//! Tuned against the eval corpora's `clean@5` / `bundle_clean` ground truth
//! (queries carrying `expect_not`) — change coefficients through a sweep
//! against those metrics, not by hand.

/// Multiplicative penalty coefficients. Constructed via [`Priors::from_env`]
/// in production; tests build custom values directly.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Priors {
    /// Applied when the chunk is test code (path pattern or content sniff).
    pub test: f64,
    /// Applied when the chunk belongs to a module symbol — i.e. it is
    /// module-level preamble (imports, constants between definitions), not
    /// the body of any named definition.
    pub preamble: f64,
    /// Applied when the chunk is below [`Priors::tiny_floor`] tokens — a
    /// 1-line import or fragment whose match can't carry much meaning.
    pub tiny: f64,
    /// Token threshold for the tiny penalty.
    pub tiny_floor: u32,
}

impl Default for Priors {
    fn default() -> Self {
        Self {
            test: 0.4,
            preamble: 0.7,
            tiny: 0.7,
            tiny_floor: 32,
        }
    }
}

impl Priors {
    /// Defaults, each overridable for tuning sweeps:
    /// `VEXUS_PRIOR_TEST`, `VEXUS_PRIOR_PREAMBLE`, `VEXUS_PRIOR_TINY`
    /// (factors, `1.0` disables), `VEXUS_PRIOR_TINY_FLOOR` (tokens).
    pub fn from_env() -> Self {
        fn env_f64(name: &str, default: f64) -> f64 {
            std::env::var(name)
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(default)
        }
        let d = Self::default();
        Self {
            test: env_f64("VEXUS_PRIOR_TEST", d.test),
            preamble: env_f64("VEXUS_PRIOR_PREAMBLE", d.preamble),
            tiny: env_f64("VEXUS_PRIOR_TINY", d.tiny),
            tiny_floor: std::env::var("VEXUS_PRIOR_TINY_FLOOR")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(d.tiny_floor),
        }
    }
}

/// Whether `path` (repo-relative, `/`-separated) is test code by naming
/// convention: a test directory component or a test-suffixed/prefixed
/// filename. Deliberately conservative — a false positive penalizes real
/// code, a false negative just leaves one test chunk unpenalized (the
/// content sniff still catches Rust's in-file `#[cfg(test)]` case).
pub fn is_test_path(path: &str) -> bool {
    for component in path.split('/') {
        let c = component.to_ascii_lowercase();
        if matches!(
            c.as_str(),
            "tests" | "test" | "__tests__" | "spec" | "specs"
        ) {
            return true;
        }
    }
    let file = path.rsplit('/').next().unwrap_or(path);
    let lower = file.to_ascii_lowercase();
    if lower.contains(".test.") || lower.contains(".spec.") {
        return true;
    }
    let stem = file.split_once('.').map(|(s, _)| s).unwrap_or(file);
    let lower_stem = stem.to_ascii_lowercase();
    lower_stem.starts_with("test_")
        || lower_stem.ends_with("_test")
        || lower_stem.ends_with("_spec")
        // Java/C# CamelCase conventions — matched case-sensitively so that
        // ordinary words ending in "test" (attest, contest) stay clean.
        || stem.ends_with("Test")
        || stem.ends_with("Tests")
}

/// Whether the query itself is looking for tests — in which case the test
/// penalty must not apply (someone asking "unit tests for invoice creation"
/// wants exactly the chunks the penalty would bury).
pub fn query_seeks_tests(query: &str) -> bool {
    query.split(|c: char| !c.is_ascii_alphanumeric()).any(|t| {
        matches!(
            t.to_ascii_lowercase().as_str(),
            "test" | "tests" | "spec" | "specs" | "testing"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_paths_match_common_conventions() {
        for p in [
            "tests/test_orders.py",
            "src/services/order_test.rs",
            "frontend/src/api/orders.test.ts",
            "frontend/src/api/orders.spec.ts",
            "app/__tests__/cart.js",
            "spec/models/user_spec.rb",
            "src/test/java/OrderServiceTest.java",
            "Project.Tests/OrderTests.cs",
            "pkg/orders/orders_test.go",
            "lib/shop/cart_test.exs",
            "test/integration.rs",
        ] {
            assert!(is_test_path(p), "{p} must be recognized as a test path");
        }
    }

    #[test]
    fn production_paths_do_not_match() {
        for p in [
            "services/invoice_service.py",
            "src/services/order_service.rs",
            "frontend/src/api/orders.ts",
            "src/attest.rs",       // "test" inside a word
            "src/contest/rank.py", // directory containing "test" as substring
            "protests/log.md",
        ] {
            assert!(
                !is_test_path(p),
                "{p} must NOT be recognized as a test path"
            );
        }
    }

    #[test]
    fn test_seeking_queries_bypass() {
        assert!(query_seeks_tests("unit tests for invoice creation"));
        assert!(query_seeks_tests("integration test coverage"));
        assert!(query_seeks_tests("where are the specs for checkout"));
        assert!(!query_seeks_tests("how does an invoice get created"));
        assert!(!query_seeks_tests("attestation flow")); // substring, not a token
    }

    #[test]
    fn env_overrides_apply() {
        // Single env-reading test (kept to one to avoid parallel-test env
        // races); everything else constructs Priors directly.
        std::env::set_var("VEXUS_PRIOR_TEST", "1.0");
        let p = Priors::from_env();
        std::env::remove_var("VEXUS_PRIOR_TEST");
        assert_eq!(p.test, 1.0);
        assert_eq!(p.preamble, Priors::default().preamble);
    }
}
