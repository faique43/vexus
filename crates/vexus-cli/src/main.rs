mod hooks;

use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::{Parser, Subcommand};
use vexus_watch::pipeline;

/// `--version` output. The schema version and model id are here because
/// they're what actually determines whether an existing `.vexus/index.db` is
/// usable by this binary — a mismatch in either rebuilds the index, so when
/// someone reports "it re-indexed everything", these are the two numbers
/// worth comparing between their old and new build.
const LONG_VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    "\nindex schema: 5",
    "\nembedding model: jina-code-v2-q",
);

// `concat!` only accepts literal tokens, so the schema version above is
// written out rather than interpolated — which would let it drift silently
// the next time the schema changes. This fails the build instead.
const _: () = assert!(
    vexus_core::SCHEMA_VERSION.as_bytes()[0] == b'5' && vexus_core::SCHEMA_VERSION.len() == 1,
    "LONG_VERSION's hardcoded schema version no longer matches vexus_core::SCHEMA_VERSION"
);

#[derive(Parser)]
#[command(
    name = "vexus",
    version,
    long_version = LONG_VERSION,
    disable_version_flag = true,
    about = "Local code intelligence for coding agents"
)]
struct Cli {
    /// Print version
    // Hand-rolled (with the default -V/--version disabled) only to also
    // accept `-v` — the first thing people actually type, per field reports.
    #[arg(short = 'V', short_alias = 'v', long = "version", action = clap::ArgAction::Version)]
    version: (),

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Build or update the index for a repo
    Index { path: Option<PathBuf> },
    /// Show index freshness and counts
    Status { path: Option<PathBuf> },
    /// Keyword search over indexed chunks
    Search {
        query: String,
        path: Option<PathBuf>,
        #[arg(long, default_value_t = 10)]
        limit: u32,
    },
    /// Run the MCP server over stdio (builds the index first if none exists)
    Serve { path: Option<PathBuf> },
    /// Set up an agent: hooks for claude-code, rules for cursor, a snippet for generic
    Init {
        #[arg(long)]
        agent: String,
        #[arg(long)]
        force: bool,
        /// Claude Code only: also register the MCP server. Its tool
        /// definitions cost every session, used or not.
        #[arg(long)]
        mcp: bool,
        path: Option<PathBuf>,
    },
    /// What installed agent hooks run — not for interactive use.
    #[command(hide = true)]
    Hook { name: String },
}

fn db_path(root: &Path) -> PathBuf {
    root.join(".vexus/index.db")
}

fn open_store(root: &Path) -> Result<vexus_core::Store> {
    let store = vexus_core::Store::open(&db_path(root))?;
    // self-ignoring dir, like target/
    std::fs::write(root.join(".vexus/.gitignore"), "*\n")?;
    Ok(store)
}

fn write_pack_file(path: &Path, content: &str, force: bool) -> Result<bool> {
    if path.exists() && !force {
        println!("skip: {}", path.display());
        return Ok(false);
    }
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(path, content)?;
    println!("{}", path.display());
    Ok(true)
}

/// The MCP server entry `init --agent claude-code` registers — the same
/// snippet the README tells people to paste by hand.
fn vexus_mcp_entry() -> serde_json::Value {
    serde_json::json!({ "command": "vexus", "args": ["serve", "."] })
}

#[derive(Debug)]
enum McpMerge {
    /// New `.mcp.json` content to write (entry added or force-replaced).
    Write(String),
    /// A `vexus` entry identical to ours is already there — nothing to do.
    AlreadyConfigured,
    /// A *different* `vexus` entry is there and `force` is off — keep the
    /// user's customization.
    KeptExisting,
}

