use std::path::Path;
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
/// `Send + Sync` so callers can share one embedder across threads) — the session is therefore
/// wrapped in a `Mutex` to give each call exclusive, serialized access.
pub struct OnnxEmbedder {
    id: String,
    dim: usize,
    session: Mutex<Session>,
    tokenizer: Tokenizer,
}

const MAX_TOKENS: usize = 512;

impl OnnxEmbedder {
    pub fn load(model_dir: &Path, manifest: &ModelManifest) -> Result<Self> {
        let model_path = model_dir.join("model_quantized.onnx");
        let tok_path = model_dir.join("tokenizer.json");
        let session = Session::builder()
            .context("build onnx session")?
            .commit_from_file(&model_path)
            .with_context(|| format!("load onnx model {}", model_path.display()))?;
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
            session: Mutex::new(session),
            tokenizer,
        })
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
        if texts.is_empty() {
            return Ok(vec![]);
        }
        let encodings = self
            .tokenizer
            .encode_batch(texts.to_vec(), true)
            .map_err(|e| anyhow::anyhow!("tokenize: {e}"))?;
        let max_len = encodings
            .iter()
            .map(|e| e.get_ids().len())
            .max()
            .unwrap_or(1);
        let n = texts.len();

        let mut ids = Array2::<i64>::zeros((n, max_len));
        let mut mask = Array2::<i64>::zeros((n, max_len));
        for (i, enc) in encodings.iter().enumerate() {
            for (j, (&id, &m)) in enc
                .get_ids()
                .iter()
                .zip(enc.get_attention_mask())
                .enumerate()
            {
                ids[[i, j]] = id as i64;
                mask[[i, j]] = m as i64;
            }
        }

        let mut session = self
            .session
            .lock()
            .map_err(|_| anyhow::anyhow!("onnx session mutex poisoned"))?;
        let outputs = session.run(ort::inputs![
            "input_ids" => Tensor::from_array(ids)?,
            "attention_mask" => Tensor::from_array(mask.clone())?,
        ])?;
        // Model has a single output "last_hidden_state": (n, seq, hidden).
        let hidden = outputs[0].try_extract_array::<f32>()?;

        let mut result = Vec::with_capacity(n);
        for i in 0..n {
            let mut pooled = vec![0f32; self.dim];
            let mut count = 0f32;
            for j in 0..max_len {
                if mask[[i, j]] == 1 {
                    count += 1.0;
                    for d in 0..self.dim {
                        pooled[d] += hidden[[i, j, d]];
                    }
                }
            }
            if count > 0.0 {
                for x in pooled.iter_mut() {
                    *x /= count;
                }
            }
            l2_normalize(&mut pooled);
            result.push(pooled);
        }
        Ok(result)
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

    #[test]
    fn load_fails_cleanly_on_missing_model() {
        let dir = tempfile::tempdir().unwrap();
        assert!(OnnxEmbedder::load(dir.path(), &crate::download::JINA_CODE_V2).is_err());
    }
}
