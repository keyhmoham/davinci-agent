use dfae_core::text::content_tokens;
use dfae_core::Embedder;

/// Deterministic feature-hashing embedder (unigrams + bigrams, signed hashing,
/// L2-normalised). No model download, no network, stable across processes —
/// good enough for skill/memory retrieval, and swappable via [`Embedder`].
#[derive(Clone)]
pub struct HashEmbedder {
    dim: usize,
}

impl HashEmbedder {
    pub fn new(dim: usize) -> Self {
        Self { dim }
    }
}

impl Default for HashEmbedder {
    fn default() -> Self {
        Self::new(256)
    }
}

fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    // final avalanche
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51afd7ed558ccd);
    h ^= h >> 33;
    h
}

impl Embedder for HashEmbedder {
    fn dim(&self) -> usize {
        self.dim
    }

    fn embed(&self, text: &str) -> Vec<f32> {
        let toks = content_tokens(text);
        let mut v = vec![0f32; self.dim];
        let mut add = |feature: String, w: f32| {
            let h = fnv1a(&feature);
            let idx = (h % self.dim as u64) as usize;
            let sign = if (h >> 40) & 1 == 0 { 1.0 } else { -1.0 };
            v[idx] += sign * w;
        };
        for t in &toks {
            add(format!("u:{t}"), 1.0);
        }
        for w in toks.windows(2) {
            add(format!("b:{} {}", w[0], w[1]), 0.7);
        }
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for x in &mut v {
                *x /= norm;
            }
        }
        v
    }
}

pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>().clamp(-1.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn similar_texts_score_higher() {
        let e = HashEmbedder::default();
        let a = e.embed("list the files in the workspace directory");
        let b = e.embed("show files in the workspace folder directory");
        let c = e.embed("calculate the square root of nine");
        assert!(cosine(&a, &b) > cosine(&a, &c) + 0.2, "{} {}", cosine(&a, &b), cosine(&a, &c));
        assert!((cosine(&a, &a) - 1.0).abs() < 1e-4);
    }

    #[test]
    fn deterministic() {
        let e = HashEmbedder::default();
        assert_eq!(e.embed("hello world"), e.embed("hello world"));
    }
}
