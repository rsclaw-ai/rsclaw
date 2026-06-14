//! Deterministic stub embedder. Derives each vector from
//! sha256(text), expands to `dimension()` f32 values normalised to
//! the unit hypersphere (so cosine similarity tests are sensible).

use anyhow::Result;
use sha2::{Digest, Sha256};

use super::KbEmbedder;

pub struct StubEmbedder {
    pub dimension: usize,
    pub id: String,
}

impl Default for StubEmbedder {
    fn default() -> Self {
        Self {
            dimension: 1024,
            id: "stub-sha256-1024".into(),
        }
    }
}

impl KbEmbedder for StubEmbedder {
    fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|t| self.embed_one(t)).collect())
    }

    fn dimension(&self) -> usize {
        self.dimension
    }

    fn embedder_id(&self) -> &str {
        &self.id
    }
}

impl StubEmbedder {
    fn embed_one(&self, text: &str) -> Vec<f32> {
        let mut v = Vec::with_capacity(self.dimension);
        let mut block = 0u32;
        while v.len() < self.dimension {
            let mut h = Sha256::new();
            h.update(text.as_bytes());
            h.update(block.to_be_bytes());
            let bytes = h.finalize();
            for chunk in bytes.chunks_exact(4) {
                if v.len() == self.dimension {
                    break;
                }
                let u = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                v.push((u as f64 / u32::MAX as f64 * 2.0 - 1.0) as f32);
            }
            block += 1;
        }
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-12);
        for x in &mut v {
            *x /= norm;
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dimension_default_is_1024() {
        let e = StubEmbedder::default();
        let v = e.embed_batch(&["hi".into()]).unwrap();
        assert_eq!(v[0].len(), 1024);
    }

    #[test]
    fn deterministic() {
        let e = StubEmbedder::default();
        let a = e.embed_batch(&["same".into()]).unwrap();
        let b = e.embed_batch(&["same".into()]).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn different_inputs_different_vectors() {
        let e = StubEmbedder::default();
        let v = e.embed_batch(&["a".into(), "b".into()]).unwrap();
        assert_ne!(v[0], v[1]);
    }

    #[test]
    fn batch_preserves_order() {
        let e = StubEmbedder::default();
        let inputs: Vec<String> = (0..5).map(|i| format!("text {i}")).collect();
        let v = e.embed_batch(&inputs).unwrap();
        assert_eq!(v.len(), 5);
        for (i, t) in inputs.iter().enumerate() {
            let single = e.embed_batch(std::slice::from_ref(t)).unwrap();
            assert_eq!(v[i], single[0]);
        }
    }

    #[test]
    fn vectors_are_unit_length() {
        let e = StubEmbedder::default();
        let v = &e.embed_batch(&["test".into()]).unwrap()[0];
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5, "got norm = {norm}");
    }
}
