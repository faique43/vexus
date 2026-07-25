use assert_cmd::Command;
use predicates::prelude::*;

fn write(root: &std::path::Path, rel: &str, content: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, content).unwrap();
}

#[test]
fn index_status_search_flow() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "app.py", "def compute_backoff(delay):\n    return delay * 2\n");

    Command::cargo_bin("vexus").unwrap()
        .args(["index", root.to_str().unwrap()])
        .assert().success()
        .stdout(predicate::str::contains("indexed: 1"));

    assert!(root.join(".vexus/index.db").exists());
    assert_eq!(std::fs::read_to_string(root.join(".vexus/.gitignore")).unwrap(), "*\n");

    Command::cargo_bin("vexus").unwrap()
        .args(["status", root.to_str().unwrap()])
        .assert().success()
        .stdout(predicate::str::contains("files: 1").and(predicate::str::contains("symbols:")));

    Command::cargo_bin("vexus").unwrap()
        .args(["search", "backoff", root.to_str().unwrap()])
        .assert().success()
        .stdout(predicate::str::contains("app.py").and(predicate::str::contains("compute_backoff")));
}

#[test]
fn status_without_index_is_calm() {
    let dir = tempfile::tempdir().unwrap();
    Command::cargo_bin("vexus").unwrap()
        .args(["status", dir.path().to_str().unwrap()])
        .assert().success()
        .stdout(predicate::str::contains("no index"));
}
