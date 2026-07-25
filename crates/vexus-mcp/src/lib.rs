//! Vexus MCP server: exposes the Plan 1+2 index (structural + semantic) as
//! MCP tools over stdio for `vexus serve`.

pub mod bundle;
pub mod format;
pub mod server;
pub mod state;
pub mod tools;

use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::Result;
use rmcp::ServiceExt;

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
    let mut store = vexus_core::Store::open(&db_path)?;
    // Self-ignoring dir, like target/ — same convention as the CLI's
    // `open_store`.
    std::fs::write(root.join(".vexus/.gitignore"), "*\n")?;

    if store.counts()?.files == 0 {
        eprintln!("vexus: no index found for {root:?} — building one now...");
        let report = vexus_embed::pipeline::index_repo(&root, &mut store)?;
        eprintln!(
            "vexus: indexed {} files ({} failed)",
            report.indexed,
            report.failed.len()
        );
        if store.vec_available() {
            match vexus_embed::select::make_embedder() {
                Some(embedder) => {
                    store.set_model(embedder.id(), embedder.dim())?;
                    match vexus_embed::pipeline::embed_pending(&mut store, embedder.as_ref()) {
                        Ok(er) => eprintln!(
                            "vexus: embedded {} chunks (cache hits: {})",
                            er.embedded, er.from_cache
                        ),
                        Err(e) => {
                            eprintln!("vexus: embedding failed ({e:#}); serving structural-only")
                        }
                    }
                }
                None => eprintln!("vexus: embeddings unavailable; serving structural-only"),
            }
        } else {
            eprintln!(
                "vexus: embeddings skipped (sqlite-vec unavailable); serving structural-only"
            );
        }
    }

    let state = Arc::new(AppState {
        store: Mutex::new(store),
        embedder: OnceLock::new(),
        root,
    });

    let server = server::VexusServer::new(state);
    server
        .serve(rmcp::transport::stdio())
        .await?
        .waiting()
        .await?;
    Ok(())
}
