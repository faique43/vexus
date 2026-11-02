//! From a raw user prompt to a few symbol-level locations.
//!
//! Agents skip optional search tools, so hints reach them through a prompt
//! hook instead, and a raw prompt isn't a search query: an issue report is
//! mostly prose, template headings and pasted code. Two things beat searching
//! the text as it stands. A name the prompt spells out resolves exactly, and
//! what's left searches better without the words every report contains. On
//! 48 real prompts the two took the right file's top-3 rate from 32 to 41.

use std::collections::HashSet;

use anyhow::Result;

use crate::priors::{is_non_core_path, is_non_core_word, is_test_path};
use crate::search::is_stopword;
use crate::Store;

/// Long prompts wander, so only their earliest terms get searched: reports
/// open with the symptom. 20 beat 40 and 80 on the same 48 prompts.
const MAX_QUERY_TERMS: usize = 20;
/// A name with more definitions than this says little about which one's meant.
const MAX_NAMED_MATCHES: usize = 2;
const SEARCH_CANDIDATES: u32 = 40;
const MIN_TERM_CHARS: usize = 3;

/// The vocabulary of issue templates and of any bug report, which a code search
/// learns nothing from on top of `is_stopword`'s words. Search can't share this
/// one: outside a report, `class` and `function` say what's being looked for.
const REPORT_WORDS: &[&str] = &[
    "actual",
    "add",
    "added",
    "adding",
    "already",
    "always",
    "behavior",
    "behaviour",
    "bug",
    "call",
    "called",
    "calling",
    "calls",
    "change",
    "changed",
    "changing",
    "class",
    "code",
    "current",
    "environment",
    "existing",
    "expected",
    "fix",
    "fixed",
    "fixes",
    "function",
    "get",
    "gets",
    "got",
    "instead",
    "issue",
    "issues",
    "like",
    "made",
    "make",
    "makes",
    "method",
    "minimum",
    "need",
    "needs",
    "never",
    "new",
    "node",
    "package",
    "packages",
    "please",
    "problem",
    "problems",
    "repo",
    "repository",
    "reproduce",
    "reproduction",
    "return",
    "returned",
    "returns",
    "search",
    "steps",
    "thanks",
    "typescript",
    "use",
    "used",
    "uses",
    "using",
    "value",
    "values",
    "version",
    "versions",
    "want",
];

/// A symbol worth looking at for a prompt.
#[derive(Debug, Clone, PartialEq)]
pub struct Hint {
    pub path: String,
    pub qualname: String,
    /// The definition's opening line, to find it again after an edit.
    pub sig: Option<String>,
    pub start_line: u32,
    pub end_line: u32,
    /// The prompt names this symbol, rather than search suggesting it.
    pub named: bool,
}

fn looks_like_code(word: &str) -> bool {
    if word.chars().count() < MIN_TERM_CHARS {
        return false;
    }
    let camel =
        word.chars().skip(1).any(char::is_uppercase) && word.chars().any(char::is_lowercase);
    let dotted = word.contains('.') && word.split('.').all(|part| !part.is_empty());
    camel || dotted || word.contains('_')
}

fn trim_to_word(word: &str) -> &str {
    word.trim_matches(|c: char| !c.is_alphanumeric() && c != '_')
}

/// Words in the prompt that look like code rather than prose: anything in
/// backticks, plus camelCase, snake_case and dotted names outside them.
pub fn code_terms(prompt: &str) -> Vec<String> {
    let in_backticks = prompt
        .split('`')
        .skip(1)
        .step_by(2)
        .flat_map(|span| span.split(|c: char| !c.is_alphanumeric() && c != '_' && c != '.'))
        .map(trim_to_word)
        .filter(|word| word.chars().count() >= MIN_TERM_CHARS);
    let spelled_out = prompt
        .split(|c: char| c.is_whitespace() || "()[]{}<>,;:\"'`=/@#*!?|".contains(c))
        .map(trim_to_word)
        .filter(|word| looks_like_code(word));
    let mut seen = HashSet::new();
    let mut terms: Vec<(usize, String)> = spelled_out
        .chain(in_backticks)
        .filter(|word| seen.insert(word.to_string()))
        .map(|word| (prompt.find(word).unwrap_or(usize::MAX), word.to_string()))
        .collect();
    terms.sort_by_key(|(position, _)| *position);
    terms.into_iter().map(|(_, word)| word).collect()
}

