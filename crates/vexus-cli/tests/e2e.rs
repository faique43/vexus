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
    write(
        root,
        "app.py",
        "def compute_backoff(delay):\n    return delay * 2\n",
    );

    Command::cargo_bin("vexus")
        .unwrap()
        .env("VEXUS_EMBEDDER", "mock")
        .args(["index", root.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("indexed: 1"));

    assert!(root.join(".vexus/index.db").exists());
    assert_eq!(
        std::fs::read_to_string(root.join(".vexus/.gitignore")).unwrap(),
        "*\n"
    );

    Command::cargo_bin("vexus")
        .unwrap()
        .env("VEXUS_EMBEDDER", "mock")
        .args(["status", root.to_str().unwrap()])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("index: 1 files")
                .and(predicate::str::contains("symbols"))
                .and(predicate::str::contains("last event: none"))
                .and(predicate::str::contains("serve: not running")),
        );

    Command::cargo_bin("vexus")
        .unwrap()
        .env("VEXUS_EMBEDDER", "mock")
        .args(["search", "backoff", root.to_str().unwrap()])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("app.py").and(predicate::str::contains("compute_backoff")),
        );
}

/// `vexus status` never claims a `role:` of its own (that line is reserved
/// for an actual in-`serve` process — see `status_text`'s doc comment) but
/// does report whether one is currently running, via a probe-and-release of
/// the same advisory `.vexus/lock` `vexus serve` uses: while another
/// process holds it, `status` reports `serve: running`; once released,
/// `status` (winning the now-uncontested lock itself, briefly, then
/// releasing it again) reports `serve: not running`.
#[test]
fn status_serve_line_reflects_the_advisory_writer_lock() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "app.py",
        "def compute_backoff(delay):\n    return delay * 2\n",
    );

    Command::cargo_bin("vexus")
        .unwrap()
        .env("VEXUS_EMBEDDER", "mock")
        .args(["index", root.to_str().unwrap()])
        .assert()
        .success();

    {
        // Simulates a `vexus serve` process already holding the lock.
        let _held = vexus_watch::WriterLock::try_acquire(root)
            .unwrap()
            .expect("test process should win the lock first");

        Command::cargo_bin("vexus")
            .unwrap()
            .env("VEXUS_EMBEDDER", "mock")
            .args(["status", root.to_str().unwrap()])
            .assert()
            .success()
            .stdout(
                predicate::str::contains("serve: running")
                    .and(predicate::str::contains("role:").not()),
            );
        // `_held` drops (and releases the lock) at the end of this block.
    }

    Command::cargo_bin("vexus")
        .unwrap()
        .env("VEXUS_EMBEDDER", "mock")
        .args(["status", root.to_str().unwrap()])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("serve: not running")
                .and(predicate::str::contains("role:").not()),
        );
}

#[test]
fn status_without_index_is_calm() {
    let dir = tempfile::tempdir().unwrap();
    Command::cargo_bin("vexus")
        .unwrap()
        .env("VEXUS_EMBEDDER", "mock")
        .args(["status", dir.path().to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("no index"));
}

#[test]
fn index_embeds_with_mock_and_hybrid_search_works() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "app.py",
        "def compute_backoff(delay):\n    return delay * 2\n",
    );

    Command::cargo_bin("vexus")
        .unwrap()
        .env("VEXUS_EMBEDDER", "mock")
        .args(["index", root.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("embedded:"));

    Command::cargo_bin("vexus")
        .unwrap()
        .env("VEXUS_EMBEDDER", "mock")
        .args(["status", root.to_str().unwrap()])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("model: mock")
                .and(predicate::str::contains("embed backlog: 0")),
        );

    Command::cargo_bin("vexus")
        .unwrap()
        .env("VEXUS_EMBEDDER", "mock")
        .args(["search", "backoff", root.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("compute_backoff"));
}