/// Merges the vexus server entry into the (possibly absent) `.mcp.json`
/// content. Pure: no filesystem access, so every edge is unit-testable.
/// Errors mean "this file can't be safely rewritten" (malformed JSON, or a
/// shape we don't understand) — the caller degrades to printing the snippet
/// rather than clobbering whatever is there.
fn merge_mcp_json(existing: Option<&str>, force: bool) -> Result<McpMerge> {
    use serde_json::{Map, Value};
    let mut root: Map<String, Value> = match existing {
        None => Map::new(),
        Some(s) => match serde_json::from_str::<Value>(s)? {
            Value::Object(m) => m,
            _ => anyhow::bail!("top level is not a JSON object"),
        },
    };
    let servers = root
        .entry("mcpServers")
        .or_insert_with(|| Value::Object(Map::new()));
    let Value::Object(servers) = servers else {
        anyhow::bail!(r#""mcpServers" is not a JSON object"#);
    };
    let entry = vexus_mcp_entry();
    match servers.get("vexus") {
        Some(current) if *current == entry => return Ok(McpMerge::AlreadyConfigured),
        Some(_) if !force => return Ok(McpMerge::KeptExisting),
        _ => {}
    }
    servers.insert("vexus".into(), entry);
    let mut out = serde_json::to_string_pretty(&Value::Object(root))?;
    out.push('\n');
    Ok(McpMerge::Write(out))
}

/// Registers the vexus MCP server in `<root>/.mcp.json`, creating or merging
/// as needed. Never fails `init`: a file we can't safely rewrite degrades to
/// printing the snippet for the user to paste, matching the old behavior.
fn register_mcp_server(root: &Path, force: bool) -> Result<()> {
    let mcp_path = root.join(".mcp.json");
    let existing = match std::fs::read_to_string(&mcp_path) {
        Ok(s) => Some(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            return Err(anyhow::Error::from(e).context(format!("read {}", mcp_path.display())))
        }
    };
    match merge_mcp_json(existing.as_deref(), force) {
        Ok(McpMerge::Write(content)) => {
            std::fs::write(&mcp_path, content)?;
            println!("{}", mcp_path.display());
        }
        Ok(McpMerge::AlreadyConfigured) => {
            println!("skip: {} (vexus already registered)", mcp_path.display());
        }
        Ok(McpMerge::KeptExisting) => {
            println!(
                "skip: {} (existing \"vexus\" entry differs — rerun with --force to replace it)",
                mcp_path.display()
            );
        }
        Err(e) => {
            eprintln!(
                "vexus: could not update {} ({e:#}) — add this yourself:",
                mcp_path.display()
            );
            println!(
                r#"{{ "mcpServers": {{ "vexus": {{ "command": "vexus", "args": ["serve", "."] }} }} }}"#
            );
        }
    }
    Ok(())
}

/// A vector for `text` and the distance floor to use with it, both `None` when
/// the embedder from `build` isn't the one this index was built with: its
/// vector wouldn't fit `vec_chunks`, which sqlite-vec rejects as a hard error.
///
/// Reads the indexed model from `meta` BEFORE building an embedder, because
/// the default path loads a full ONNX session, which is wasted work when
/// `meta.model_id` already says it can't match. `VEXUS_EMBEDDER=mock`/`none`
/// stay cheap either way.
fn embed_for_query(
    store: &vexus_core::Store,
    text: &str,
    build: impl FnOnce() -> Option<Box<dyn vexus_embed::Embedder>>,
) -> (Option<Vec<f32>>, Option<f64>) {
    let indexed_id = store.meta("model_id").ok().flatten();
    let indexed_dim = store.meta("model_dim").ok().flatten();
    let worth_building = match std::env::var("VEXUS_EMBEDDER").ok().as_deref() {
        Some("mock") | Some("none") => true,
        _ => indexed_id.is_none() || indexed_id.as_deref() == Some(vexus_embed::JINA_CODE_V2.id),
    };
    if !worth_building {
        return (None, None);
    }
    let Some(embedder) = build() else {
        return (None, None);
    };
    let same_model = indexed_id.as_deref() == Some(embedder.id())
        && indexed_dim.as_deref() == Some(embedder.dim().to_string().as_str());
    if !same_model {
        return (None, None);
    }
    let vector = embedder.embed(&[text]).ok().and_then(|mut v| v.pop());
    (
        vector,
        vexus_embed::effective_distance_floor(embedder.as_ref()),
    )
}

/// How much of a prompt gets embedded. A report's opening says what's wrong,
/// and the model only reads 512 tokens anyway.
const EMBEDDED_PROMPT_CHARS: usize = 1200;
/// Enough to fill the shown hints after dropping any whose file is gone.
const LOCATED_HINTS: usize = 8;

/// `vexus hook prompt`: locations for a prompt, as Claude Code hook output.
/// Everything that can rule a prompt out comes before the model loads, which
/// costs 0.3 s and 380 MB.
fn prompt_hook(payload: &str) -> Option<String> {
    let (prompt, cwd) = hooks::parse_prompt_payload(payload)?;
    let root = hooks::find_root(Path::new(&cwd))?;
    hooks::refresh_index(&root);
    let store = vexus_core::Store::open_read_only(&db_path(&root)).ok()?;
    if !hooks::worth_locating(&prompt, store.names_code(&prompt).ok()?) {
        return None;
    }
    let opening: String = prompt.chars().take(EMBEDDED_PROMPT_CHARS).collect();
    let (vector, floor) = embed_for_query(&store, &opening, vexus_embed::select::cached_embedder);
    let located = store
        .locate(&prompt, vector.as_deref(), floor, LOCATED_HINTS)
        .ok()?;
    let context = hooks::prompt_context(&root, &store, &located)?;
    Some(hooks::context_json("UserPromptSubmit", &context))
}

fn run_hook(name: &str) -> Result<()> {
    // Unknown names exit 0 silently: settings written by a newer vexus running
    // against an older binary must not break the agent's session.
    if name == "prompt" {
        let mut payload = String::new();
        use std::io::Read;
        let _ = std::io::stdin().read_to_string(&mut payload);
        if let Some(json) = prompt_hook(&payload) {
            println!("{json}");
        }
    }
    Ok(())
}

fn init_steering_packs(agent: &str, root: &Path, force: bool, mcp: bool) -> Result<()> {
    match agent {
        "claude-code" => {
            hooks::install(root, force)?;
            if mcp {
                register_mcp_server(root, force)?;
            }
            if !db_path(root).exists() {
                hooks::index_in_background(root);
                println!("\nIndexing in the background. Hints start when it finishes.");
            }
            println!(
                "\nStart Claude Code from {}. The hooks point it at relevant code on each \
                 prompt, with no tools added. Claude Code may ask you to approve them.",
                root.display()
            );
        }
        "cursor" => {
            let cursor_rules = root.join(".cursor/rules/vexus.mdc");
            let mdc_content = include_str!("../../../packs/cursor/vexus.mdc");
            write_pack_file(&cursor_rules, mdc_content, force)?;
        }
        "generic" => {
            let generic_content = include_str!("../../../packs/generic/AGENTS-snippet.md");
            println!("{}", generic_content);
        }
        _ => {
            anyhow::bail!(
                "Unknown agent: {}. Expected one of: claude-code, cursor, generic",
                agent
            );
        }
    }

    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Index { path } => {
            let root = path.unwrap_or_else(|| PathBuf::from("."));
            // Finding I8: hold the same advisory writer lock `vexus serve`
            // uses for the whole run. If something else already holds it —
            // in practice, almost always an already-running `vexus serve`,
            // whose writer thread owns this index — refuse outright rather
            // than risk two processes writing to the same SQLite file at
            // once; an honest refusal beats corruption (the `busy_timeout`
            // pragma alone would just make the two writes race instead of
            // preventing the race). Holding the lock (rather than a
            // check-and-release probe) for the whole run also means a
            // `vexus serve` that starts concurrently correctly sees this
            // process as the writer and starts in reader mode instead of
            // racing it.
            let _lock = match vexus_watch::WriterLock::try_acquire(&root)? {
                Some(lock) => lock,
                None => {
                    eprintln!(
                        "error: another vexus process owns this index — indexing skipped \
                         (a running serve or index; let it finish or stop it)"
                    );
                    std::process::exit(1);
                }
            };
            let mut store = open_store(&root)?;
            let r = pipeline::index_repo(&root, &mut store)?;
            println!(
                "indexed: {}  unchanged: {}  skipped: {}  removed: {}  failed: {}",
                r.indexed,
                r.skipped_unchanged,
                r.skipped_unsupported,
                r.removed,
                r.failed.len()
            );
            for f in &r.failed {
                eprintln!("failed: {f}");
            }

            if !store.vec_available() {
                // No point building/loading an embedder (or reporting a
                // fake "embedded: 0") when sqlite-vec itself isn't loaded —
                // every embedding would be discarded on the way into the
                // store, so structural-only is the honest outcome here.
                println!("embeddings: skipped (sqlite-vec unavailable)");
            } else {
                match vexus_embed::select::make_embedder_for_backlog(store.embed_backlog()?) {
                    Some(embedder) => {
                        store.set_model(embedder.id(), embedder.dim())?;
                        // Degrade, never die: structural indexing above already
                        // succeeded and was reported, so an embedding failure
                        // (e.g. a flaky ONNX run) must not abort the command.
                        match pipeline::embed_pending(&mut store, embedder.as_ref()) {
                            Ok(er) => {
                                println!("embedded: {}", er.embedded)
                            }
                            Err(e) => {
                                eprintln!(
                                    "vexus: embedding failed ({e:#}); index is structural-only"
                                );
                                println!("embeddings: skipped (embed error, see stderr)");
                            }
                        }
                    }
                    None => {
                        let reason = if std::env::var("VEXUS_EMBEDDER").as_deref() == Ok("none") {
                            "VEXUS_EMBEDDER=none"
                        } else {
                            "unavailable, see stderr"
                        };
                        println!("embeddings: skipped ({reason})");
                    }
                }
            }
            // A network round-trip is acceptable here — indexing already
            // took orders of magnitude longer, and the result only surfaces
            // on the NEXT invocation (status/serve read the cache, never
            // the network). No-ops inside the TTL or when disabled.
            vexus_mcp::update::refresh_if_stale();
            if let Some(notice) = vexus_mcp::update::notice() {
                eprintln!("vexus: {notice}");
            }
        }
        Cmd::Status { path } => {
            let root = path.unwrap_or_else(|| PathBuf::from("."));
            if !db_path(&root).exists() {
                println!("no index — run: vexus index");
                return Ok(());
            }
            let store = vexus_core::Store::open(&db_path(&root))?;
            // Renders through the exact same `vexus_mcp::state::status_text`
            // the MCP `status` tool uses — CLI/MCP parity is structural
            // (one format string, not two hand-copied ones). `role: None`:
            // the `role:` line is an in-`serve` claim ("I am the writer/
            // reader for as long as this process runs"), which a one-shot
            // command briefly touching the lock and letting go isn't
            // entitled to make — see `status_text`'s doc comment.
            let text = vexus_mcp::state::status_text(&store, None)?;

            // Instead, report whether a `vexus serve` is currently running
            // at all, via a probe-and-release of the same advisory
            // `.vexus/lock`: winning it means nothing currently holds it
            // (so no server is running); losing it means something else
            // does. Drop the lock IMMEDIATELY on the winning path — there is
            // a microscopic race between that drop and the `println!` below
            // (a `vexus serve` could start in between, making this line
            // stale the instant it's printed), which is fine for a
            // best-effort diagnostic command but is exactly why the lock
            // must never be held across the print, let alone longer.
            let serve_line = match vexus_watch::WriterLock::try_acquire(&root)? {
                Some(lock) => {
                    drop(lock);
                    "serve: not running"
                }
                None => "serve: running (another process is keeping the index fresh)",
            };
            println!("{text}\n{serve_line}");
        }
        Cmd::Search { query, path, limit } => {
            let root = path.unwrap_or_else(|| PathBuf::from("."));
            if !db_path(&root).exists() {
                println!("no index — run: vexus index");
                return Ok(());
            }
            let store = vexus_core::Store::open(&db_path(&root))?;
            let (query_vec, knn_floor) =
                embed_for_query(&store, &query, vexus_embed::select::make_embedder);
            let (hits, outcome) =
                store.search_hybrid_scored(&query, query_vec.as_deref(), knn_floor, limit)?;
            if outcome == vexus_core::search::SearchOutcome::WeakVectorOnly {
                println!("weak match — nothing indexed clearly matches; nearest neighbors only:");
            }
            for h in hits {
                let qual = h.qualname.unwrap_or_else(|| "(preamble)".into());
                println!(
                    "{}  {}:{}-{}  {:.2}\n    {}",
                    qual, h.path, h.start_line, h.end_line, h.score, h.excerpt
                );
            }
        }
        Cmd::Serve { path } => {
            let root = path.unwrap_or_else(|| PathBuf::from("."));
            vexus_mcp::serve(root)?;
        }
        Cmd::Init {
            agent,
            force,
            mcp,
            path,
        } => {
            let root = path.unwrap_or_else(|| PathBuf::from("."));
            init_steering_packs(&agent, &root, force, mcp)?;
        }
        Cmd::Hook { name } => {
            run_hook(&name)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(m: &McpMerge) -> serde_json::Value {
        match m {
            McpMerge::Write(s) => serde_json::from_str(s).unwrap(),
            other => panic!("expected Write, got {other:?}"),
        }
    }

    #[test]
    fn merge_creates_file_from_scratch() {
        let v = parsed(&merge_mcp_json(None, false).unwrap());
        assert_eq!(v["mcpServers"]["vexus"]["command"], "vexus");
        assert_eq!(
            v["mcpServers"]["vexus"]["args"],
            serde_json::json!(["serve", "."])
        );
    }

    #[test]
    fn merge_preserves_unrelated_keys_and_servers() {
        let existing = r#"{"zeta": 1, "mcpServers": {"other": {"command": "x"}}, "alpha": 2}"#;
        let m = merge_mcp_json(Some(existing), false).unwrap();
        let v = parsed(&m);
        assert_eq!(v["zeta"], 1);
        assert_eq!(v["alpha"], 2);
        assert_eq!(v["mcpServers"]["other"]["command"], "x");
        assert_eq!(v["mcpServers"]["vexus"]["command"], "vexus");
        // key order preserved, not alphabetized: user's file shouldn't be
        // reshuffled just because vexus touched it
        let McpMerge::Write(s) = m else {
            unreachable!()
        };
        let zeta = s.find("zeta").unwrap();
        let alpha = s.find("alpha").unwrap();
        assert!(zeta < alpha, "key order must be preserved: {s}");
    }

    #[test]
    fn merge_is_idempotent() {
        let McpMerge::Write(first) = merge_mcp_json(None, false).unwrap() else {
            unreachable!()
        };
        assert!(matches!(
            merge_mcp_json(Some(&first), false).unwrap(),
            McpMerge::AlreadyConfigured
        ));
    }

    #[test]
    fn merge_keeps_a_differing_vexus_entry_without_force() {
        let existing = r#"{"mcpServers": {"vexus": {"command": "custom"}}}"#;
        assert!(matches!(
            merge_mcp_json(Some(existing), false).unwrap(),
            McpMerge::KeptExisting
        ));
    }

    #[test]
    fn merge_replaces_a_differing_vexus_entry_with_force() {
        let existing = r#"{"mcpServers": {"vexus": {"command": "custom"}}}"#;
        let v = parsed(&merge_mcp_json(Some(existing), true).unwrap());
        assert_eq!(v["mcpServers"]["vexus"]["command"], "vexus");
    }

    #[test]
    fn merge_refuses_to_touch_malformed_json() {
        assert!(merge_mcp_json(Some("{not json"), false).is_err());
        assert!(merge_mcp_json(Some("[1, 2]"), false).is_err());
        assert!(merge_mcp_json(Some(r#"{"mcpServers": 42}"#), false).is_err());
    }
}