/// The prompt reduced to the words worth searching for: no fenced code, noise
/// words or repeats, and only the earliest terms of a long prompt. The names
/// of non-core directories go too: reports use them in passing, and search
/// would take them as asking for that code.
pub fn focus_query(prompt: &str) -> String {
    let mut in_fence = false;
    let mut seen = HashSet::new();
    let mut terms = Vec::new();
    for line in prompt.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        for word in line.split(|c: char| !c.is_alphanumeric() && c != '_') {
            let lower = word.to_ascii_lowercase();
            if word.chars().count() >= MIN_TERM_CHARS
                && !is_stopword(&lower)
                && REPORT_WORDS.binary_search(&lower.as_str()).is_err()
                && !is_non_core_word(&lower)
                && seen.insert(lower)
            {
                terms.push(word);
            }
        }
    }
    terms.truncate(MAX_QUERY_TERMS);
    terms.join(" ")
}

impl Store {
    /// Definitions of `term` in the project's own code, if there are few
    /// enough to take it as naming them.
    fn named_hints(&self, term: &str) -> Result<Vec<Hint>> {
        let mut parts = term.rsplit('.');
        let name = parts.next().unwrap_or(term);
        let qualifier = parts.next().map(str::to_ascii_lowercase);
        let mut stmt = self.conn.prepare_cached(
            "SELECT f.path, s.qualname, s.sig, s.start_line, s.end_line
             FROM symbols s JOIN files f ON f.id = s.file_id
             WHERE s.name = ?1 AND s.kind != 'module'",
        )?;
        let hints: Vec<Hint> = stmt
            .query_map([name], |r| {
                Ok(Hint {
                    path: r.get(0)?,
                    qualname: r.get(1)?,
                    sig: r.get(2)?,
                    start_line: r.get(3)?,
                    end_line: r.get(4)?,
                    named: true,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .filter(|h| !is_non_core_path(&h.path) && !is_test_path(&h.path))
            .filter(|h| {
                qualifier
                    .as_ref()
                    .is_none_or(|q| h.qualname.to_ascii_lowercase().contains(q.as_str()))
            })
            .collect();
        if hints.len() > MAX_NAMED_MATCHES {
            return Ok(Vec::new());
        }
        Ok(hints)
    }

    /// `(start, end, sig)` of a symbol, unless it's the file's module symbol:
    /// a hint pointing at a whole file says less than the chunk that matched.
    fn definition(&self, symbol_id: i64) -> Option<(u32, u32, Option<String>)> {
        self.conn
            .query_row(
                "SELECT start_line, end_line, sig FROM symbols WHERE id = ?1 AND kind != 'module'",
                [symbol_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .ok()
    }

    fn search_hints(
        &self,
        prompt: &str,
        query_vec: Option<&[f32]>,
        knn_floor: Option<f64>,
    ) -> Result<Vec<Hint>> {
        let (hits, _) = self.search_hybrid_scored(
            &focus_query(prompt),
            query_vec,
            knn_floor,
            SEARCH_CANDIDATES,
        )?;
        Ok(hits
            .into_iter()
            .filter_map(|hit| {
                let qualname = hit.qualname?;
                let (start_line, end_line, sig) = hit
                    .symbol_id
                    .and_then(|id| self.definition(id))
                    .unwrap_or((hit.start_line, hit.end_line, None));
                Some(Hint {
                    path: hit.path,
                    qualname,
                    sig,
                    start_line,
                    end_line,
                    named: false,
                })
            })
            .collect())
    }

    /// The best locations for `prompt`, symbols it names first. `query_vec`
    /// and `knn_floor` are as for `search_hybrid_scored`.
    pub fn locate(
        &self,
        prompt: &str,
        query_vec: Option<&[f32]>,
        knn_floor: Option<f64>,
        limit: usize,
    ) -> Result<Vec<Hint>> {
        let mut hints = Vec::new();
        for term in code_terms(prompt) {
            hints.extend(self.named_hints(&term)?);
        }
        hints.extend(self.search_hints(prompt, query_vec, knn_floor)?);
        let mut seen = HashSet::new();
        hints.retain(|h| seen.insert(h.qualname.clone()));
        hints.truncate(limit);
        Ok(hints)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::*;

    fn symbol_file(qualname: &str, body: &str) -> FileIndex {
        let filler = "the quick brown fox jumps over the lazy dog and keeps on running ";
        FileIndex {
            symbols: vec![NewSymbol {
                name: qualname.rsplit('.').next().unwrap().into(),
                qualname: qualname.into(),
                kind: SymbolKind::Function,
                sig: None,
                start_line: 10,
                end_line: 40,
                parent: None,
                arity: Some(0),
            }],
            edges: vec![],
            chunks: vec![NewChunk {
                is_test: false,
                symbol: Some(0),
                start_line: 12,
                end_line: 20,
                content: format!("def f():\n    # {body}\n    {filler}{filler}{filler}\n"),
            }],
        }
    }

    fn store_with(files: &[(&str, &str, &str)]) -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("index.db")).unwrap();
        for (i, (path, qualname, body)) in files.iter().enumerate() {
            store
                .replace_file(path, "python", &[i as u8; 32], &symbol_file(qualname, body))
                .unwrap();
        }
        (dir, store)
    }

    #[test]
    fn code_terms_keep_identifiers_and_drop_prose() {
        let terms =
            code_terms("Fix ClientNats.publish and parse_enum in `handleClose`, not Hello.");
        assert_eq!(terms, ["ClientNats.publish", "parse_enum", "handleClose"]);
    }

    #[test]
    fn code_terms_keep_a_plain_word_in_backticks() {
        assert_eq!(code_terms("the `listen` callback runs twice"), ["listen"]);
    }

    #[test]
    fn focus_query_drops_noise_and_repeats() {
        let q = focus_query("The Kafka client should not hang\n```\nsecret_in_fence()\n```\nkafka");
        assert_eq!(q, "Kafka client hang");
    }

    /// Search suspends its non-core penalty for these words, and in a report
    /// they're how people talk, not a request to look at the samples.
    #[test]
    fn a_report_saying_for_example_still_ranks_samples_last() {
        let (_d, store) = store_with(&[
            (
                "sample/a.py",
                "sample.a.widget",
                "frobnicate frobnicate frobnicate",
            ),
            ("src/b.py", "src.b.widget", "frobnicate"),
        ]);
        for prompt in ["frobnicate, for example", "frobnicate in the integration"] {
            let hints = store.locate(prompt, None, None, 5).unwrap();
            assert_eq!(hints[0].path, "src/b.py", "{prompt}: {hints:?}");
        }
    }

    #[test]
    fn report_words_stay_sorted_and_apart_from_stopwords() {
        assert!(REPORT_WORDS.windows(2).all(|w| w[0] < w[1]));
        assert!(!REPORT_WORDS.iter().any(|w| is_stopword(w)));
    }

    #[test]
    fn focus_query_keeps_the_earliest_terms_of_a_long_prompt() {
        let prompt = (0..50)
            .map(|i| format!("term{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let q = focus_query(&prompt);
        assert_eq!(q.split(' ').count(), 20);
        assert!(q.starts_with("term0 term1"));
    }

    #[test]
    fn a_named_symbol_comes_first_with_its_whole_span() {
        let (_d, store) = store_with(&[
            ("a.py", "a.frobnicate_widget", "unrelated words"),
            ("b.py", "b.other", "frobnicate frobnicate frobnicate"),
        ]);
        let hints = store
            .locate("fix frobnicate_widget please", None, None, 5)
            .unwrap();
        assert_eq!(hints[0].path, "a.py");
        assert!(hints[0].named);
        assert_eq!((hints[0].start_line, hints[0].end_line), (10, 40));
    }

    #[test]
    fn names_found_only_in_samples_or_tests_are_not_treated_as_named() {
        let (_d, store) = store_with(&[
            ("sample/a.py", "a.parse_thing", "alpha"),
            ("tests/test_b.py", "b.parse_thing", "beta"),
        ]);
        let hints = store.locate("look at parse_thing", None, None, 5).unwrap();
        assert!(hints.iter().all(|h| !h.named), "{hints:?}");
    }

    #[test]
    fn a_name_with_many_definitions_is_too_ambiguous_to_call_named() {
        let (_d, store) = store_with(&[
            ("a.py", "a.run_task", "alpha"),
            ("b.py", "b.run_task", "beta"),
            ("c.py", "c.run_task", "gamma"),
        ]);
        let hints = store.locate("the run_task thing", None, None, 5).unwrap();
        assert!(hints.iter().all(|h| !h.named), "{hints:?}");
    }

    #[test]
    fn a_qualifier_picks_between_definitions() {
        let (_d, store) = store_with(&[
            ("a.py", "ClientNats.publish", "alpha"),
            ("b.py", "ClientMqtt.publish", "beta"),
        ]);
        let hints = store
            .locate("ClientNats.publish hangs", None, None, 5)
            .unwrap();
        assert_eq!(hints[0].qualname, "ClientNats.publish");
        assert!(hints[0].named);
    }

    #[test]
    fn a_sample_copy_ranks_below_the_real_code() {
        let (_d, store) = store_with(&[
            (
                "sample/a.py",
                "sample.a.widget",
                "frobnicate frobnicate frobnicate",
            ),
            ("src/b.py", "src.b.widget", "frobnicate"),
        ]);
        let hints = store.locate("frobnicate", None, None, 5).unwrap();
        assert_eq!(hints[0].path, "src/b.py", "{hints:?}");
    }

    #[test]
    fn nothing_indexed_means_no_hints() {
        let (_d, store) = store_with(&[]);
        assert!(store
            .locate("anything at all", None, None, 5)
            .unwrap()
            .is_empty());
    }
}
