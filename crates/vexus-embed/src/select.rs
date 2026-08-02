//! Selects which `Embedder` a run uses, driven by the `VEXUS_EMBEDDER` env var.
//!
//! Moved here (from `vexus-cli`) so both the CLI and `vexus-mcp`'s `serve`
//! startup path share one selection policy instead of two copies drifting
//! apart.

use crate::{Embedder, MockEmbedder};

/// The user's home directory, without pulling in a `dirs`-style crate:
/// `HOME` on Unix, falling back to `USERPROFILE` on Windows. `None` if
/// neither is set (e.g. a stripped-down container).
#[cfg(feature = "onnx")]
fn home_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from)
}

/// Downloads (if needed) and loads the default ONNX model, ignoring
/// `VEXUS_EMBEDDER` entirely.
///
/// [`make_embedder`] treats a failure here as "degrade to structural-only";
/// callers that specifically want the real model — the perf harness's
/// `--real` mode, whose whole purpose is measuring what embedding costs —
/// need the error instead of a silent downgrade to a different measurement.
#[cfg(feature = "onnx")]
pub fn real_embedder() -> anyhow::Result<Box<dyn Embedder>> {
    use anyhow::Context;
    let home = home_dir().context("no HOME/USERPROFILE to resolve the model cache from")?;
    let models = home.join(".vexus/models");
    let dir = crate::download::ensure_model(&crate::JINA_CODE_V2, &models)?;
    let embedder = crate::OnnxEmbedder::load(&dir, &crate::JINA_CODE_V2)?;
    Ok(Box::new(embedder))
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
        #[cfg(feature = "onnx")]
        _ => match real_embedder() {
            Ok(e) => Some(e),
            Err(e) => {
                eprintln!("vexus: embeddings unavailable ({e:#}); running structural-only");
                None
            }
        },
        // Structural-only build: the ONNX runtime is compiled out entirely
        // (targets it has no prebuilt binaries for — Intel macOS,
        // glibc < 2.39, musl). Same graceful degradation path as a runtime
        // load failure, just decided at compile time.
        #[cfg(not(feature = "onnx"))]
        _ => {
            eprintln!(
                "vexus: this build has no embedding runtime (structural-only build); \
                 keyword+graph search only"
            );
            None
        }
    }
}
