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

/// Greedy fill by score desc until budget_tokens exhausted.
/// Per-item cost = estimate_tokens(content) + 20-token overhead.
/// Dedupes identical chunk_ids (keeps highest score).
/// Returns (selected sorted by (path, start_line), omitted as (qualname/path, score) sorted by score desc).
pub fn pack(items: Vec<BundleItem>, budget_tokens: u32) -> (Vec<BundleItem>, Vec<(String, f64)>) {
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

    // Greedily pack items by score
    let mut selected_indices = std::collections::HashSet::new();
    let mut remaining: Vec<(usize, &BundleItem)> = sorted_items;
    let mut used_tokens: u32 = 0;

    while !remaining.is_empty() {
        let item = remaining[0].1;
        let item_cost = estimate_tokens(&item.content) + 20;
        if used_tokens + item_cost <= budget_tokens {
            let (idx, _) = remaining.remove(0);
            selected_indices.insert(idx);
            used_tokens += item_cost;
        } else {
            // Budget exhausted
            break;
        }
    }

    // Build selected list (sorted by path, then start_line)
    let mut selected_items: Vec<BundleItem> = deduped
        .iter()
        .filter(|&&idx| selected_indices.contains(&idx))
        .map(|&idx| items[idx].clone())
        .collect();
    selected_items.sort_by(|a, b| a.path.cmp(&b.path).then(a.start_line.cmp(&b.start_line)));

    // Build omitted list: all items not in selected, sorted by score descending
    let mut omitted: Vec<(String, f64)> = items
        .iter()
        .enumerate()
        .filter(|(idx, _)| !selected_indices.contains(idx))
        .map(|(_, item)| {
            let name = item
                .qualname
                .as_ref()
                .cloned()
                .unwrap_or_else(|| item.path.clone());
            (name, item.score)
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

        // Only the higher-score item should be selected
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].score, 2.0);
        assert_eq!(omitted.len(), 1);
        assert_eq!(omitted[0].1, 1.0);
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
}
