mod pipeline;

use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::{Parser, Subcommand};
use vexus_embed::Embedder;

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

/// The user's home directory, without pulling in a `dirs`-style crate:
/// `HOME` on Unix, falling back to `USERPROFILE` on Windows. `None` if
/// neither is set (e.g. a stripped-down container).
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// Selects the embedder for this run from `VEXUS_EMBEDDER`:
/// - `mock` → deterministic `MockEmbedder` (tests/CI, no model download)
/// - `none` → structural-only, no embeddings
/// - unset  → download/load the default ONNX model; any failure degrades to
///   `None` (structural-only) rather than failing the whole command.
fn make_embedder() -> Option<Box<dyn Embedder>> {
    match std::env::var("VEXUS_EMBEDDER").as_deref() {
        Ok("mock") => Some(Box::new(vexus_embed::MockEmbedder)),
        Ok("none") => None,
        _ => {
            let Some(home) = home_dir() else {
                eprintln!(
                    "vexus: embeddings unavailable (no HOME/USERPROFILE); running structural-only"
                );
                return None;
            };
            let models = home.join(".vexus/models");
            match vexus_embed::download::ensure_model(&vexus_embed::JINA_CODE_V2, &models)
                .and_then(|dir| vexus_embed::OnnxEmbedder::load(&dir, &vexus_embed::JINA_CODE_V2))
            {
                Ok(e) => Some(Box::new(e)),
                Err(e) => {
                    eprintln!("vexus: embeddings unavailable ({e:#}); running structural-only");
                    None
                }
            }
        }
    }
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
                match make_embedder() {
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
            let c = store.counts()?;
            println!(
                "files: {}  symbols: {}  edges: {}  chunks: {}",
                c.files, c.symbols, c.edges, c.chunks
            );
            let model_id = store.meta("model_id")?.unwrap_or_else(|| "none".into());
            let backlog = store.embed_backlog()?;
            let vec_status = if store.vec_available() {
                "available"
            } else {
                "unavailable"
            };
            println!(
                "model: {}  embed backlog: {}  vec: {}",
                model_id, backlog, vec_status
            );
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
            let indexed_model = (
                store.meta("model_id").ok().flatten(),
                store.meta("model_dim").ok().flatten(),
            );
            let query_vec = make_embedder().and_then(|embedder| {
                let same_model = indexed_model.0.as_deref() == Some(embedder.id())
                    && indexed_model.1.as_deref() == Some(embedder.dim().to_string().as_str());
                if !same_model {
                    return None;
                }
                embedder
                    .embed(&[query.as_str()])
                    .ok()
                    .and_then(|mut v| v.pop())
            });
            for h in store.search_hybrid(&query, query_vec.as_deref(), limit)? {
                let qual = h.qualname.unwrap_or_else(|| "(preamble)".into());
                println!(
                    "{}  {}:{}-{}  {:.2}\n    {}",
                    qual, h.path, h.start_line, h.end_line, h.score, h.excerpt
                );
            }
        }
    }
    Ok(())
}
