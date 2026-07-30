//! Semantic search: ese embeddings + exact brute-force top-k, with an
//! optional anny HNSW accelerator for indexed corpora.

use crate::EMBED_DIM;
use anny::hnsw::Hnsw;
use anny::metric::{Cosine, Metric};
use rayon::prelude::*;

/// HNSW instantiation used for `.semgrep/hnsw.bin`.
/// M0=32 (M=16), compile-time top-K=128, EF_SEARCH=192, EF_BUILD=128.
pub type SemgrepHnsw = Hnsw<f32, Cosine, EMBED_DIM, 32, 128, 192, 128, 16>;

pub fn new_hnsw() -> SemgrepHnsw {
    SemgrepHnsw::new(Cosine, 0xB06)
}

/// Embed a batch of chunk texts (rayon-parallel inside ese for >=16 items).
pub fn embed_batch(texts: &[&str]) -> Vec<[f32; EMBED_DIM]> {
    ese::encode(texts)
}

pub fn embed_query(query: &str) -> [f32; EMBED_DIM] {
    ese::encode_single(query)
}

// ---------------------------------------------------------------------------
// Token-level embeddings (RESEARCH.md §9): ese pools by uniform mean, which
// lets boilerplate tokens drown identifiers. These helpers re-pool with
// SIF-style rarity weights and score candidates by MaxSim late interaction.
// ---------------------------------------------------------------------------

/// Default SIF smoothing constant (Arora et al.: a/(a + p(w))).
pub const SIF_A: f64 = 1e-3;

fn default_sif_a() -> f64 {
    SIF_A
}

/// Corpus token statistics for SIF weighting, stored in the index.
/// `a` is persisted so query-time pooling always matches build-time.
/// `mean` is the sample-estimated common component (--sif-center):
/// subtracted from every pooled vector, chunk and query alike.
#[derive(Default, serde::Serialize, serde::Deserialize)]
pub struct SifStats {
    pub freqs: std::collections::HashMap<String, u64>,
    pub total: u64,
    #[serde(default = "default_sif_a")]
    pub a: f64,
    #[serde(default)]
    pub mean: Option<Vec<f32>>,
}

impl SifStats {
    /// a/(a + p(w)); unseen tokens get the maximum weight (rare = strong).
    #[inline]
    pub fn weight(&self, token: &str) -> f32 {
        let p =
            self.freqs.get(token).map(|&c| c as f64 / self.total.max(1) as f64).unwrap_or(0.0);
        (self.a / (self.a + p)) as f32
    }

    /// Count `text`'s tokens into the stats (build-time pass 1).
    pub fn count(&mut self, text: &str) {
        ese::for_each_token(text, |tok| {
            *self.freqs.entry(tok.to_string()).or_insert(0) += 1;
            self.total += 1;
        });
    }

    pub fn merge(&mut self, other: SifStats) {
        for (t, c) in other.freqs {
            *self.freqs.entry(t).or_insert(0) += c;
        }
        self.total += other.total;
    }
}

/// SIF-weighted pooling: Σ w(tok)·v(tok) / Σ w(tok). Rare tokens dominate;
/// boilerplate nearly vanishes. Falls back to the zero vector for token-less
/// text (same contract as `encode_single` on empty input).
pub fn embed_sif(text: &str, sif: &SifStats) -> [f32; EMBED_DIM] {
    let mut out = [0.0f32; EMBED_DIM];
    let mut wsum = 0.0f32;
    ese::for_each_token_vector(text, |tok, v| {
        let w = sif.weight(tok);
        wsum += w;
        for (o, x) in out.iter_mut().zip(v.iter()) {
            *o += w * x;
        }
    });
    if wsum > 0.0 {
        for o in out.iter_mut() {
            *o /= wsum;
        }
        // Common-component removal: the direction every pooled vector
        // shares carries no discriminative signal; subtracting it is the
        // second half of the SIF recipe.
        if let Some(mean) = &sif.mean {
            for (o, m) in out.iter_mut().zip(mean.iter()) {
                *o -= m;
            }
        }
    }
    out
}

/// Per-word unit-normalized vectors, with each word's SIF weight when stats
/// are given (uniform weight 1.0 otherwise). Zero-vector words are skipped.
pub fn token_vectors(text: &str, sif: Option<&SifStats>) -> Vec<(f32, Vec<f32>)> {
    let mut out = Vec::new();
    ese::for_each_token_vector(text, |tok, v| {
        let mut v = v.to_vec();
        normalize(&mut v);
        if v.iter().any(|&x| x != 0.0) {
            out.push((sif.map_or(1.0, |s| s.weight(tok)), v));
        }
    });
    out
}

