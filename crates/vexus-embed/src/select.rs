//! Selects which `Embedder` a run uses, driven by the `VEXUS_EMBEDDER` env var.
//!
//! Moved here (from `vexus-cli`) so both the CLI and `vexus-mcp`'s `serve`
//! startup path share one selection policy instead of two copies drifting
//! apart.

use crate::{Embedder, MockEmbedder, OnnxEmbedder};

/// The user's home directory, without pulling in a `dirs`-style crate:
/// `HOME` on Unix, falling back to `USERPROFILE` on Windows. `None` if
/// neither is set (e.g. a stripped-down container).
fn home_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from)
}

/// Selects the embedder for this run from `VEXUS_EMBEDDER`:
/// - `mock` → deterministic `MockEmbedder` (tests/CI, no model download)
/// - `none` → structural-only, no embeddings
/// - unset  → download/load the default ONNX model; any failure degrades to
///   `None` (structural-only) rather than failing the whole command.
pub fn make_embedder() -> Option<Box<dyn Embedder>> {
    match std::env::var("VEXUS_EMBEDDER").as_deref() {
        Ok("mock") => Some(Box::new(MockEmbedder)),
        Ok("none") => None,
        _ => {
            let Some(home) = home_dir() else {
                eprintln!(
                    "vexus: embeddings unavailable (no HOME/USERPROFILE); running structural-only"
                );
                return None;
            };
            let models = home.join(".vexus/models");
            match crate::download::ensure_model(&crate::JINA_CODE_V2, &models)
                .and_then(|dir| OnnxEmbedder::load(&dir, &crate::JINA_CODE_V2))
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
