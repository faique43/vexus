//! Structural ranking priors: multiplicative penalties applied to the fused
//! RRF score in `search_hybrid_scored`, so that chunks which are rarely the
//! answer — test code, samples and examples, module import preambles,
//! near-empty fragments — rank below production code that matched equally
//! well.
//!
//! Why multiplicative, and why these magnitudes: adjacent RRF ranks differ
//! by ~1.6% (`1/(60+r)` steps), and appearing in both candidate lists vs one
//! roughly doubles a score. A test factor of 0.4 pushes a rank-1
//! single-list test hit below essentially every single-list production hit
//! in the candidate window while keeping it retrievable; the softer
//! preamble/tiny factors demote by a few ranks without hiding. `1.0`
//! disables a penalty. Penalties stack (an import preamble is typically
//! preamble+tiny).
//!
//! The values below came out of a sweep against the eval corpora's
//! `clean@5` / `answer_in_bundle` ground truth (queries carrying
//! `expect_not`), not out of intuition — change them the same way:
//!
//! - `test`: 0.2, 0.3 and 0.4 score identically; 0.5 loses `clean@5`
//!   (1.0000 -> 0.7500). 0.4 is the least-aggressive value that still
//!   scores perfectly, so it penalizes as little as the ground truth allows.
//! - `preamble`: disabling it (1.0) costs `answer_in_bundle`
//!   (1.0000 -> 0.9655) — the penalty earns its keep. 0.6 edges out 0.7 on
//!   the real embedder, but see below.
//!   Once blank-separated preamble runs merged into one chunk (schema 4)
//!   they stopped being tiny, so the 0.7 x 0.7 they used to earn fell to
//!   0.7 and they outranked real code. A later sweep moved `preamble` to
//!   0.5, about what the two penalties came to. On the mock gate 0.5 scores
//!   the same as anything lower. It has to stay above `test`: at 0.35 test
//!   code passes preambles and the real embedder's clean@5 falls to 0.8333.
//! - `tiny`: 1.0 scores highest on search alone while also costing
//!   `answer_in_bundle`, so the penalty earns its keep too; 0.8 is the best
//!   value that holds `answer_in_bundle` at 1.0000.
//!
//! - `non_core`: the eval corpora have no such directories, so this was swept
//!   on Express and NestJS. 0.15 and 0.3 score the same; at 0.5 sample files
//!   return to NestJS maps, and at 0.7 a sample ranks first and the prompt
//!   hook's file@3 falls 25 -> 23 of 48. 0.3 is the least aggressive value
//!   that holds all of it.
//!
//! `preamble: 0.6, tiny: 0.8` was measured and deliberately NOT taken: on
//! the real embedder it buys recall@5 0.9061 -> 0.9152 and ndcg@10 0.7191
//! -> 0.7239 with mrr flat, but it costs the mock gate 0.0345 of
//! `answer_in_bundle`. Trading a real regression in the blind-mode ratchet
//! that guards every future change for a fractional gain here is a bad
//! bargain, so the values stay where the sweep says they are safest.

/// Multiplicative penalty coefficients. Constructed via [`Priors::from_env`]
/// in production; tests build custom values directly.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Priors {
    /// Applied when the chunk is test code (path pattern or index-time flag).
    pub test: f64,
    /// Applied when the chunk belongs to a module symbol — i.e. it is
    /// module-level preamble (imports, constants between definitions), not
    /// the body of any named definition.
    pub preamble: f64,
    /// Applied when the chunk is below [`Priors::tiny_floor`] tokens — a
    /// 1-line import or fragment whose match can't carry much meaning.
    pub tiny: f64,
    /// Applied when the chunk sits in a non-core directory (samples, examples,
    /// integration apps): they repeat the project's names, so they match as
    /// well as the code they copy.
    pub non_core: f64,
    /// Token threshold for the tiny penalty.
    pub tiny_floor: u32,
}

impl Default for Priors {
    fn default() -> Self {
        Self {
            test: 0.4,
            preamble: 0.5,
            tiny: 0.7,
            non_core: 0.3,
            tiny_floor: 32,
        }
    }
}

impl Priors {
    /// Defaults, each overridable for tuning sweeps:
    /// `VEXUS_PRIOR_TEST`, `VEXUS_PRIOR_PREAMBLE`, `VEXUS_PRIOR_TINY`,
    /// `VEXUS_PRIOR_NON_CORE` (factors, `1.0` disables),
    /// `VEXUS_PRIOR_TINY_FLOOR` (tokens).
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
            non_core: env_f64("VEXUS_PRIOR_NON_CORE", d.non_core),
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
/// code, a false negative just leaves one test chunk unpenalized (Rust's
/// in-file tests are flagged at index time instead).
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

/// Directories of code that illustrates or exercises the project rather than
/// being the project.
const NON_CORE_DIRS: &[&str] = &[
    "benchmark",
    "benchmarks",
    "demo",
    "demos",
    "doc",
    "docs",
    "e2e",
    "example",
    "examples",
    "fixture",
    "fixtures",
    "integration",
    "sample",
    "samples",
    "third_party",
    "vendor",
];

/// Whether `word`, in any case, is the name of a non-core directory.
pub fn is_non_core_word(word: &str) -> bool {
    NON_CORE_DIRS.contains(&word.to_ascii_lowercase().as_str())
}

/// Whether `path` sits in a non-core directory. By directory component, so
/// `src/examples.ts` is real code.
pub fn is_non_core_path(path: &str) -> bool {
    let mut components: Vec<&str> = path.split('/').collect();
    components.pop();
    components.into_iter().any(is_non_core_word)
}

/// Whether the query itself asks about non-core code, in which case the
/// non-core penalty must not apply. "For example" is prose, not such a request.
pub fn query_seeks_non_core(query: &str) -> bool {
    let words: Vec<String> = query
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .filter(|t| !t.is_empty())
        .map(str::to_ascii_lowercase)
        .collect();
    words.iter().enumerate().any(|(i, word)| {
        is_non_core_word(word) && !(word == "example" && i > 0 && words[i - 1] == "for")
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
    fn non_core_goes_by_directory_not_file_name() {
        assert!(is_non_core_path("sample/01-cats/cats.ts"));
        assert!(is_non_core_path("packages/x/examples/demo.py"));
        assert!(is_non_core_path("integration/app.ts"));
        assert!(!is_non_core_path("packages/core/router.ts"));
        assert!(!is_non_core_path("src/examples.ts"));
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
    fn non_core_seeking_queries_bypass() {
        assert!(query_seeks_non_core("show me an example of a guard"));
        assert!(query_seeks_non_core("which samples use websockets"));
        assert!(query_seeks_non_core("the e2e setup for kafka"));
        assert!(!query_seeks_non_core("how does a request reach a handler"));
        assert!(!query_seeks_non_core("sampled traffic")); // substring, not a token
    }

    /// Reports say "for example" all the time and don't mean the directory.
    #[test]
    fn for_example_is_prose() {
        assert!(!query_seeks_non_core("errors, for example a timeout"));
        assert!(!query_seeks_non_core("For example, the broker refuses"));
        assert!(query_seeks_non_core("what is the example for routing"));
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
