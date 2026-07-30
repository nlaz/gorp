//! Reciprocal-rank fusion, and the modes that select which engines run.

use std::collections::HashMap;

/// Which engines answer a query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Keyword,
    Bm25,
    Semantic,
    Hybrid,
}

const RRF_K: f32 = 60.0;

/// For single-engine modes, pass scores through (semantic distance is turned
/// into a similarity so higher is always better). For hybrid, weighted RRF:
/// BM25 contributes at weight 1.0, semantic at `sem_weight` — evals showed
/// the lexical list is the stronger engine and equal weighting diluted it.
pub fn fuse(
    mode: Mode,
    bm25: Vec<(u32, f32)>,
    sem: Vec<(u32, f32)>,
    k: usize,
    sem_weight: f32,
) -> Vec<(u32, f32)> {
    let mut out = match mode {
        Mode::Bm25 => bm25,
        Mode::Semantic => sem.into_iter().map(|(id, d)| (id, 1.0 - d)).collect(),
        Mode::Hybrid => {
            let mut rrf: HashMap<u32, f32> = HashMap::new();
            for (rank, (id, _)) in bm25.iter().enumerate() {
                *rrf.entry(*id).or_insert(0.0) += 1.0 / (RRF_K + rank as f32 + 1.0);
            }
            for (rank, (id, _)) in sem.iter().enumerate() {
                *rrf.entry(*id).or_insert(0.0) += sem_weight / (RRF_K + rank as f32 + 1.0);
            }
            let mut v: Vec<(u32, f32)> = rrf.into_iter().collect();
            v.sort_unstable_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
            v
        }
        Mode::Keyword => unreachable!("keyword handled earlier"),
    };
    out.truncate(k);
    out
}
