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
    /// Document frequency: in how many `count` calls (files) each token
    /// appeared. What idf weighting (§14.7) reads, as freqs is what SIF reads.
    #[serde(default)]
    pub df: std::collections::HashMap<String, u64>,
    #[serde(default)]
    pub n_docs: u64,
    /// Weight by BM25-style idf over `df` instead of a/(a+p) over `freqs`
    /// (--sif-idf, §14.7). Persisted so query pooling always matches chunks.
    #[serde(default)]
    pub idf: bool,
}

impl SifStats {
    /// SIF: a/(a + p(w)), unseen tokens get the maximum weight (rare =
    /// strong). idf mode: ln((n − df + ½)/(df + ½) + 1), the BM25 curve.
    #[inline]
    pub fn weight(&self, token: &str) -> f32 {
        if self.idf {
            let n = self.n_docs.max(1) as f64;
            let df = self.df.get(token).copied().unwrap_or(0) as f64;
            return ((n - df + 0.5) / (df + 0.5) + 1.0).ln() as f32;
        }
        let p =
            self.freqs.get(token).map(|&c| c as f64 / self.total.max(1) as f64).unwrap_or(0.0);
        (self.a / (self.a + p)) as f32
    }

    /// Count one document's tokens into the stats (build-time pass 1).
    /// A call is a document: collection counts land in `freqs`, presence
    /// lands in `df` once per token however often it repeats.
    pub fn count(&mut self, text: &str) {
        let mut local: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
        ese::for_each_token(text, |tok| match local.get_mut(tok) {
            Some(c) => *c += 1,
            None => {
                local.insert(tok.to_string(), 1);
            }
        });
        self.n_docs += 1;
        for (t, c) in local {
            self.total += c;
            *self.df.entry(t.clone()).or_insert(0) += 1;
            *self.freqs.entry(t).or_insert(0) += c;
        }
    }

    /// Fold another shard's *counts* in. Deliberately not a full merge: `a`,
    /// `mean` and `idf` are configuration, not observations, and silently
    /// taking them from whichever shard merged last is how a build ends up
    /// weighting chunks with one constant and queries with another.
    pub fn merge_counts(&mut self, other: SifStats) {
        for (t, c) in other.freqs {
            *self.freqs.entry(t).or_insert(0) += c;
        }
        for (t, c) in other.df {
            *self.df.entry(t).or_insert(0) += c;
        }
        self.total += other.total;
        self.n_docs += other.n_docs;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn stats() -> SifStats {
        let mut s = SifStats::default();
        // 4 documents; "common" in all of them, "rare" in one.
        for i in 0..4 {
            s.count(if i == 0 { "common rare common" } else { "common filler" });
        }
        s.a = SIF_A;
        s
    }

    #[test]
    fn both_weightings_rank_rare_above_common() {
        let mut s = stats();
        assert!(s.weight("rare") > s.weight("common"));
        s.idf = true;
        assert!(s.weight("rare") > s.weight("common"));
    }

    #[test]
    fn idf_weight_is_the_bm25_curve_over_document_frequency() {
        let mut s = stats();
        s.idf = true;
        // n=4, df(rare)=1: ln((4 − 1 + 0.5)/1.5 + 1). Repetition within a doc
        // must not move df — "common" appears twice in doc 0 but df is 4.
        assert!((s.weight("rare") - ((3.5f32 / 1.5) + 1.0).ln()).abs() < 1e-6);
        assert_eq!(s.df.get("common"), Some(&4));
        assert_eq!(s.freqs.get("common"), Some(&5));
    }

    #[test]
    fn merge_folds_df_and_docs_like_the_other_counts() {
        let mut a = stats();
        let b = stats();
        a.merge_counts(b);
        assert_eq!(a.n_docs, 8);
        assert_eq!(a.df.get("rare"), Some(&2));
    }
}
