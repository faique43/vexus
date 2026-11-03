//! Deterministic filler that pushes a fixture corpus past the Medium tier's
//! 2000-chunk boundary, so its labelled queries also run with the parameters
//! real repositories get.
//!
//! The filler borrows the fixtures' domain words, because a haystack nothing
//! matches changes nothing but the tier: these files compete for the same
//! keywords the way unrelated code does in a real repository. Like `perf.rs`,
//! every byte is a pure function of the file index.

use std::path::Path;

use anyhow::{Context, Result};

/// Five chunks a file, which clears the boundary without the fixture's help.
const FILE_COUNT: usize = 420;

/// Coprime lengths, so topic and action pairs don't repeat in step.
const TOPICS: [&str; 16] = [
    "cart",
    "coupon",
    "customer",
    "discount",
    "email",
    "inventory",
    "invoice",
    "order",
    "payment",
    "refund",
    "report",
    "session",
    "subscription",
    "token",
    "user",
    "webhook",
];
const ACTIONS: [&str; 7] = [
    "archive",
    "audit",
    "export",
    "migrate",
    "reconcile",
    "replay",
    "summarize",
];

fn title_case(word: &str) -> String {
    let mut chars = word.chars();
    let first = chars.next().map(|c| c.to_ascii_uppercase());
    first.into_iter().chain(chars).collect()
}

/// `(repo-relative path, content)` of filler file `i`.
fn padding_file(i: usize) -> (String, String) {
    let topic = TOPICS[i % TOPICS.len()];
    let action = ACTIONS[i % ACTIONS.len()];
    let (topic_title, action_title) = (title_case(topic), title_case(action));
    let path = format!("archive/{topic}/{action}_{topic}_{i:04}.py");
    let content = format!(
        "\"\"\"Legacy {action} step for {topic} records, shard {i}.\"\"\"\n\
         \n\
         import logging\n\
         \n\
         \n\
         def {action}_{topic}_{i:04}(record):\n\
         \x20   \"\"\"{action_title} one {topic} record from shard {i}.\"\"\"\n\
         \x20   return load_{topic}_{i:04}(record)\n\
         \n\
         \n\
         def load_{topic}_{i:04}(record):\n\
         \x20   \"\"\"Load the {topic} record kept in shard {i}.\"\"\"\n\
         \x20   return record\n\
         \n\
         \n\
         class {topic_title}Shard{i:04}:\n\
         \x20   \"\"\"Holds the {topic} records of shard {i}.\"\"\"\n\
         \n\
         \x20   def {action}(self, record):\n\
         \x20       \"\"\"{action_title} a {topic} record held by this shard.\"\"\"\n\
         \x20       return {action}_{topic}_{i:04}(record)\n"
    );
    (path, content)
}

/// Writes every filler file under `root`.
pub fn write_padding(root: &Path) -> Result<()> {
    for i in 0..FILE_COUNT {
        let (rel, content) = padding_file(i);
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create_dir_all {}", parent.display()))?;
        }
        std::fs::write(&path, content).with_context(|| format!("write {}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_is_a_pure_function_of_its_index() {
        assert_eq!(padding_file(7), padding_file(7));
        assert_ne!(padding_file(7), padding_file(8));
    }

    #[test]
    fn no_two_files_share_a_path() {
        let mut paths: Vec<String> = (0..FILE_COUNT).map(|i| padding_file(i).0).collect();
        paths.sort();
        paths.dedup();
        assert_eq!(paths.len(), FILE_COUNT);
    }

    #[test]
    fn filler_mentions_the_vocabulary_it_competes_for() {
        let (path, content) = padding_file(7);
        assert_eq!(path, "archive/order/archive_order_0007.py");
        assert!(content.contains("def archive_order_0007(record):"));
        assert!(content.contains("class OrderShard0007:"));
    }
}
