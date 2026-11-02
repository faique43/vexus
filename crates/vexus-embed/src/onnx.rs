use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use anyhow::{Context, Result};
use ndarray::Array2;
use ort::session::Session;
use ort::value::Tensor;
use tokenizers::Tokenizer;

use crate::download::ModelManifest;
use crate::embedder::{l2_normalize, Embedder};

/// Embedder backed by a local ONNX model (e.g. jina-code-v2, quantized) run through `ort`.
///
/// `Session::run` requires `&mut Session`, but `Embedder::embed` takes `&self` (the trait is
/// `Send + Sync` so callers can share one embedder across threads) — each session is therefore
/// wrapped in a `Mutex` to give each call exclusive, serialized access.
pub struct OnnxEmbedder {
    id: String,
    dim: usize,
    sessions: Vec<Mutex<Session>>,
    tokenizer: Tokenizer,
}

const MAX_TOKENS: usize = 512;

/// Threads per session when several run at once. Across a sweep on an 18-core
/// machine, 4 sessions of 3 threads took 14.5s on Express against 27.4s for one
/// session on ONNX's own default, for 8% more CPU.
const THREADS_PER_SESSION: usize = 3;

impl OnnxEmbedder {
    pub fn load(model_dir: &Path, manifest: &ModelManifest) -> Result<Self> {
        Self::load_sessions(model_dir, manifest, 1)
    }

    /// Loads `sessions` copies of the model so `embed` can run that many texts
    /// at once. Each one holds about 300 MB for as long as the embedder lives,
    /// so this is for short runs with a lot to embed, not for a server.
    pub fn load_sessions(
        model_dir: &Path,
        manifest: &ModelManifest,
        sessions: usize,
    ) -> Result<Self> {
        let model_path = model_dir.join("model_quantized.onnx");
        let tok_path = model_dir.join("tokenizer.json");
        let threads = (sessions > 1).then_some(THREADS_PER_SESSION);
        let sessions = (0..sessions)
            .map(|_| open_session(&model_path, threads).map(Mutex::new))
            .collect::<Result<Vec<_>>>()?;
        let mut tokenizer = Tokenizer::from_file(&tok_path)
            .map_err(|e| anyhow::anyhow!("load tokenizer {}: {e}", tok_path.display()))?;
        tokenizer
            .with_truncation(Some(tokenizers::TruncationParams {
                max_length: MAX_TOKENS,
                ..Default::default()
            }))
            .map_err(|e| anyhow::anyhow!("truncation: {e}"))?;
        Ok(Self {
            id: manifest.id.to_string(),
            dim: manifest.dim,
            sessions,
            tokenizer,
        })
    }
}

fn open_session(model_path: &Path, threads: Option<usize>) -> Result<Session> {
    let mut builder = Session::builder().context("build onnx session")?;
    if let Some(threads) = threads {
        builder = builder
            .with_intra_threads(threads)
            .map_err(|e| anyhow::anyhow!("set onnx threads: {e}"))?;
    }
    builder
        .commit_from_file(model_path)
        .with_context(|| format!("load onnx model {}", model_path.display()))
}

impl OnnxEmbedder {
    /// One text per inference. Texts sharing a run change each other's vector
    /// (cosine down to 0.992 on this model), while queries are always embedded
    /// alone. Sharing also buys no speed on CPU: 1 and 32 cost the same wall
    /// time, and 32 uses five times the memory.
    fn embed_one(&self, session: &Mutex<Session>, text: &str) -> Result<Vec<f32>> {
        let encoding = self
            .tokenizer
            .encode(text, true)
            .map_err(|e| anyhow::anyhow!("tokenize: {e}"))?;
        let len = encoding.get_ids().len();
        let ids: Vec<i64> = encoding.get_ids().iter().map(|&id| id as i64).collect();
        let mask: Vec<i64> = encoding
            .get_attention_mask()
            .iter()
            .map(|&m| m as i64)
            .collect();

        let mut session = session
            .lock()
            .map_err(|_| anyhow::anyhow!("onnx session mutex poisoned"))?;
        let outputs = session.run(ort::inputs![
            "input_ids" => Tensor::from_array(Array2::from_shape_vec((1, len), ids)?)?,
            "attention_mask" => Tensor::from_array(Array2::from_shape_vec((1, len), mask.clone())?)?,
        ])?;
        // Model has a single output "last_hidden_state": (1, seq, hidden).
        let hidden = outputs[0].try_extract_array::<f32>()?;

        let mut pooled = vec![0f32; self.dim];
        let mut count = 0f32;
        for (j, _) in mask.iter().enumerate().filter(|(_, &m)| m == 1) {
            count += 1.0;
            for (d, x) in pooled.iter_mut().enumerate() {
                *x += hidden[[0, j, d]];
            }
        }
        if count > 0.0 {
            for x in pooled.iter_mut() {
                *x /= count;
            }
        }
        l2_normalize(&mut pooled);
        Ok(pooled)
    }
}

