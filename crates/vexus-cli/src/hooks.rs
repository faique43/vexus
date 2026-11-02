//! Claude Code hooks, installed by `vexus init --agent claude-code`.
//!
//! Agents skip optional search tools and pay for their definitions anyway, so
//! the index reaches them through hooks: no tool definitions, nothing to
//! choose. A hook runs on every prompt whether or not it can help, so nothing
//! here may break a session: anything unexpected ends in silence and exit 0.

use std::path::Path;
use std::time::{Duration, SystemTime};

use serde_json::{json, Value};
use vexus_core::locate::Hint;
use vexus_core::Store;

/// Hints a prompt gets, each about 25 tokens.
const SHOWN_HINTS: usize = 4;
const HINT_HEADER: &str =
    "Likely relevant code, from the repo index. Check it fits before relying on it.";

/// Below this a prompt is a reply, like "yes" or "run it", with nothing to find.
const MIN_PROMPT_CHARS: usize = 15;
/// A prompt this long that names code already says where to look. Hints on
/// those cost more than they saved and lowered the pass rate, even exact ones.
const DETAILED_PROMPT_CHARS: usize = 800;

/// Whether `hints` would help with `prompt`.
pub fn worth_showing(prompt: &str, hints: &[Hint]) -> bool {
    let chars = prompt.chars().count();
    let already_detailed = chars > DETAILED_PROMPT_CHARS && hints.iter().any(|h| h.named);
    chars >= MIN_PROMPT_CHARS && !hints.is_empty() && !already_detailed
}

/// `path:start-end Parent.name`, or without the range when `span` is unknown.
pub fn hint_line(hint: &Hint, span: Option<(u32, u32)>) -> String {
    let mut parts = hint.qualname.rsplit('.');
    let name = parts.next().unwrap_or_default();
    let stem = Path::new(&hint.path).file_stem().and_then(|s| s.to_str());
    let name = match parts.next() {
        Some(parent) if Some(parent) != stem => format!("{parent}.{name}"),
        _ => name.to_string(),
    };
    match span {
        Some((start, end)) => format!("{}:{start}-{end} {name}", hint.path),
        None => format!("{} {name}", hint.path),
    }
}

/// Where a symbol is now in `text`, for a file changed since it was indexed.
///
/// The index can trail the file by minutes while an agent edits it, so this
/// is the usual case in a working session, not an edge. A definition's opening
/// line survives most edits, and the nearest copy of it is the one that moved.
pub fn moved_span(hint: &Hint, text: &str) -> Option<(u32, u32)> {
    let sig = hint.sig.as_deref()?.trim();
    let start = text
        .lines()
        .zip(1u32..)
        .filter(|(line, _)| line.trim() == sig)
        .map(|(_, number)| number)
        .min_by_key(|number| number.abs_diff(hint.start_line))?;
    Some((start, start + hint.end_line.saturating_sub(hint.start_line)))
}

/// Where `hint` is on disk right now, `None` once its file is gone. The
/// inner `None` means the file is there but the symbol can't be placed.
fn current_span(root: &Path, store: &Store, hint: &Hint) -> Option<Option<(u32, u32)>> {
    let bytes = std::fs::read(root.join(&hint.path)).ok()?;
    let indexed = store.file_hash(&hint.path).ok()??;
    if indexed == *blake3::hash(&bytes).as_bytes() {
        return Some(Some((hint.start_line, hint.end_line)));
    }
    Some(moved_span(hint, &String::from_utf8_lossy(&bytes)))
}

/// The context to add for `prompt`, if any.
pub fn prompt_context(root: &Path, store: &Store, prompt: &str, hints: &[Hint]) -> Option<String> {
    if !worth_showing(prompt, hints) {
        return None;
    }
    let lines: Vec<String> = hints
        .iter()
        .filter_map(|h| Some(hint_line(h, current_span(root, store, h)?)))
        .take(SHOWN_HINTS)
        .collect();
    if lines.is_empty() {
        return None;
    }
    Some(format!("{HINT_HEADER}\n{}", lines.join("\n")))
}

/// `text` as the JSON Claude Code expects from a hook for `event`.
pub fn context_json(event: &str, text: &str) -> String {
    json!({"hookSpecificOutput": {"hookEventName": event, "additionalContext": text}}).to_string()
}

/// What the prompt hook needs out of Claude Code's payload: `(prompt, cwd)`.
pub fn parse_prompt_payload(payload: &str) -> Option<(String, String)> {
    let v: Value = serde_json::from_str(payload).ok()?;
    Some((
        v.get("prompt")?.as_str()?.to_string(),
        v.get("cwd")?.as_str()?.to_string(),
    ))
}

