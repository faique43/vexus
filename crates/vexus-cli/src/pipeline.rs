use std::collections::HashSet;
use std::path::Path;

use anyhow::Result;
use vexus_core::Store;

#[derive(Debug, Default)]
pub struct IndexReport {
    pub indexed: usize,
    pub skipped_unchanged: usize,
    pub skipped_unsupported: usize,
    pub removed: usize,
    pub failed: Vec<String>,
}

const MAX_FILE_BYTES: u64 = 1_048_576;

pub fn index_repo(root: &Path, store: &mut Store) -> Result<IndexReport> {
    let mut report = IndexReport::default();
    let mut seen: HashSet<String> = HashSet::new();

    let walker = ignore::WalkBuilder::new(root)
        .hidden(false)
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            name != ".git" && name != ".vexus"
        })
        .build();

    for entry in walker {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.path();
        let rel = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");

        let Some(lang) = vexus_index::lang::for_path(path) else {
            report.skipped_unsupported += 1;
            continue;
        };
        if entry.metadata().map(|m| m.len()).unwrap_or(0) > MAX_FILE_BYTES {
            report.skipped_unsupported += 1;
            continue;
        }
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) => {
                report.failed.push(format!("{rel}: {e}"));
                continue;
            }
        };
        if bytes.iter().take(8192).any(|&b| b == 0) {
            report.skipped_unsupported += 1;
            continue;
        }

        seen.insert(rel.clone());
        let hash: [u8; 32] = *blake3::hash(&bytes).as_bytes();
        if store.file_hash(&rel)? == Some(hash) {
            report.skipped_unchanged += 1;
            continue;
        }

        let source = String::from_utf8_lossy(&bytes).into_owned();
        let parsed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            vexus_index::parse::parse_file(lang, &rel, &source)
        }));
        match parsed {
            Ok(idx) => {
                store.replace_file(&rel, lang.name, &hash, &idx)?;
                report.indexed += 1;
            }
            Err(_) => report.failed.push(format!("{rel}: parser panic")),
        }
    }

    // Remove DB entries for files gone from disk.
    let db_paths = store.file_paths()?;
    for p in db_paths {
        if !seen.contains(&p) {
            store.remove_file(&p)?;
            report.removed += 1;
        }
    }

    store.resolve_all_edges()?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &std::path::Path, rel: &str, content: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    #[test]
    fn index_hash_gate_and_removal() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "src/app.py", "def run():\n    return 1\n");
        write(root, "src/util.py", "def helper():\n    return 2\n");
        write(root, "README.md", "# not code\n");
        write(root, "big.py", &"x = 1\n".repeat(200_000)); // >1MB

        let mut store = vexus_core::Store::open(&root.join(".vexus/index.db")).unwrap();

        let r = index_repo(root, &mut store).unwrap();
        assert_eq!(r.indexed, 2);
        assert_eq!(r.skipped_unsupported, 2); // README (no language) + big.py (>1MB)
        assert_eq!(store.counts().unwrap().files, 2);

        // second run: nothing changed
        let r = index_repo(root, &mut store).unwrap();
        assert_eq!((r.indexed, r.skipped_unchanged), (0, 2));

        // edit one, delete one
        write(root, "src/app.py", "def run():\n    return 42\n");
        std::fs::remove_file(root.join("src/util.py")).unwrap();
        let r = index_repo(root, &mut store).unwrap();
        assert_eq!((r.indexed, r.removed), (1, 1));
        assert_eq!(store.counts().unwrap().files, 1);
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_file_goes_to_failed_and_others_still_index() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "src/app.py", "def run():\n    return 1\n");
        write(root, "src/secret.py", "def hidden():\n    return 2\n");

        let secret_path = root.join("src/secret.py");
        std::fs::set_permissions(&secret_path, std::fs::Permissions::from_mode(0o000)).unwrap();

        let mut store = vexus_core::Store::open(&root.join(".vexus/index.db")).unwrap();
        let result = index_repo(root, &mut store);

        // Restore permissions before asserting/unwrapping so tempdir cleanup never fails.
        std::fs::set_permissions(&secret_path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let r = result.unwrap();
        assert_eq!(r.failed.len(), 1, "failed: {:?}", r.failed);
        assert!(r.failed[0].contains("secret.py"), "failed: {:?}", r.failed);
        assert_eq!(r.indexed, 1);
        assert_eq!(store.counts().unwrap().files, 1);
    }
}
