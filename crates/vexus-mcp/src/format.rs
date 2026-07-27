//! Text rendering for bundles, candidates, and edge trees.

use vexus_core::query::{EdgeHit, SymbolInfo};

use crate::bundle::{BundleItem, OmittedItem};

/// Find the longest run of consecutive backticks in a string.
/// Used to determine fence size for code blocks that might contain backticks.
fn longest_backtick_run(s: &str) -> usize {
    let mut max_run = 0;
    let mut current_run = 0;
    for ch in s.chars() {
        if ch == '`' {
            current_run += 1;
            max_run = max_run.max(current_run);
        } else {
            current_run = 0;
        }
    }
    max_run
}

/// Render a bundle of selected items with an optional omitted footer.
/// Groups by path (in order of first appearance), adds `## <path>` headers,
/// and per-item shows `<path>:<start>-<end>` with triple-backtick fenced code.
/// Omitted list is capped at 15 `name:start-end` entries, deduped: several
/// omitted chunks that share a qualname (e.g. multiple chunks of the same
/// function) would otherwise render as an uninformative run of repeated bare
/// names — the range disambiguates which occurrence was left out, and exact
/// repeats (same name, same range) collapse to one entry.
pub fn render_bundle(selected: &[BundleItem], omitted: &[OmittedItem]) -> String {
    let mut output = String::new();

    // Group items by path (preserving order of first appearance)
    let mut path_order: Vec<String> = Vec::new();
    let mut path_groups: std::collections::HashMap<String, Vec<&BundleItem>> =
        std::collections::HashMap::new();

    for item in selected {
        if !path_groups.contains_key(&item.path) {
            path_order.push(item.path.clone());
        }
        path_groups.entry(item.path.clone()).or_default().push(item);
    }

    // Render each path group
    for path in path_order {
        let items = &path_groups[&path];

        // Add header
        output.push_str("## ");
        output.push_str(&path);
        output.push('\n');

        // Add each item
        for item in items {
            output.push_str(&format!(
                "{}:{}-{}\n",
                item.path, item.start_line, item.end_line
            ));
            // Use a fence with enough backticks to safely escape the content
            let fence_size = (longest_backtick_run(&item.content) + 1).max(3);
            let fence = "`".repeat(fence_size);
            output.push_str(&fence);
            output.push('\n');
            output.push_str(&item.content);
            if !item.content.ends_with('\n') {
                output.push('\n');
            }
            output.push_str(&fence);
            output.push('\n');
        }
    }

    // Add omitted footer if needed
    if !omitted.is_empty() {
        let mut seen = std::collections::HashSet::new();
        let mut labels: Vec<String> = Vec::new();
        for (name, _score, start, end) in omitted {
            let label = format!("{name}:{start}-{end}");
            if seen.insert(label.clone()) {
                labels.push(label);
            }
        }
        labels.truncate(15);

        output.push_str("Related (not included, raise budget_tokens or use `open`): ");
        output.push_str(&labels.join(", "));
        output.push('\n');
    }

    output
}

/// Render a list of symbol candidates: one per row with qualname, signature, and location.
pub fn render_candidates(candidates: &[SymbolInfo]) -> String {
    let mut output = String::new();

    for candidate in candidates {
        let sig_str = candidate.sig.as_deref().unwrap_or("");
        output.push_str(&format!(
            "{} — {} — {}:{}-{}\n",
            candidate.qualname, sig_str, candidate.path, candidate.start_line, candidate.end_line
        ));
    }

    output
}

