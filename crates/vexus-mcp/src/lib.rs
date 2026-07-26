//! Vexus MCP server: exposes the Plan 1+2 index (structural + semantic) as
//! MCP tools over stdio for `vexus serve`.

pub mod bundle;
pub mod format;
pub mod server;
pub mod state;
pub mod tools;

use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context, Result};
use rmcp::ServiceExt;
use vexus_watch::pipeline;

use crate::state::AppState;

/// Entry point for `vexus serve [PATH]`. Blocks for the lifetime of the
/// stdio MCP session (until the client disconnects); builds its own tokio
/// runtime internally so callers don't need to be async.
pub fn serve(root: PathBuf) -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(serve_async(root))
}

async fn serve_async(root: PathBuf) -> Result<()> {
    let db_path = root.join(".vexus/index.db");

    // Startup indexing needs a writer. It's opened, used, and dropped here —
    // tool handlers never see it; they only ever get the read-only Store
    // opened below. The watcher (a later task) takes over as the long-lived
    // writer.
    {
        let mut store = vexus_core::Store::open(&db_path)
            .with_context(|| format!("failed to open index at {}", db_path.display()))?;
        // Self-ignoring dir, like target/ — same convention as the CLI's
        // `open_store`. Best-effort: a stray `.gitignore` we can't write (e.g.
        // read-only checkout) is a nuisance, not a reason to refuse to serve.
        let _ = std::fs::write(root.join(".vexus/.gitignore"), "*\n");

        if store.counts()?.files == 0 {
            eprintln!("vexus: no index found for {root:?} — building one now...");
            // A failed initial index degrades to serving an empty/partial index
            // rather than refusing to start at all — `status` still reports the
            // truth, and tools simply have less to work with until the caller
            // re-runs `vexus index`.
            match pipeline::index_repo(&root, &mut store)
                .with_context(|| format!("failed to index repo at {}", root.display()))
            {
                Ok(report) => {
                    eprintln!(
                        "vexus: indexed {} files ({} failed)",
                        report.indexed,
                        report.failed.len()
                    );
                    if store.vec_available() {
                        match vexus_embed::select::make_embedder() {
                            Some(embedder) => {
                                store.set_model(embedder.id(), embedder.dim())?;
                                match pipeline::embed_pending(&mut store, embedder.as_ref()) {
                                    Ok(er) => eprintln!(
                                        "vexus: embedded {} chunks (cache hits: {})",
                                        er.embedded, er.from_cache
                                    ),
                                    Err(e) => eprintln!(
                                        "vexus: embedding failed ({e:#}); serving structural-only"
                                    ),
                                }
                            }
                            None => {
                                eprintln!("vexus: embeddings unavailable; serving structural-only")
                            }
                        }
                    } else {
                        eprintln!(
                            "vexus: embeddings skipped (sqlite-vec unavailable); serving structural-only"
                        );
                    }
                }
                Err(e) => {
                    eprintln!(
                        "vexus: initial indexing failed ({e:#}); serving with an empty index"
                    );
                }
            }
        }
    } // writer dropped here

    // Tool handlers only ever read; opening read-only keeps concurrent
    // access (this process's watcher, in a later task, and any other reader)
    // from contending with tools over a write lock, and makes accidental
    // writes from a tool handler fail loudly instead of corrupting state.
    let store = vexus_core::Store::open_read_only(&db_path)
        .with_context(|| format!("failed to open read-only index at {}", db_path.display()))?;

    let state = Arc::new(AppState {
        store: Mutex::new(store),
        embedder: OnceLock::new(),
        root,
        last_generation: AtomicU64::new(0),
    });

    let server = server::VexusServer::new(state);
    server
        .serve(rmcp::transport::stdio())
        .await?
        .waiting()
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A store-open failure is the one startup error that must still be
    /// fatal (a `.vexus/index.db` we can't even open isn't something
    /// `serve` can degrade around) — but it must fail with a message naming
    /// *what* failed to open, not a bare io error. Forces the failure by
    /// pre-creating `.vexus` as a plain file, so `Store::open`'s
    /// `create_dir_all(root/.vexus)` errors out before touching stdio (this
    /// runs synchronously to completion — it never reaches the point where
    /// `serve` would block on real stdin).
    #[test]
    fn serve_wraps_store_open_failure_with_the_index_path() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join(".vexus"), b"not a directory").unwrap();

        let err = serve(root.clone()).expect_err("Store::open must fail when .vexus is a file");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("failed to open index"),
            "expected the wrapped context to name the failing step: {msg}"
        );
        assert!(
            msg.contains("index.db"),
            "expected the error to name the index path: {msg}"
        );
    }
}