#[test]
fn structural_only_mode_still_works() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "app.py",
        "def compute_backoff(delay):\n    return delay * 2\n",
    );

    Command::cargo_bin("vexus")
        .unwrap()
        .env("VEXUS_EMBEDDER", "none")
        .args(["index", root.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("embeddings: skipped"));

    Command::cargo_bin("vexus")
        .unwrap()
        .env("VEXUS_EMBEDDER", "none")
        .args(["search", "backoff", root.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("compute_backoff"));
}

#[test]
fn init_cursor_writes_mdc_file() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    Command::cargo_bin("vexus")
        .unwrap()
        .args(["init", "--agent", "cursor", root.to_str().unwrap()])
        .assert()
        .success();

    let mdc_path = root.join(".cursor/rules/vexus.mdc");
    assert!(mdc_path.exists(), "cursor mdc file should exist");

    let content = std::fs::read_to_string(&mdc_path).unwrap();
    assert!(
        content.contains("alwaysApply: true"),
        "cursor file should contain alwaysApply: true"
    );
    assert!(
        content.contains("vexus code index"),
        "cursor file should contain vexus description"
    );
}

#[test]
fn init_claude_code_writes_all_files() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    Command::cargo_bin("vexus")
        .unwrap()
        .args(["init", "--agent", "claude-code", root.to_str().unwrap()])
        .assert()
        .success();

    // Check all expected files exist
    let plugin_json = root.join(".claude/plugins/vexus/.claude-plugin/plugin.json");
    let hooks_json = root.join(".claude/plugins/vexus/hooks/hooks.json");
    let nudge_sh = root.join(".claude/plugins/vexus/hooks/nudge-grep.sh");
    let skill_md = root.join(".claude/plugins/vexus/skills/vexus/SKILL.md");

    assert!(plugin_json.exists(), "plugin.json should exist");
    assert!(hooks_json.exists(), "hooks.json should exist");
    assert!(nudge_sh.exists(), "nudge-grep.sh should exist");
    assert!(skill_md.exists(), "SKILL.md should exist");

    // Check hook script is executable
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::metadata(&nudge_sh).unwrap().permissions();
        assert!(
            perms.mode() & 0o111 != 0,
            "nudge-grep.sh should have execute bit set"
        );
    }
}

#[test]
fn init_claude_code_without_force_skips_existing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    // First run
    Command::cargo_bin("vexus")
        .unwrap()
        .args(["init", "--agent", "claude-code", root.to_str().unwrap()])
        .assert()
        .success();

    let plugin_json = root.join(".claude/plugins/vexus/.claude-plugin/plugin.json");
    let original_content = std::fs::read_to_string(&plugin_json).unwrap();

    // Second run without --force should skip
    Command::cargo_bin("vexus")
        .unwrap()
        .args(["init", "--agent", "claude-code", root.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("skip").or(predicate::str::contains("Skip")));

    // Content should be unchanged
    let current_content = std::fs::read_to_string(&plugin_json).unwrap();
    assert_eq!(
        original_content, current_content,
        "file should not be overwritten without --force"
    );
}

#[test]
fn init_claude_code_with_force_overwrites() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    // First run
    Command::cargo_bin("vexus")
        .unwrap()
        .args(["init", "--agent", "claude-code", root.to_str().unwrap()])
        .assert()
        .success();

    let plugin_json = root.join(".claude/plugins/vexus/.claude-plugin/plugin.json");

    // Modify the file
    std::fs::write(&plugin_json, "modified content").unwrap();
    let modified_content = std::fs::read_to_string(&plugin_json).unwrap();
    assert_eq!(modified_content, "modified content");

    // Second run with --force should overwrite
    Command::cargo_bin("vexus")
        .unwrap()
        .args([
            "init",
            "--agent",
            "claude-code",
            "--force",
            root.to_str().unwrap(),
        ])
        .assert()
        .success();

    let current_content = std::fs::read_to_string(&plugin_json).unwrap();
    assert_ne!(
        current_content, "modified content",
        "file should be overwritten with --force"
    );
    assert!(
        current_content.contains("vexus"),
        "file should contain original content"
    );
}

#[test]
fn init_generic_prints_to_stdout() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    Command::cargo_bin("vexus")
        .unwrap()
        .args(["init", "--agent", "generic", root.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("explore").and(predicate::str::contains("Code search")));

    // Check no files were created
    assert!(
        !root.join(".cursor").exists(),
        "no .cursor directory should be created"
    );
    assert!(
        !root.join(".claude").exists(),
        "no .claude directory should be created"
    );
}