/// Render an edge tree: one line per EdgeHit with indent (2 spaces × depth-1),
/// qualname, path:start, and confidence (None → "unresolved").
pub fn render_edge_tree(edges: &[EdgeHit]) -> String {
    let mut output = String::new();

    for edge in edges {
        let indent = " ".repeat(2 * (edge.depth.saturating_sub(1) as usize));
        let confidence = edge.confidence.as_deref().unwrap_or("unresolved");

        // Synthetic endpoints (never resolved to a real symbol row) have no
        // path — rendering their zero location as `(:0)` reads like a bug,
        // so they get name + confidence only.
        if edge.symbol.path.is_empty() {
            output.push_str(&format!(
                "{}{}  [{}]\n",
                indent, edge.symbol.qualname, confidence
            ));
        } else {
            output.push_str(&format!(
                "{}{}  ({}:{})  [{}]\n",
                indent, edge.symbol.qualname, edge.symbol.path, edge.symbol.start_line, confidence
            ));
        }
    }

    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_bundle_item(
        path: &str,
        qualname: Option<&str>,
        start_line: u32,
        end_line: u32,
        content: &str,
    ) -> BundleItem {
        BundleItem {
            path: path.to_string(),
            qualname: qualname.map(|s| s.to_string()),
            start_line,
            end_line,
            content: content.to_string(),
            score: 1.0,
            chunk_id: -1,
        }
    }

    fn make_symbol(
        qualname: &str,
        sig: Option<&str>,
        path: &str,
        start_line: u32,
        end_line: u32,
    ) -> SymbolInfo {
        SymbolInfo {
            id: 1,
            name: "test".to_string(),
            qualname: qualname.to_string(),
            kind: "function".to_string(),
            sig: sig.map(|s| s.to_string()),
            path: path.to_string(),
            start_line,
            end_line,
        }
    }

    fn make_edge(
        qualname: &str,
        path: &str,
        start_line: u32,
        confidence: Option<&str>,
        depth: u32,
    ) -> EdgeHit {
        EdgeHit {
            symbol: SymbolInfo {
                id: 1,
                name: "test".to_string(),
                qualname: qualname.to_string(),
                kind: "function".to_string(),
                sig: None,
                path: path.to_string(),
                start_line,
                end_line: start_line + 10,
            },
            via_name: "test".to_string(),
            confidence: confidence.map(|s| s.to_string()),
            depth,
        }
    }

    #[test]
    fn render_bundle_with_headers_and_fences() {
        let selected = vec![
            make_bundle_item("a.rs", Some("aaa"), 1, 10, "fn a() {}"),
            make_bundle_item("a.rs", Some("aab"), 15, 20, "fn b() {}"),
            make_bundle_item("b.rs", Some("bbb"), 1, 30, "fn c() {}"),
        ];

        let output = render_bundle(&selected, &[]);

        // Check for headers
        assert!(output.contains("## a.rs"));
        assert!(output.contains("## b.rs"));

        // Check for item headers
        assert!(output.contains("a.rs:1-10"));
        assert!(output.contains("a.rs:15-20"));
        assert!(output.contains("b.rs:1-30"));

        // Check for fences
        assert!(output.contains("```\nfn a() {}\n```"));
        assert!(output.contains("```\nfn b() {}\n```"));
        assert!(output.contains("```\nfn c() {}\n```"));
    }

    #[test]
    fn render_bundle_omitted_footer() {
        let selected = vec![make_bundle_item("a.rs", Some("aaa"), 1, 10, "x")];
        let omitted = vec![
            ("func1".to_string(), 0.9, 10, 20),
            ("func2".to_string(), 0.8, 30, 40),
        ];

        let output = render_bundle(&selected, &omitted);

        assert!(output.contains("Related (not included, raise budget_tokens or use `open`): "));
        assert!(output.contains("func1:10-20, func2:30-40"));
    }

    #[test]
    fn render_bundle_omitted_footer_dedupes_same_name_and_range() {
        let selected = vec![];
        // Two chunks of the same function omitted at different ranges must
        // both show up (disambiguated by range); an exact duplicate entry
        // (same name, same range) must collapse to one.
        let omitted = vec![
            ("helper".to_string(), 0.9, 1, 10),
            ("helper".to_string(), 0.8, 20, 30),
            ("helper".to_string(), 0.8, 20, 30),
        ];

        let output = render_bundle(&selected, &omitted);

        assert!(
            output.contains("helper:1-10, helper:20-30"),
            "expected both distinct ranges shown once each: {output:?}"
        );
        let footer_start = output.find("Related").unwrap();
        let footer = &output[footer_start..];
        assert_eq!(
            footer.matches("helper:20-30").count(),
            1,
            "exact duplicate (same name, same range) must be deduped: {output:?}"
        );
    }

    #[test]
    fn render_bundle_omitted_capped_at_15() {
        let selected = vec![];
        let mut omitted = vec![];
        for i in 0..20 {
            omitted.push((format!("func{}", i), 1.0 - (i as f64 * 0.01), i, i + 10));
        }

        let output = render_bundle(&selected, &omitted);

        // With max 15 names capped, we should see exactly 15 items
        // which is 14 commas in the joined string
        let footer_start = output.find("Related").unwrap();
        let footer = &output[footer_start..];
        // Count the names: should be at most 15
        let names_part = footer.split(": ").nth(1).unwrap_or("");
        let name_count = names_part.matches(',').count() + 1; // comma count + 1 = item count
        assert!(
            name_count <= 15,
            "Expected at most 15 names, got {}",
            name_count
        );
    }

    #[test]
    fn render_bundle_escapes_backticks_in_content() {
        // Content containing triple backticks should use a larger fence
        let content_with_backticks = "code:\n```\ninner snippet\n```\nmore code";
        let selected = vec![make_bundle_item(
            "test.rs",
            Some("func"),
            1,
            10,
            content_with_backticks,
        )];

        let output = render_bundle(&selected, &[]);

        // Should use 4-backtick fence (more than the inner 3-backtick run)
        assert!(
            output.contains("````\n"),
            "Should have opening 4-backtick fence"
        );
        assert!(
            output.contains("\n````\n"),
            "Should have closing 4-backtick fence"
        );
        // The inner triple backticks should be intact
        assert!(output.contains("```\ninner snippet\n```"));
    }

    #[test]
    fn render_candidates_format() {
        let candidates = vec![
            make_symbol("foo::bar", Some("fn(i32) -> String"), "foo.rs", 10, 20),
            make_symbol("baz::qux", None, "bar.rs", 5, 15),
        ];

        let output = render_candidates(&candidates);

        assert!(output.contains("foo::bar — fn(i32) -> String — foo.rs:10-20"));
        assert!(output.contains("baz::qux —  — bar.rs:5-15"));
    }

    #[test]
    fn render_edge_tree_indent_and_confidence() {
        let edges = vec![
            make_edge("root", "a.rs", 1, Some("high"), 1),
            make_edge("child1", "b.rs", 2, Some("medium"), 2),
            make_edge("child2", "c.rs", 3, None, 2),
            make_edge("grandchild", "d.rs", 4, Some("low"), 3),
        ];

        let output = render_edge_tree(&edges);

        // Depth 1 → 0 spaces
        assert!(output.contains("root  (a.rs:1)  [high]"));

        // Depth 2 → 2 spaces
        assert!(output.contains("  child1  (b.rs:2)  [medium]"));
        assert!(output.contains("  child2  (c.rs:3)  [unresolved]"));

        // Depth 3 → 4 spaces
        assert!(output.contains("    grandchild  (d.rs:4)  [low]"));
    }
}
