//! Token-budgeted bundle packing: greedy selection of items by score until
//! budget exhausted, with deduplication and ordering.

use vexus_core::model::estimate_tokens;

/// A piece of source code or context to include in a bundle.
#[derive(Debug, Clone)]
pub struct BundleItem {
    pub path: String,
    pub qualname: Option<String>,
    pub start_line: u32,
    pub end_line: u32,
    pub content: String,
    pub score: f64,
    pub chunk_id: i64, // -1 for non-chunk sources (file slices)
}

/// One item left out of the packed bundle: its display name (qualname, or
/// path when there's no symbol), its score, and the line range it would
/// have covered — the range travels along so the rendered footer can show
/// *which* occurrence of a name was left out, not just its bare name.
pub type OmittedItem = (String, f64, u32, u32);

/// Greedy fill by score desc until budget_tokens exhausted.
/// Per-item cost = estimate_tokens(content) + 20-token overhead.
/// Dedupes identical chunk_ids (keeps highest score).
/// Returns (selected sorted by (path, start_line), omitted sorted by score
/// desc).
pub fn pack(items: Vec<BundleItem>, budget_tokens: u32) -> (Vec<BundleItem>, Vec<OmittedItem>) {
    if items.is_empty() {
        return (vec![], vec![]);
    }

    // Deduplicate by chunk_id, keeping the highest score for each chunk_id.
    // For chunk_id == -1 (non-chunk sources), keep all items.
    let mut chunk_id_best: std::collections::HashMap<i64, usize> = std::collections::HashMap::new();

    // First pass: find best index for each chunk_id
    for (idx, item) in items.iter().enumerate() {
        if item.chunk_id != -1 {
            chunk_id_best
                .entry(item.chunk_id)
                .and_modify(|best_idx| {
                    if item.score > items[*best_idx].score {
                        *best_idx = idx;
                    }
                })
                .or_insert(idx);
        }
    }

    // Second pass: collect all items that should be kept
    let mut deduped = Vec::new();
    for (idx, item) in items.iter().enumerate() {
        if item.chunk_id == -1 {
            // Non-chunk sources are always kept
            deduped.push(idx);
        } else if chunk_id_best.get(&item.chunk_id) == Some(&idx) {
            // This is the best item for its chunk_id
            deduped.push(idx);
        }
    }

    // Collect the actual items and sort by score descending
    let mut sorted_items: Vec<(usize, &BundleItem)> = deduped
        .iter()
        .copied()
        .map(|idx| (idx, &items[idx]))
        .collect();
    sorted_items.sort_by(|a, b| {
        b.1.score
            .partial_cmp(&a.1.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // Greedily pack items by score. An item that doesn't fit is skipped
    // (`continue`), not treated as end-of-budget (`break`): scores aren't
    // sized, so one big high-score item sitting ahead of several small ones
    // must not starve every smaller item behind it in the sort order.
    let mut selected_indices = std::collections::HashSet::new();
    let mut used_tokens: u32 = 0;
    for &(idx, item) in &sorted_items {
        let item_cost = estimate_tokens(&item.content) + 20;
        if used_tokens + item_cost <= budget_tokens {
            selected_indices.insert(idx);
            used_tokens += item_cost;
        }
    }

    // Guarantee at least one item survives when there's anything to show at
    // all: an empty bundle reads as "nothing relevant found", which is a
    // worse outcome for the caller than one over-budget item they can still
    // read (and know to raise budget_tokens for the rest).
    if selected_indices.is_empty() {
        if let Some(&(idx, _)) = sorted_items.first() {
            selected_indices.insert(idx);
        }
    }

    // Build selected list (sorted by path, then start_line)
    let mut selected_items: Vec<BundleItem> = deduped
        .iter()
        .filter(|&&idx| selected_indices.contains(&idx))
        .map(|&idx| items[idx].clone())
        .collect();
    selected_items.sort_by(|a, b| a.path.cmp(&b.path).then(a.start_line.cmp(&b.start_line)));

    // Build omitted list: items that made it through dedup but weren't selected (budget-exhausted only).
    // Deduplicated losers are omitted entirely and don't appear as "related".
    let mut omitted: Vec<OmittedItem> = deduped
        .iter()
        .filter(|&&idx| !selected_indices.contains(&idx))
        .map(|&idx| {
            let item = &items[idx];
            let name = item
                .qualname
                .as_ref()
                .cloned()
                .unwrap_or_else(|| item.path.clone());
            (name, item.score, item.start_line, item.end_line)
        })
        .collect();
    omitted.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    (selected_items, omitted)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_item(
        path: &str,
        qualname: Option<&str>,
        start_line: u32,
        end_line: u32,
        content: &str,
        score: f64,
        chunk_id: i64,
    ) -> BundleItem {
        BundleItem {
            path: path.to_string(),
            qualname: qualname.map(|s| s.to_string()),
            start_line,
            end_line,
            content: content.to_string(),
            score,
            chunk_id,
        }
    }

    #[test]
    fn pack_budget_math() {
        // Each item is 300 tokens (estimate_tokens returns text.chars().count() / 4).
        // To get 300 tokens: need 300 * 4 = 1200 chars.
        let content_300 = "a".repeat(1200);

        let items = vec![
            make_item("a.rs", Some("aaa"), 1, 10, &content_300, 1.0, 1),
            make_item("b.rs", Some("bbb"), 1, 20, &content_300, 2.0, 2),
            make_item("c.rs", Some("ccc"), 1, 30, &content_300, 3.0, 3),
        ];

        // Budget: 700 tokens
        // First item (score 3.0): 300 + 20 = 320 tokens, used = 320
        // Second item (score 2.0): 300 + 20 = 320 tokens, used = 640
        // Third item (score 1.0): 300 + 20 = 320 tokens, would be 960 > 700, so omitted
        let (selected, omitted) = pack(items, 700);

        assert_eq!(selected.len(), 2);
        assert_eq!(omitted.len(), 1);

        // Selected should be sorted by (path, start_line)
        assert_eq!(selected[0].path, "b.rs");
        assert_eq!(selected[1].path, "c.rs");

        // Omitted should have the lower-score item
        assert_eq!(omitted[0].0, "aaa");
        assert_eq!(omitted[0].1, 1.0);
    }

    #[test]
    fn pack_dedupes_chunk_ids() {
        // Two items with the same chunk_id but different scores
        let items = vec![
            make_item("a.rs", Some("aaa"), 1, 10, "x", 1.0, 1),
            make_item("a.rs", Some("aaa"), 15, 25, "x", 2.0, 1), // Same chunk_id, higher score
        ];

        let (selected, omitted) = pack(items, 10000);

        // Only the higher-score item should be selected.
        // The lower-score duplicate is deduplicated away and should NOT appear in omitted
        // (its content is present via the higher-score copy, so not "related missing").
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].score, 2.0);
        assert!(
            omitted.is_empty(),
            "Deduplicated losers should not appear in omitted"
        );
    }

    #[test]
    fn pack_preserves_ordering() {
        // Multiple items from same path, should be sorted by start_line
        let items = vec![
            make_item("a.rs", Some("func1"), 20, 30, "x", 2.0, 1),
            make_item("a.rs", Some("func2"), 10, 15, "x", 3.0, 2),
            make_item("a.rs", Some("func3"), 40, 50, "x", 1.0, 3),
        ];

        let (selected, _) = pack(items, 10000);

        assert_eq!(selected.len(), 3);
        assert_eq!(selected[0].start_line, 10);
        assert_eq!(selected[1].start_line, 20);
        assert_eq!(selected[2].start_line, 40);
    }

    #[test]
    fn pack_handles_non_chunk_sources() {
        // Items with chunk_id -1 should not be deduplicated
        let items = vec![
            make_item("a.rs", Some("aaa"), 1, 10, "x", 1.0, -1),
            make_item("a.rs", Some("aaa"), 15, 25, "x", 1.0, -1),
        ];

        let (selected, _) = pack(items, 10000);

        // Both should be kept since chunk_id is -1
        assert_eq!(selected.len(), 2);
    }

    #[test]
    fn pack_empty() {
        let (selected, omitted) = pack(vec![], 700);
        assert_eq!(selected.len(), 0);
        assert_eq!(omitted.len(), 0);
    }

    /// Regression: a `break` on the first over-budget item (in score order)
    /// used to stop packing entirely, starving every smaller item behind
    /// it — even when several of them would comfortably fit. `pack` must
    /// skip past the oversized item and keep trying the rest.
    #[test]
    fn pack_skips_over_budget_item_instead_of_stopping() {
        // Item costs: 320 tokens each for "small" (80 chars -> 20 + 20
        // overhead), and a big 2020-token item (8000 chars -> 2000 + 20).
        let small = "a".repeat(80); // 20 + 20 = 40 tokens
        let big = "a".repeat(8000); // 2000 + 20 = 2020 tokens

        let items = vec![
            // Highest score, but alone blows almost the whole budget.
            make_item("big.rs", Some("big"), 1, 10, &big, 10.0, 1),
            make_item("a.rs", Some("small_a"), 1, 10, &small, 5.0, 2),
            make_item("b.rs", Some("small_b"), 1, 10, &small, 4.0, 3),
        ];

        // Budget fits both small items (80 tokens) but not the big one on
        // top of them.
        let (selected, omitted) = pack(items, 100);

        assert_eq!(
            selected.len(),
            2,
            "both small items must be packed even though a higher-score item didn't fit: {selected:?}"
        );
        assert!(selected.iter().any(|i| i.path == "a.rs"));
        assert!(selected.iter().any(|i| i.path == "b.rs"));
        assert_eq!(omitted.len(), 1);
        assert_eq!(omitted[0].0, "big");
    }

    /// Regression: an empty selection reads as "nothing relevant" to a
    /// caller, which is worse than showing one item that happens to blow
    /// the budget. The single highest-score item must always survive when
    /// there's anything to pack at all.
    #[test]
    fn pack_guarantees_at_least_one_item_even_when_over_budget() {
        let huge = "a".repeat(40_000); // 10_000 + 20 tokens, way over any small budget.
        let items = vec![make_item("huge.rs", Some("huge"), 1, 10, &huge, 1.0, 1)];

        let (selected, omitted) = pack(items, 50);

        assert_eq!(
            selected.len(),
            1,
            "the only item must still be selected despite blowing the budget"
        );
        assert_eq!(selected[0].path, "huge.rs");
        assert!(omitted.is_empty());
    }
}