/// MaxSim late-interaction score: each query token finds its best-matching
/// chunk token; the (weighted) sum is the score. Higher is better. One strong
/// identifier match can't be averaged away by boilerplate — the failure mode
/// of pooled single-vector scoring.
pub fn maxsim(query_toks: &[(f32, Vec<f32>)], doc_toks: &[(f32, Vec<f32>)]) -> f32 {
    let mut score = 0.0f32;
    for (w, q) in query_toks {
        let mut best = f32::NEG_INFINITY;
        for (_, d) in doc_toks {
            // Unit vectors: dot = cosine similarity.
            let sim: f32 = q.iter().zip(d.iter()).map(|(a, b)| a * b).sum();
            if sim > best {
                best = sim;
            }
        }
        if best.is_finite() {
            score += w * best;
        }
    }
    score
}

/// Cosine distance (1 - cos). Lower is better; range [0, 2].
#[inline]
pub fn distance(a: &[f32], b: &[f32]) -> f32 {
    <Cosine as Metric<f32>>::distance(a, b)
}

/// Normalize to unit length in place (no-op for zero vectors).
pub fn normalize(v: &mut [f32]) {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 0.0 {
        for x in v {
            *x /= n;
        }
    }
}

/// Distance for *unit-normalized* vectors: `1 − a·b`, identical ordering and
/// value to cosine distance but one FMA stream instead of three. This is the
/// hot kernel of the brute-force scan (provenance: `rank:brute` was 72% of a
/// warm kernel-corpus query under Cosine).
#[inline]
pub fn dot_distance(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len().min(b.len());
    let (a, b) = (&a[..n], &b[..n]);
    let mut lanes = [0.0f32; 8];
    let mut i = 0;
    while i + 8 <= n {
        for l in 0..8 {
            lanes[l] += a[i + l] * b[i + l];
        }
        i += 8;
    }
    let mut d: f32 = lanes.iter().sum();
    while i < n {
        d += a[i] * b[i];
        i += 1;
    }
    1.0 - d
}

/// Quantize a unit vector to i8 (x → round(x·127)). On normalized vectors
/// the i8·i8 dot preserves cosine ordering to within noise, and the 4×
/// smaller matrix is the point: the brute scan is page-fault/IO bound
/// (provenance: 188k faults, 0.45× CPU util on the f32 matrix).
pub fn quantize_i8(v: &[f32]) -> Vec<i8> {
    v.iter().map(|&x| (x * 127.0).round().clamp(-127.0, 127.0) as i8).collect()
}

/// Distance for i8-quantized unit vectors: `1 − (a·b)/127²`, same range
/// convention as [`dot_distance`]. Integer accumulation autovectorizes well.
#[inline]
pub fn dot_distance_i8(a: &[i8], b: &[i8]) -> f32 {
    let n = a.len().min(b.len());
    let (a, b) = (&a[..n], &b[..n]);
    let mut lanes = [0i32; 8];
    let mut i = 0;
    while i + 8 <= n {
        for l in 0..8 {
            lanes[l] += a[i + l] as i32 * b[i + l] as i32;
        }
        i += 8;
    }
    let mut d: i32 = lanes.iter().sum();
    while i < n {
        d += a[i] as i32 * b[i] as i32;
        i += 1;
    }
    1.0 - d as f32 / (127.0 * 127.0)
}

/// Exact top-k over an i8-quantized embedding matrix (`n × EMBED_DIM` bytes,
/// mmap'd `emb.bin` in index format v2). Returns (chunk_id, distance) ascending.
pub fn brute_force_top_k_i8(query: &[i8], matrix: &[i8], k: usize) -> Vec<(u32, f32)> {
    let n = matrix.len() / EMBED_DIM;
    if n == 0 || k == 0 {
        return Vec::new();
    }
    let chunk_rows = 32 * 1024;
    let mut best: Vec<(u32, f32)> = (0..n.div_ceil(chunk_rows))
        .into_par_iter()
        .map(|block| {
            let start = block * chunk_rows;
            let end = (start + chunk_rows).min(n);
            let mut local = TopK::new(k);
            for row in start..end {
                let v = &matrix[row * EMBED_DIM..(row + 1) * EMBED_DIM];
                local.push(row as u32, dot_distance_i8(query, v));
            }
            local.into_sorted()
        })
        .reduce(Vec::new, |mut a, b| {
            a.extend(b);
            a
        });
    best.sort_unstable_by(|x, y| x.1.total_cmp(&y.1).then(x.0.cmp(&y.0)));
    best.truncate(k);
    best
}

