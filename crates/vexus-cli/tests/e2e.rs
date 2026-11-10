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

/// Finding I8: `vexus index` must refuse — rather than race a concurrent
/// writer over the same SQLite file — when something else already holds
/// the advisory `.vexus/lock` (in practice, almost always an already-running
/// `vexus serve`).
#[test]
fn index_refuses_when_a_writer_lock_is_already_held() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "app.py",
        "def compute_backoff(delay):\n    return delay * 2\n",
    );

    // Simulates a `vexus serve` process already holding the lock.
    let _held = vexus_watch::WriterLock::try_acquire(root)
        .unwrap()
        .expect("test process should win the lock first");

    Command::cargo_bin("vexus")
        .unwrap()
        .env("VEXUS_EMBEDDER", "mock")
        .args(["index", root.to_str().unwrap()])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(
            "another vexus process owns this index — indexing skipped",
        ));

    // Nothing was written — the refusal happened before any index.db work.
    assert!(!root.join(".vexus/index.db").exists());
}

/// A `vexus serve` stopped while it builds a missing index leaves it marked
/// `indexing`, and MCP tools won't answer from an index in that state. A
/// full `vexus index` builds all of it, so it clears the mark.
#[test]
fn index_clears_the_mark_a_serve_stopped_mid_build_leaves() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "app.py", "def f():\n    return 1\n");
    {
        let mut store = vexus_core::Store::open(&root.join(".vexus/index.db")).unwrap();
        vexus_watch::set_freshness(&mut store, vexus_watch::Freshness::Indexing).unwrap();
    }

    Command::cargo_bin("vexus")
        .unwrap()
        .env("VEXUS_EMBEDDER", "none")
        .args(["index", root.to_str().unwrap()])
        .assert()
        .success();
    Command::cargo_bin("vexus")
        .unwrap()
        .args(["status", root.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("freshness: fresh"));
}

/// The prompt hook re-indexes in the background once its marker is five
/// minutes old. A `vexus index` has just done that work, so it sets the marker
/// too, and the next prompt doesn't start a second run over an unchanged repo.
#[test]
fn index_counts_as_the_hooks_refresh() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "app.py", "def f():\n    return 1\n");

    Command::cargo_bin("vexus")
        .unwrap()
        .env("VEXUS_EMBEDDER", "none")
        .args(["index", root.to_str().unwrap()])
        .assert()
        .success();

    let age = std::fs::metadata(root.join(".vexus/refreshed"))
        .and_then(|m| m.modified())
        .expect("index must leave the refresh marker")
        .elapsed()
        .unwrap();
    assert!(
        age < std::time::Duration::from_secs(60),
        "marker is {age:?} old"
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

fn init_claude_code(root: &std::path::Path, extra: &[&str]) -> assert_cmd::assert::Assert {
    let bin = assert_cmd::cargo::cargo_bin("vexus");
    Command::cargo_bin("vexus")
        .unwrap()
        .env("PATH", bin.parent().unwrap())
        .env("VEXUS_EMBEDDER", "mock")
        .args(["init", "--agent", "claude-code"])
        .args(extra)
        .arg(root.to_str().unwrap())
        .assert()
}

fn claude_settings(root: &std::path::Path) -> serde_json::Value {
    let text = std::fs::read_to_string(root.join(".claude/settings.json")).unwrap();
    serde_json::from_str(&text).unwrap()
}

#[test]
fn init_claude_code_installs_a_prompt_hook_claude_code_will_load() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    init_claude_code(root, &[]).success();

    let settings = claude_settings(root);
    assert_eq!(
        settings["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"],
        "vexus hook prompt"
    );
}

#[test]
fn init_claude_code_uses_the_full_path_when_vexus_is_not_on_path() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    Command::cargo_bin("vexus")
        .unwrap()
        .env("PATH", root)
        .env("VEXUS_EMBEDDER", "mock")
        .args(["init", "--agent", "claude-code", root.to_str().unwrap()])
        .assert()
        .success();

    let settings = claude_settings(root);
    let command = settings["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap();
    let bin = assert_cmd::cargo::cargo_bin("vexus");
    assert_eq!(command, format!("\"{}\" hook prompt", bin.display()));
}

