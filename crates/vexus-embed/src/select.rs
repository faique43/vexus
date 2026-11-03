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

/// A backlog below this stays on one session: a few saved files would not
/// repay loading more, at about 300 MB each.
#[cfg(feature = "onnx")]
const BULK_BACKLOG: i64 = 256;

/// Past four, sessions add memory and little speed. Express on 18 cores took
/// 14.5s at 1.4 GB with four, and 13.4s at 2.5 GB with nine.
#[cfg(feature = "onnx")]
const MAX_SESSIONS: usize = 4;

#[cfg(feature = "onnx")]
fn sessions_for(backlog: i64, cores: usize) -> usize {
    if backlog < BULK_BACKLOG {
        return 1;
    }
    (cores / 4).clamp(1, MAX_SESSIONS)
}

#[cfg(feature = "onnx")]
fn available_cores() -> usize {
    std::thread::available_parallelism().map_or(1, |cores| cores.get())
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
    real_embedder_with_sessions(1)
}

#[cfg(feature = "onnx")]
fn real_embedder_with_sessions(sessions: usize) -> anyhow::Result<Box<dyn Embedder>> {
    use anyhow::Context;
    let home = home_dir().context("no HOME/USERPROFILE to resolve the model cache from")?;
    let models = home.join(".vexus/models");
    let dir = crate::download::ensure_model(&crate::JINA_CODE_V2, &models)?;
    let embedder = crate::OnnxEmbedder::load_sessions(&dir, &crate::JINA_CODE_V2, sessions)?;
    Ok(Box::new(embedder))
}

/// [`make_embedder`] for a caller that can't wait: `None` unless the model is
/// already on disk, so it never downloads. A prompt hook must not start a
/// 160 MB fetch, or block a prompt on one.
pub fn cached_embedder() -> Option<Box<dyn Embedder>> {
    match std::env::var("VEXUS_EMBEDDER").as_deref() {
        Ok("mock") => Some(Box::new(MockEmbedder)),
        Ok("none") => None,
        #[cfg(feature = "onnx")]
        _ => {
            let models = home_dir()?.join(".vexus/models");
            let dir = crate::download::cached_model(&crate::JINA_CODE_V2, &models)?;
            let embedder =
                crate::OnnxEmbedder::load_sessions(&dir, &crate::JINA_CODE_V2, 1).ok()?;
            Some(Box::new(embedder))
        }
        #[cfg(not(feature = "onnx"))]
        _ => None,
    }
}

/// Selects the embedder for this run from `VEXUS_EMBEDDER`:
/// - `mock` → deterministic `MockEmbedder` (tests/CI, no model download)
/// - `none` → structural-only, no embeddings
/// - unset  → download/load the default ONNX model; any failure degrades to
///   `None` (structural-only) rather than failing the whole command.
pub fn make_embedder() -> Option<Box<dyn Embedder>> {
    make_embedder_for_backlog(0)
}

/// [`make_embedder`] for a one-shot run about to embed `backlog` chunks. Not
/// for a long-lived process: the extra sessions keep their memory until the
/// embedder is dropped.
#[cfg_attr(not(feature = "onnx"), allow(unused_variables))]
pub fn make_embedder_for_backlog(backlog: i64) -> Option<Box<dyn Embedder>> {
    match std::env::var("VEXUS_EMBEDDER").as_deref() {
        Ok("mock") => Some(Box::new(MockEmbedder)),
        Ok("none") => None,
        #[cfg(feature = "onnx")]
        _ => match real_embedder_with_sessions(sessions_for(backlog, available_cores())) {
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

#[cfg(all(test, feature = "onnx"))]
mod tests {
    use super::*;

    #[test]
    fn small_backlog_stays_on_one_session() {
        assert_eq!(sessions_for(0, 18), 1);
        assert_eq!(sessions_for(BULK_BACKLOG - 1, 18), 1);
    }

    #[test]
    fn big_backlog_scales_with_cores_up_to_the_cap() {
        assert_eq!(sessions_for(BULK_BACKLOG, 2), 1);
        assert_eq!(sessions_for(BULK_BACKLOG, 4), 1);
        assert_eq!(sessions_for(BULK_BACKLOG, 8), 2);
        assert_eq!(sessions_for(BULK_BACKLOG, 18), 4);
        assert_eq!(sessions_for(BULK_BACKLOG, 64), MAX_SESSIONS);
    }
}