/// Exact top-k over a flat embedding matrix (`n × EMBED_DIM` f32 values,
/// e.g. an mmap'd `emb.bin`). Returns (chunk_id, distance) ascending.
/// `normalized` selects the fast dot kernel (index format v2 stores unit
/// vectors); otherwise falls back to full cosine.
pub fn brute_force_top_k(
    query: &[f32],
    matrix: &[f32],
    k: usize,
    normalized: bool,
) -> Vec<(u32, f32)> {
    let n = matrix.len() / EMBED_DIM;
    if n == 0 || k == 0 {
        return Vec::new();
    }
    let chunk_rows = 16 * 1024;
    let dist: fn(&[f32], &[f32]) -> f32 = if normalized { dot_distance } else { distance };
    let mut best: Vec<(u32, f32)> = (0..n.div_ceil(chunk_rows))
        .into_par_iter()
        .map(|block| {
            let start = block * chunk_rows;
            let end = (start + chunk_rows).min(n);
            let mut local = TopK::new(k);
            for row in start..end {
                let v = &matrix[row * EMBED_DIM..(row + 1) * EMBED_DIM];
                local.push(row as u32, dist(query, v));
            }
            local.into_sorted()
        })
        .reduce(Vec::new, |mut a, b| {
            a.extend(b);
            a
        });
    best.sort_unstable_by(|x, y| x.1.total_cmp(&y.1).then(x.0.cmp(&y.0)));
    best.truncate(k);
    best
}

/// Incremental top-k accumulator for streaming (unindexed) search: feed
/// (id, distance) pairs as chunks are embedded, keep only k best.
pub struct TopK {
    k: usize,
    // max-heap by distance so the worst kept item is on top
    heap: std::collections::BinaryHeap<HeapItem>,
}

struct HeapItem(f32, u32);
impl PartialEq for HeapItem {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0 && self.1 == other.1
    }
}
impl Eq for HeapItem {}
impl PartialOrd for HeapItem {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for HeapItem {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.total_cmp(&other.0).then(self.1.cmp(&other.1))
    }
}

impl TopK {
    pub fn new(k: usize) -> Self {
        Self { k, heap: std::collections::BinaryHeap::with_capacity(k + 1) }
    }

    #[inline]
    pub fn push(&mut self, id: u32, dist: f32) {
        if self.heap.len() < self.k {
            self.heap.push(HeapItem(dist, id));
        } else if let Some(worst) = self.heap.peek()
            && dist < worst.0
        {
            self.heap.pop();
            self.heap.push(HeapItem(dist, id));
        }
    }

    /// Current worst kept distance (if k items are held).
    pub fn threshold(&self) -> Option<f32> {
        (self.heap.len() >= self.k).then(|| self.heap.peek().unwrap().0)
    }

    /// (id, distance) ascending by distance.
    pub fn into_sorted(self) -> Vec<(u32, f32)> {
        let mut v: Vec<(u32, f32)> =
            self.heap.into_iter().map(|HeapItem(d, i)| (i, d)).collect();
        v.sort_unstable_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topk_keeps_k_best() {
        let mut t = TopK::new(3);
        for (i, d) in [(0, 0.9), (1, 0.1), (2, 0.5), (3, 0.05), (4, 0.7)] {
            t.push(i, d);
        }
        let got = t.into_sorted();
        assert_eq!(got.iter().map(|x| x.0).collect::<Vec<_>>(), vec![3, 1, 2]);
    }

    #[test]
    fn brute_force_matches_naive() {
        // 100 pseudo-random rows, fixed seed via LCG
        let mut state = 42u64;
        let mut next = || {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            ((state >> 33) as f32 / u32::MAX as f32) - 0.5
        };
        let n = 100;
        let matrix: Vec<f32> = (0..n * EMBED_DIM).map(|_| next()).collect();
        let query: Vec<f32> = (0..EMBED_DIM).map(|_| next()).collect();
        let got = brute_force_top_k(&query, &matrix, 5, false);
        let mut naive: Vec<(u32, f32)> = (0..n)
            .map(|i| (i as u32, distance(&query, &matrix[i * EMBED_DIM..(i + 1) * EMBED_DIM])))
            .collect();
        naive.sort_unstable_by(|a, b| a.1.total_cmp(&b.1));
        naive.truncate(5);
        assert_eq!(got, naive);
    }

    #[test]
    fn embeddings_rank_related_text_closer() {
        let dog = embed_query("a dog barked at the mailman");
        let puppy = embed_query("the puppy was barking loudly");
        let math = embed_query("integral calculus and differential equations");
        assert!(distance(&dog, &puppy) < distance(&dog, &math));
    }

    #[test]
    fn hnsw_roundtrip_and_search() {
        let mut h = new_hnsw();
        let a = embed_query("filesystem error handling");
        let b = embed_query("network socket timeout");
        let c = embed_query("puppy training tips");
        for v in [a, b, c] {
            h.insert(v);
        }
        let q = embed_query("disk write failure");
        let hits = h.search(&a);
        assert_eq!(hits[0].1, 0);
        let hits = h.search(&q);
        assert!(!hits.is_empty());

        let bytes = h.to_bytes();
        let h2 = SemgrepHnsw::from_bytes(Cosine, &bytes).unwrap();
        assert_eq!(h2.search(&a)[0].1, 0);
    }
}
