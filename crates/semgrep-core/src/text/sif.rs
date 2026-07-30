//! SIF-weighted pooling (RESEARCH.md §9.1).
//!
//! `ese` pools a string by uniform mean, which lets boilerplate tokens drown
//! the identifiers that actually discriminate. These re-pool by rarity weight,
//! a/(a + p(w)), using corpus statistics stored alongside the index — so a
//! query must be pooled with the same statistics as the chunks, or the two are
//! not in the same space.

use crate::EMBED_DIM;
use crate::rank::normalize;

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

    /// Fold another shard's *counts* in. Deliberately not a full merge: `a` and
    /// `mean` are configuration, not observations, and silently taking them from
    /// whichever shard merged last is how a build ends up weighting chunks with
    /// one constant and queries with another.
    pub fn merge_counts(&mut self, other: SifStats) {
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