#[test]
fn init_claude_code_builds_a_missing_index() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "app.py", "def f():\n    return 1\n");

    init_claude_code(root, &[]).success();

    for _ in 0..100 {
        if root.join(".vexus/index.db").exists() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    panic!("hints would never start");
}

#[test]
fn init_claude_code_adds_no_tools_by_default() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    init_claude_code(root, &[]).success();

    assert!(
        !root.join(".mcp.json").exists(),
        "tool definitions cost every session, used or not"
    );
    assert!(!root.join(".claude/skills").exists());
}

#[test]
fn init_claude_code_keeps_existing_settings_and_hooks() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        ".claude/settings.json",
        r#"{"model": "x", "hooks": {"UserPromptSubmit": [{"hooks": [{"type": "command", "command": "mine"}]}]}}"#,
    );

    init_claude_code(root, &[]).success();

    let settings = claude_settings(root);
    assert_eq!(settings["model"], "x");
    let entries = settings["hooks"]["UserPromptSubmit"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["hooks"][0]["command"], "mine");
}

#[test]
fn init_claude_code_twice_adds_the_hook_once() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    init_claude_code(root, &[]).success();
    let first = std::fs::read_to_string(root.join(".claude/settings.json")).unwrap();
    init_claude_code(root, &[])
        .success()
        .stdout(predicate::str::contains("skip"));

    assert_eq!(
        std::fs::read_to_string(root.join(".claude/settings.json")).unwrap(),
        first
    );
}

#[test]
fn init_claude_code_leaves_malformed_settings_alone() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, ".claude/settings.json", "{not json");

    init_claude_code(root, &[])
        .success()
        .stdout(predicate::str::contains("vexus hook prompt"));

    assert_eq!(
        std::fs::read_to_string(root.join(".claude/settings.json")).unwrap(),
        "{not json"
    );
}

fn run_prompt_hook(root: &std::path::Path, prompt: &str) -> assert_cmd::assert::Assert {
    let payload = serde_json::json!({
        "prompt": prompt,
        "cwd": root.to_str().unwrap(),
        "session_id": "s",
    });
    Command::cargo_bin("vexus")
        .unwrap()
        .env("VEXUS_EMBEDDER", "mock")
        .args(["hook", "prompt"])
        .write_stdin(payload.to_string())
        .assert()
}

#[test]
fn prompt_hook_points_at_the_code_a_prompt_is_about() {
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

    let out = run_prompt_hook(root, "retries wait too long in compute_backoff")
        .success()
        .get_output()
        .stdout
        .clone();

    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let context = v["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(context.contains("app.py:1-2 compute_backoff"), "{context}");
}

/// A repo indexed keyword-only, with its background refresh already done.
fn keyword_repo(root: &std::path::Path) {
    write(
        root,
        "app.py",
        "def compute_backoff(delay):\n    # plan the next retry\n    return delay * 2\n",
    );
    Command::cargo_bin("vexus")
        .unwrap()
        .env("VEXUS_EMBEDDER", "none")
        .args(["index", root.to_str().unwrap()])
        .assert()
        .success();
}

#[test]
fn prompt_hook_never_downloads_the_model() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    keyword_repo(&root);
    let payload = serde_json::json!({
        "prompt": "retries wait too long in compute_backoff",
        "cwd": root.to_str().unwrap(),
    });

    let output = Command::cargo_bin("vexus")
        .unwrap()
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("VEXUS_MODEL_URL", "file:///nowhere/")
        .env_remove("VEXUS_EMBEDDER")
        .args(["hook", "prompt"])
        .write_stdin(payload.to_string())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    assert!(
        !home.join(".vexus/models").exists(),
        "a prompt must not start a 160 MB download"
    );
    let text = String::from_utf8(output).unwrap();
    assert!(text.contains("compute_backoff"), "{text}");
}

