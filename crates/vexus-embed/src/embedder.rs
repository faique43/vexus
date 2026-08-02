use anyhow::Result;

pub trait Embedder: Send + Sync {
    /// Stable identifier stored in meta.model_id (e.g. "jina-code-v2-q", "mock").
    fn id(&self) -> &str;
    fn dim(&self) -> usize;
    /// One L2-normalized vector per input text, in order.
    fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>>;
    /// L2-distance ceiling above which a KNN candidate should not count as
    /// a real semantic match (`None` = no floor, keep every candidate).
    /// A property of the embedder, not of retrieval: the distance
    /// distribution is model-specific, and the mock embedder's hash vectors
    /// have no meaningful notion of "near" at all — so it stays `None`
    /// there, which also keeps the mock-mode eval baseline untouched.
    fn distance_floor(&self) -> Option<f64> {
        None
    }
}

/// The floor retrieval should actually use for `embedder`: the
/// `VEXUS_KNN_FLOOR` env var when set (a calibration override; `0` disables
/// the floor entirely), else the embedder's own `distance_floor()`.
pub fn effective_distance_floor(embedder: &dyn Embedder) -> Option<f64> {
    if let Ok(v) = std::env::var("VEXUS_KNN_FLOOR") {
        if let Ok(f) = v.trim().parse::<f64>() {
            return if f > 0.0 { Some(f) } else { None };
        }
    }
    embedder.distance_floor()
}

pub struct MockEmbedder;

pub const MOCK_DIM: usize = 768;

impl Embedder for MockEmbedder {
    fn id(&self) -> &str {
        "mock"
    }

    fn dim(&self) -> usize {
        MOCK_DIM
    }

    fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        Ok(texts
            .iter()
            .map(|t| {
                let mut hasher = blake3::Hasher::new();
                hasher.update(t.as_bytes());
                let mut xof = hasher.finalize_xof();
                let mut bytes = vec![0u8; MOCK_DIM * 4];
                xof.fill(&mut bytes);
                let mut v: Vec<f32> = bytes
                    .chunks_exact(4)
                    .map(|b| {
                        let u = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
                        (u as f32 / u32::MAX as f32) * 2.0 - 1.0
                    })
                    .collect();
                l2_normalize(&mut v);
                v
            })
            .collect())
    }
}

pub fn l2_normalize(v: &mut [f32]) {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_is_deterministic_normalized_and_distinct() {
        let m = MockEmbedder;
        assert_eq!(m.id(), "mock");
        assert_eq!(m.dim(), 768);
        let v = m
            .embed(&["fn foo() {}", "fn foo() {}", "class Bar:"])
            .unwrap();
        assert_eq!(v.len(), 3);
        assert_eq!(v[0], v[1]);
        assert_ne!(v[0], v[2]);
        for vec in &v {
            assert_eq!(vec.len(), 768);
            let norm: f32 = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
            assert!((norm - 1.0).abs() < 1e-4, "not normalized: {norm}");
        }
    }
}
