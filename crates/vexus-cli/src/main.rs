use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::{Parser, Subcommand};
use vexus_watch::pipeline;

#[derive(Parser)]
#[command(
    name = "vexus",
    version,
    about = "Local code intelligence for coding agents"
)]
struct Cli {
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
    /// Initialize steering packs for an agent (claude-code, cursor, or generic)
    Init {
        #[arg(long)]
        agent: String,
        #[arg(long)]
        force: bool,
        path: Option<PathBuf>,
    },
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

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o755);
    std::fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<()> {
    Ok(())
}

fn init_steering_packs(agent: &str, root: &Path, force: bool) -> Result<()> {
    match agent {
        "claude-code" => {
            // `.claude/plugins/vexus` is never scanned by Claude Code — only
            // `.claude/skills/<name>/` is auto-discovered per session. A
            // full plugin bundle (`.claude-plugin/plugin.json`, `hooks/`,
            // `skills/`) dropped there still loads correctly, as a
            // skills-directory plugin, so the internal layout below is
            // unchanged — only the root it's written under moves.
            let plugin_root = root.join(".claude/skills/vexus");

            let plugin_json_content =
                include_str!("../../../packs/claude-code/.claude-plugin/plugin.json");
            let hooks_json_content = include_str!("../../../packs/claude-code/hooks/hooks.json");
            let nudge_sh_content = include_str!("../../../packs/claude-code/hooks/nudge-grep.sh");
            let skill_md_content = include_str!("../../../packs/claude-code/skills/vexus/SKILL.md");

            write_pack_file(
                &plugin_root.join(".claude-plugin/plugin.json"),
                plugin_json_content,
                force,
            )?;
            write_pack_file(
                &plugin_root.join("hooks/hooks.json"),
                hooks_json_content,
                force,
            )?;
            let nudge_script = plugin_root.join("hooks/nudge-grep.sh");
            write_pack_file(&nudge_script, nudge_sh_content, force)?;
            set_executable(&nudge_script)?;
            write_pack_file(
                &plugin_root.join("skills/vexus/SKILL.md"),
                skill_md_content,
                force,
            )?;

            println!(
                "\nClaude Code loads this from the LAUNCH directory's .claude/skills — start \
                 Claude Code from {} (or a directory it's later trusted from) for it to be \
                 picked up. The first session that finds it will show a workspace-trust \
                 dialog (it bundles a hook); accept it to enable the nudge + skill.",
                root.display()
            );
            println!("\nAdd this to .mcp.json:");
            println!(
                r#"{{ "mcpServers": {{ "vexus": {{ "command": "vexus", "args": ["serve", "."] }} }} }}"#
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
                match vexus_embed::select::make_embedder() {
                    Some(embedder) => {
                        store.set_model(embedder.id(), embedder.dim())?;
                        // Degrade, never die: structural indexing above already
                        // succeeded and was reported, so an embedding failure
                        // (e.g. a flaky ONNX run) must not abort the command.
                        match pipeline::embed_pending(&mut store, embedder.as_ref()) {
                            Ok(er) => {
                                println!(
                                    "embedded: {} (cache hits: {})",
                                    er.embedded, er.from_cache
                                )
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
            // Only embed the query if the selected embedder is the same one
            // the index was built with — a mismatch (different model, or a
            // different dimension) would otherwise feed a vector into
            // `search_hybrid`'s KNN lookup that doesn't match `vec_chunks`'
            // declared width, which sqlite-vec rejects as a hard error.
            // Falling back to keyword-only search here is exactly the
            // "degrade, never die" behavior a query-embed failure gets below.
            //
            // Read the indexed model from `meta` BEFORE constructing an
            // embedder: `make_embedder()`'s default path builds the full
            // ONNX embedder (sha256 over a ~150MB file plus an ONNX Runtime
            // session load), which is wasted work whenever `meta.model_id`
            // already tells us it can't match. `VEXUS_EMBEDDER=mock`/`none`
            // stay cheap either way, so only the default path needs gating.
            let indexed_model = (
                store.meta("model_id").ok().flatten(),
                store.meta("model_dim").ok().flatten(),
            );
            let embedder_env = std::env::var("VEXUS_EMBEDDER").ok();
            let worth_building = match embedder_env.as_deref() {
                Some("mock") | Some("none") => true,
                _ => {
                    indexed_model.0.is_none()
                        || indexed_model.0.as_deref() == Some(vexus_embed::JINA_CODE_V2.id)
                }
            };
            let query_vec = if worth_building {
                vexus_embed::select::make_embedder().and_then(|embedder| {
                    let same_model = indexed_model.0.as_deref() == Some(embedder.id())
                        && indexed_model.1.as_deref() == Some(embedder.dim().to_string().as_str());
                    if !same_model {
                        return None;
                    }
                    embedder
                        .embed(&[query.as_str()])
                        .ok()
                        .and_then(|mut v| v.pop())
                })
            } else {
                None
            };
            for h in store.search_hybrid(&query, query_vec.as_deref(), limit)? {
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
        Cmd::Init { agent, force, path } => {
            let root = path.unwrap_or_else(|| PathBuf::from("."));
            init_steering_packs(&agent, &root, force)?;
        }
    }
    Ok(())
}