#[test]
fn prompt_hook_works_from_a_subdirectory() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    keyword_repo(root);
    write(root, "sub/dir/notes.txt", "x");
    let payload = serde_json::json!({
        "prompt": "retries wait too long in compute_backoff",
        "cwd": root.join("sub/dir").to_str().unwrap(),
    });

    Command::cargo_bin("vexus")
        .unwrap()
        .env("VEXUS_EMBEDDER", "none")
        .args(["hook", "prompt"])
        .write_stdin(payload.to_string())
        .assert()
        .success()
        .stdout(predicate::str::contains("app.py:1-3 compute_backoff"));
}

#[test]
fn prompt_hook_says_nothing_about_an_unrelated_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    keyword_repo(root);
    let payload = serde_json::json!({
        "prompt": "plan a three day hiking itinerary around Kyoto",
        "cwd": root.to_str().unwrap(),
    });

    Command::cargo_bin("vexus")
        .unwrap()
        .env("VEXUS_EMBEDDER", "none")
        .args(["hook", "prompt"])
        .write_stdin(payload.to_string())
        .assert()
        .success()
        .stdout("");
}

#[test]
fn prompt_hook_stays_silent_without_an_index() {
    let dir = tempfile::tempdir().unwrap();

    run_prompt_hook(dir.path(), "retries wait too long in compute_backoff")
        .success()
        .stdout("");
}

#[test]
fn prompt_hook_survives_a_bad_payload() {
    Command::cargo_bin("vexus")
        .unwrap()
        .args(["hook", "prompt"])
        .write_stdin("not json")
        .assert()
        .success()
        .stdout("");
}

#[test]
fn version_flag_accepts_all_spellings() {
    for flag in ["-v", "-V", "--version"] {
        Command::cargo_bin("vexus")
            .unwrap()
            .arg(flag)
            .assert()
            .success()
            .stdout(predicate::str::contains(env!("CARGO_PKG_VERSION")));
    }
}

#[test]
fn init_claude_code_writes_mcp_json() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    Command::cargo_bin("vexus")
        .unwrap()
        .args([
            "init",
            "--agent",
            "claude-code",
            "--mcp",
            root.to_str().unwrap(),
        ])
        .assert()
        .success();

    let mcp: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(root.join(".mcp.json")).unwrap()).unwrap();
    assert_eq!(mcp["mcpServers"]["vexus"]["command"], "vexus");
    assert_eq!(
        mcp["mcpServers"]["vexus"]["args"],
        serde_json::json!(["serve", "."])
    );
}

#[test]
fn init_claude_code_merges_into_existing_mcp_json() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join(".mcp.json"),
        r#"{"mcpServers": {"other": {"command": "x", "args": []}}}"#,
    )
    .unwrap();

    Command::cargo_bin("vexus")
        .unwrap()
        .args([
            "init",
            "--agent",
            "claude-code",
            "--mcp",
            root.to_str().unwrap(),
        ])
        .assert()
        .success();

    let mcp: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(root.join(".mcp.json")).unwrap()).unwrap();
    assert_eq!(
        mcp["mcpServers"]["other"]["command"], "x",
        "existing servers must survive the merge"
    );
    assert_eq!(mcp["mcpServers"]["vexus"]["command"], "vexus");
}

#[test]
fn init_claude_code_keeps_customized_vexus_entry_without_force() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let custom =
        r#"{"mcpServers": {"vexus": {"command": "/opt/custom/vexus", "args": ["serve", "."]}}}"#;
    std::fs::write(root.join(".mcp.json"), custom).unwrap();

    Command::cargo_bin("vexus")
        .unwrap()
        .args([
            "init",
            "--agent",
            "claude-code",
            "--mcp",
            root.to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("--force"));

    assert_eq!(
        std::fs::read_to_string(root.join(".mcp.json")).unwrap(),
        custom,
        "a customized vexus entry must not be clobbered without --force"
    );
}

#[test]
fn init_claude_code_leaves_malformed_mcp_json_alone() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join(".mcp.json"), "{not json").unwrap();

    // init must still succeed (the hooks matter more) and must not
    // destroy the user's file, however broken it is
    Command::cargo_bin("vexus")
        .unwrap()
        .args([
            "init",
            "--agent",
            "claude-code",
            "--mcp",
            root.to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("mcpServers"));

    assert_eq!(
        std::fs::read_to_string(root.join(".mcp.json")).unwrap(),
        "{not json"
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