/// Default KNN floor for jina-code-v2, as an L2 distance over unit vectors
/// (sqlite-vec's vec0 default metric; d = sqrt(2 - 2·cos)). 1.1 ≈ cosine
/// 0.4 — lenient on purpose: it only exists to shed candidates that are
/// genuinely unrelated to the query, which on a small corpus is most of it
/// (vec0's `k = N` returns the N nearest regardless of distance, so a
/// 30-file repo hands RRF its entire corpus for every query). Calibration
/// override: `VEXUS_KNN_FLOOR` (see `effective_distance_floor`).
const JINA_CODE_V2_KNN_FLOOR: f64 = 1.1;

impl Embedder for OnnxEmbedder {
    fn id(&self) -> &str {
        &self.id
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn distance_floor(&self) -> Option<f64> {
        Some(JINA_CODE_V2_KNN_FLOOR)
    }

    fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        // Each session takes the next text until none are left, so a slow
        // long chunk holds up one worker and not the others.
        let next = AtomicUsize::new(0);
        let work = |session: &Mutex<Session>| -> Result<Vec<(usize, Vec<f32>)>> {
            let mut done = Vec::new();
            loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(text) = texts.get(i) else {
                    return Ok(done);
                };
                done.push((i, self.embed_one(session, text)?));
            }
        };
        let mut done = Vec::with_capacity(texts.len());
        std::thread::scope(|scope| -> Result<()> {
            let workers: Vec<_> = self
                .sessions
                .iter()
                .map(|session| scope.spawn(|| work(session)))
                .collect();
            for worker in workers {
                let finished = worker
                    .join()
                    .map_err(|_| anyhow::anyhow!("embedding worker panicked"))?;
                done.extend(finished?);
            }
            Ok(())
        })?;
        done.sort_by_key(|(i, _)| *i);
        Ok(done.into_iter().map(|(_, vector)| vector).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedder::Embedder;

    /// Requires the real model on disk; run manually:
    /// cargo test -p vexus-embed --release -- --ignored onnx_real
    #[test]
    #[ignore]
    fn onnx_real_model_embeds_code_sensibly() {
        let home = std::env::var("HOME").unwrap();
        let dir = std::path::Path::new(&home).join(".vexus/models/jina-code-v2-q");
        let e = OnnxEmbedder::load(&dir, &crate::download::JINA_CODE_V2).unwrap();
        let v = e
            .embed(&[
                "def add(a, b):\n    return a + b",
                "def sum_two(x, y):\n    return x + y",
                "class HttpServer:\n    def listen(self, port): ...",
            ])
            .unwrap();
        let sim = |a: &[f32], b: &[f32]| -> f32 { a.iter().zip(b).map(|(x, y)| x * y).sum() };
        // two add-functions more similar to each other than to the server class
        assert!(sim(&v[0], &v[1]) > sim(&v[0], &v[2]));
        assert_eq!(v[0].len(), 768);
    }

    /// Queries are embedded alone, so a chunk's vector must not depend on
    /// which chunks shared its call.
    #[test]
    #[ignore]
    fn onnx_real_model_vector_ignores_its_neighbours() {
        let home = std::env::var("HOME").unwrap();
        let dir = std::path::Path::new(&home).join(".vexus/models/jina-code-v2-q");
        let e = OnnxEmbedder::load(&dir, &crate::download::JINA_CODE_V2).unwrap();
        let short = "def add(a, b):\n    return a + b";
        let long = "class HttpServer:\n    def listen(self, port): ...\n".repeat(40);

        let alone = e.embed(&[short]).unwrap().remove(0);
        let beside_long = e.embed(&[short, &long]).unwrap().remove(0);

        assert_eq!(alone, beside_long);
    }

    #[test]
    #[ignore]
    fn onnx_real_model_sessions_match_a_single_session() {
        let home = std::env::var("HOME").unwrap();
        let dir = std::path::Path::new(&home).join(".vexus/models/jina-code-v2-q");
        let one = OnnxEmbedder::load(&dir, &crate::download::JINA_CODE_V2).unwrap();
        let four = OnnxEmbedder::load_sessions(&dir, &crate::download::JINA_CODE_V2, 4).unwrap();
        let texts: Vec<String> = (0..16)
            .map(|i| format!("def f{i}(x):\n    return x + {i}\n"))
            .collect();
        let texts: Vec<&str> = texts.iter().map(String::as_str).collect();

        assert_eq!(one.embed(&texts).unwrap(), four.embed(&texts).unwrap());
    }

    #[test]
    fn load_fails_cleanly_on_missing_model() {
        let dir = tempfile::tempdir().unwrap();
        assert!(OnnxEmbedder::load(dir.path(), &crate::download::JINA_CODE_V2).is_err());
    }
}