const PROMPT_HOOK_COMMAND: &str = "vexus hook prompt";
const PROMPT_HOOK_ARGS: &str = "hook prompt";
#[cfg(windows)]
const BINARY_NAME: &str = "vexus.exe";
#[cfg(not(windows))]
const BINARY_NAME: &str = "vexus";
/// The event's default is 30 seconds. A hook that slow has already failed.
const PROMPT_HOOK_TIMEOUT_SECS: u64 = 20;

/// What the hook should run. A bare `vexus` that isn't on PATH would fail on
/// every prompt, so the running binary's full path stands in for it.
fn hook_command(path_var: Option<&std::ffi::OsStr>, exe: &Path) -> String {
    let on_path = path_var.is_some_and(|paths| {
        std::env::split_paths(paths).any(|dir| dir.join(BINARY_NAME).is_file())
    });
    if on_path {
        PROMPT_HOOK_COMMAND.to_string()
    } else {
        format!("\"{}\" {PROMPT_HOOK_ARGS}", exe.display())
    }
}

/// `existing` settings with the prompt hook added, or `None` when it's already
/// there. An error means the file can't be safely rewritten.
pub fn merge_settings(existing: Option<&str>, command: &str) -> anyhow::Result<Option<String>> {
    let mut root = match existing {
        None => serde_json::Map::new(),
        Some(text) => match serde_json::from_str::<Value>(text)? {
            Value::Object(map) => map,
            _ => anyhow::bail!("top level is not a JSON object"),
        },
    };
    let hooks = root.entry("hooks").or_insert_with(|| json!({}));
    let Value::Object(hooks) = hooks else {
        anyhow::bail!(r#""hooks" is not a JSON object"#);
    };
    let entries = hooks.entry("UserPromptSubmit").or_insert_with(|| json!([]));
    let Value::Array(entries) = entries else {
        anyhow::bail!(r#""hooks.UserPromptSubmit" is not a JSON array"#);
    };
    if entries.iter().any(runs_prompt_hook) {
        return Ok(None);
    }
    entries.push(json!({"hooks": [{
        "type": "command",
        "command": command,
        "timeout": PROMPT_HOOK_TIMEOUT_SECS,
    }]}));
    let mut text = serde_json::to_string_pretty(&Value::Object(root))?;
    text.push('\n');
    Ok(Some(text))
}

fn is_prompt_hook(hook: &Value) -> bool {
    hook["command"]
        .as_str()
        .is_some_and(|c| c == PROMPT_HOOK_COMMAND || c.ends_with(&format!("\" {PROMPT_HOOK_ARGS}")))
}

fn runs_prompt_hook(entry: &Value) -> bool {
    entry["hooks"]
        .as_array()
        .is_some_and(|hooks| hooks.iter().any(is_prompt_hook))
}

/// Adds the hooks to `<root>/.claude/settings.json`, where Claude Code loads
/// them from. A file it can't safely rewrite is left alone, and the snippet is
/// printed to add by hand.
pub fn install(root: &Path) -> anyhow::Result<()> {
    let path = root.join(".claude/settings.json");
    let existing = match std::fs::read_to_string(&path) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(anyhow::Error::from(e).context(format!("read {}", path.display()))),
    };
    let exe = std::env::current_exe()?;
    let path_var = std::env::var_os("PATH");
    let command = hook_command(path_var.as_deref(), &exe);
    if command != PROMPT_HOOK_COMMAND {
        eprintln!(
            "vexus: not on PATH, so the hook uses {}. Anyone else sharing this settings file \
             needs the same path.",
            exe.display()
        );
    }
    match merge_settings(existing.as_deref(), &command) {
        Ok(Some(text)) => {
            std::fs::create_dir_all(root.join(".claude"))?;
            std::fs::write(&path, text)?;
            println!("{}", path.display());
        }
        Ok(None) => println!("skip: {} (vexus hook already there)", path.display()),
        Err(e) => {
            eprintln!(
                "vexus: could not update {} ({e:#}) — add this yourself:",
                path.display()
            );
            println!("{}", merge_settings(None, &command)?.unwrap_or_default());
        }
    }
    Ok(())
}

/// How long the index may trail the code on disk.
const REFRESH_INTERVAL: Duration = Duration::from_secs(300);

fn refresh_due(marker: &Path, now: SystemTime) -> bool {
    let last = std::fs::metadata(marker).and_then(|m| m.modified());
    last.map_or(true, |last| {
        now.duration_since(last)
            .is_ok_and(|age| age >= REFRESH_INTERVAL)
    })
}

/// Starts `vexus index` in the background if it's due. With no MCP server
/// there's no watcher, and a hook can't make the prompt wait for this: hints
/// use the index as it stands, and `current_span` covers what's changed.
pub fn refresh_index(root: &Path) {
    let marker = root.join(".vexus/refreshed");
    if !refresh_due(&marker, SystemTime::now()) || std::fs::write(&marker, b"").is_err() {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let _ = std::process::Command::new(exe)
        .arg("index")
        .arg(root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;
    use vexus_core::model::*;

    fn hint(path: &str, named: bool) -> Hint {
        Hint {
            path: path.into(),
            qualname: "packages.core.Router.handle".into(),
            sig: Some("def handle(self):".into()),
            start_line: 10,
            end_line: 40,
            named,
        }
    }

    fn file_with_sig_at(line: usize) -> String {
        let mut lines = vec!["pass"; 60];
        lines[line - 1] = "    def handle(self):";
        lines.join("\n")
    }

    fn indexed_repo(path: &str, content: &str) -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(path), content).unwrap();
        let mut store = Store::open(&dir.path().join("index.db")).unwrap();
        let hash = *blake3::hash(content.as_bytes()).as_bytes();
        let file = FileIndex {
            symbols: vec![],
            edges: vec![],
            chunks: vec![],
        };
        store.replace_file(path, "python", &hash, &file).unwrap();
        (dir, store)
    }

    #[test]
    fn replies_get_no_hints() {
        assert!(!worth_showing("yes do it", &[hint("a.py", false)]));
    }

    #[test]
    fn no_hints_means_nothing_to_show() {
        assert!(!worth_showing("the cache misses on every read", &[]));
    }

    #[test]
    fn a_short_request_gets_hints_even_when_it_names_code() {
        assert!(worth_showing(
            "add a date option to ConsoleLogger",
            &[hint("a.py", true)]
        ));
    }

    #[test]
    fn a_long_vague_report_gets_hints() {
        let report = "the app hangs when the broker restarts ".repeat(30);
        assert!(worth_showing(&report, &[hint("a.py", false)]));
    }

    #[test]
    fn a_long_report_that_names_code_gets_none() {
        let report = "ClientNats hangs when the broker restarts ".repeat(30);
        assert!(!worth_showing(&report, &[hint("a.py", true)]));
    }

    #[test]
    fn a_hint_line_is_location_then_short_name() {
        assert_eq!(
            hint_line(&hint("src/a.py", false), Some((10, 40))),
            "src/a.py:10-40 Router.handle"
        );
    }

    #[test]
    fn a_hint_line_doesnt_repeat_the_file_name() {
        let mut h = hint("src/client-nats.ts", false);
        h.qualname = "src.client-nats.helper".into();
        assert_eq!(
            hint_line(&h, Some((10, 40))),
            "src/client-nats.ts:10-40 helper"
        );
    }

    #[test]
    fn a_hint_with_no_known_lines_is_just_the_location() {
        assert_eq!(
            hint_line(&hint("src/a.py", false), None),
            "src/a.py Router.handle"
        );
    }

    #[test]
    fn a_symbol_pushed_down_by_an_edit_keeps_its_length() {
        let text = file_with_sig_at(13);
        assert_eq!(moved_span(&hint("a.py", false), &text), Some((13, 43)));
    }

    #[test]
    fn a_symbol_whose_signature_is_gone_has_no_lines() {
        assert_eq!(moved_span(&hint("a.py", false), "pass\npass"), None);
    }

    #[test]
    fn a_repeated_signature_resolves_to_the_nearest() {
        let mut lines = vec!["pass"; 80];
        lines[1] = "def handle(self):";
        lines[11] = "def handle(self):";
        lines[70] = "def handle(self):";
        let text = lines.join("\n");
        assert_eq!(moved_span(&hint("a.py", false), &text), Some((12, 42)));
    }

    #[test]
    fn a_symbol_with_no_signature_has_no_lines() {
        let mut h = hint("a.py", false);
        h.sig = None;
        assert_eq!(moved_span(&h, &file_with_sig_at(13)), None);
    }

    #[test]
    fn context_lists_hints_under_the_header() {
        let (dir, store) = indexed_repo("a.py", "x = 1\n");
        let text = prompt_context(
            dir.path(),
            &store,
            "why does this crash on startup",
            &[hint("a.py", false)],
        )
        .unwrap();
        assert_eq!(text, format!("{HINT_HEADER}\na.py:10-40 Router.handle"));
    }

    #[test]
    fn context_follows_a_symbol_through_an_edit() {
        let (dir, store) = indexed_repo("a.py", "x = 1\n");
        std::fs::write(dir.path().join("a.py"), file_with_sig_at(13)).unwrap();
        let text = prompt_context(
            dir.path(),
            &store,
            "why does this crash on startup",
            &[hint("a.py", false)],
        )
        .unwrap();
        assert!(text.ends_with("\na.py:13-43 Router.handle"), "{text}");
    }

    #[test]
    fn context_drops_the_range_when_the_symbol_cant_be_found() {
        let (dir, store) = indexed_repo("a.py", "x = 1\n");
        std::fs::write(dir.path().join("a.py"), "x = 2\n").unwrap();
        let text = prompt_context(
            dir.path(),
            &store,
            "why does this crash on startup",
            &[hint("a.py", false)],
        )
        .unwrap();
        assert!(text.ends_with("\na.py Router.handle"), "{text}");
    }

    #[test]
    fn context_skips_a_deleted_file() {
        let (dir, store) = indexed_repo("a.py", "x = 1\n");
        std::fs::remove_file(dir.path().join("a.py")).unwrap();
        let text = prompt_context(
            dir.path(),
            &store,
            "why does this crash on startup",
            &[hint("a.py", false)],
        );
        assert_eq!(text, None);
    }

    #[test]
    fn context_shows_at_most_four() {
        let (dir, store) = indexed_repo("a.py", "x = 1\n");
        let hints = vec![hint("a.py", false); 9];
        let text =
            prompt_context(dir.path(), &store, "why does this crash on startup", &hints).unwrap();
        assert_eq!(text.lines().count(), 5);
    }

    #[test]
    fn hook_json_has_the_shape_claude_code_reads() {
        let v: serde_json::Value =
            serde_json::from_str(&context_json("UserPromptSubmit", "hello")).unwrap();
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "UserPromptSubmit");
        assert_eq!(v["hookSpecificOutput"]["additionalContext"], "hello");
    }

    #[test]
    fn a_payload_gives_prompt_and_cwd() {
        let got = parse_prompt_payload(r#"{"prompt":"fix it","cwd":"/repo","session_id":"s"}"#);
        assert_eq!(got, Some(("fix it".to_string(), "/repo".to_string())));
    }

    #[test]
    fn a_bad_payload_gives_nothing() {
        assert_eq!(parse_prompt_payload("not json"), None);
        assert_eq!(parse_prompt_payload(r#"{"prompt":"no cwd"}"#), None);
    }

    const COMMAND: &str = "vexus hook prompt";

    #[test]
    fn settings_are_created_when_there_are_none() {
        let text = merge_settings(None, COMMAND).unwrap().unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            v["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"],
            COMMAND
        );
    }

    #[test]
    fn settings_already_holding_the_hook_need_no_write() {
        let once = merge_settings(None, COMMAND).unwrap().unwrap();
        assert_eq!(merge_settings(Some(&once), COMMAND).unwrap(), None);
    }

    #[test]
    fn a_hook_installed_by_path_counts_as_installed() {
        let once = merge_settings(None, "\"/opt/my tools/vexus\" hook prompt")
            .unwrap()
            .unwrap();
        assert_eq!(merge_settings(Some(&once), COMMAND).unwrap(), None);
    }

    #[test]
    fn settings_that_are_not_an_object_are_refused() {
        assert!(merge_settings(Some("[]"), COMMAND).is_err());
        assert!(merge_settings(Some(r#"{"hooks": []}"#), COMMAND).is_err());
        assert!(merge_settings(Some(r#"{"hooks": {"UserPromptSubmit": 1}}"#), COMMAND).is_err());
    }

    #[test]
    fn the_command_is_plain_when_vexus_is_on_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(BINARY_NAME), b"").unwrap();
        let path = std::env::join_paths([dir.path()]).unwrap();
        assert_eq!(
            hook_command(Some(&path), Path::new("/opt/x/vexus")),
            "vexus hook prompt"
        );
    }

    #[test]
    fn the_command_uses_the_full_path_when_vexus_is_not_on_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = std::env::join_paths([dir.path()]).unwrap();
        assert_eq!(
            hook_command(Some(&path), Path::new("/opt/my tools/vexus")),
            "\"/opt/my tools/vexus\" hook prompt"
        );
    }

    #[test]
    fn a_refresh_is_due_without_a_marker() {
        let dir = tempfile::tempdir().unwrap();
        assert!(refresh_due(
            &dir.path().join("refreshed"),
            SystemTime::now()
        ));
    }

    #[test]
    fn a_refresh_waits_out_the_interval() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("refreshed");
        std::fs::write(&marker, b"").unwrap();
        let now = SystemTime::now();
        assert!(!refresh_due(&marker, now));
        assert!(refresh_due(
            &marker,
            now + REFRESH_INTERVAL + Duration::from_secs(1)
        ));
    }
}
