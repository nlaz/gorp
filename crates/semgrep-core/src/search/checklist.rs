//! The learned checklist (RESEARCH.md §35.2): a logistic model over
//! candidate-local features that rewrites the relevance vector MMR consumes.
//!
//! The feature definitions here MIRROR `eval/locbench/checklist_train.py` —
//! the two lists must not drift, and both say so. Everything is a pure
//! function of the `Candidate` rows in hand: no I/O, no index state, which is
//! what makes the checklist cold==warm by construction (`finalize` is the
//! shared tail of both paths).
//!
//! Query-global features are deliberately absent: under a linear model a
//! per-query constant cannot change within-query order, and within-query
//! order is the only thing this decides.

use super::hit::Candidate;

/// Feature order, shared with the trainer:
/// fine_n, fine_missing, coarse_n, bm25_recip, bm25_missing, phrases_pop,
/// decl_share, path_share, chunk_frac.
pub(crate) const N_FEATURES: usize = 9;

/// Placeholder until the §35.2 training run lands its const block: all-zero
/// weights make the learned score a constant 0.5, and blending a constant is
/// order-neutral — the plumbing ships inert, exactly like `--learned-blend`'s
/// 0.0 default.
pub(crate) const WEIGHTS: [f32; N_FEATURES] = [0.0; N_FEATURES];
pub(crate) const BIAS: f32 = 0.0;

fn minmax(vals: &[f32]) -> Vec<f32> {
    let lo = vals.iter().copied().fold(f32::INFINITY, f32::min);
    let hi = vals.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    if !(hi > lo) {
        return vec![0.5; vals.len()];
    }
    vals.iter().map(|v| (v - lo) / (hi - lo)).collect()
}

/// One feature row per candidate, in candidate order.
pub(crate) fn features_of(kept: &[Candidate]) -> Vec<[f32; N_FEATURES]> {
    let fines: Vec<f32> = kept.iter().map(|c| c.fine.map_or(0.0, |f| f.score)).collect();
    let coarses: Vec<f32> = kept.iter().map(|c| c.score).collect();
    let fine_n = minmax(&fines);
    let coarse_n = minmax(&coarses);
    kept.iter()
        .enumerate()
        .map(|(i, c)| {
            let br = c.bm25_rank;
            [
                if c.fine.is_some() { fine_n[i] } else { 0.0 },
                if c.fine.is_some() { 0.0 } else { 1.0 },
                coarse_n[i],
                br.map_or(0.0, |r| 1.0 / r as f32),
                if br.is_some() { 0.0 } else { 1.0 },
                c.phrases.count_ones() as f32,
                c.decl_share,
                c.path_share,
                (c.chunk.end_line.saturating_sub(c.chunk.start_line) + 1) as f32 / 32.0,
            ]
        })
        .collect()
}

/// Blend the learned score into `relevance` and reorder `kept` in lockstep.
///
/// The base is min-max normalized first so the blend weight means the same
/// thing whatever score space the relevance arrived in (fine-blended, raw
/// RRF, raw BM25); the learned side is a sigmoid, so both operands live in
/// [0, 1]. The reorder matters even though MMR renormalizes: MMR is skipped
/// outright on short pools, where `kept`'s own order is the final order.
pub(crate) fn blend(kept: &mut [Candidate], relevance: &mut [f32], blend: f32) {
    debug_assert_eq!(kept.len(), relevance.len());
    if kept.is_empty() {
        return;
    }
    let base = minmax(relevance);
    let learned: Vec<f32> = features_of(kept)
        .iter()
        .map(|x| {
            let z: f32 = x.iter().zip(WEIGHTS).map(|(xi, wi)| xi * wi).sum::<f32>() + BIAS;
            1.0 / (1.0 + (-z).exp())
        })
        .collect();
    for (i, r) in relevance.iter_mut().enumerate() {
        *r = (1.0 - blend) * base[i] + blend * learned[i];
    }
    let mut order: Vec<usize> = (0..kept.len()).collect();
    order.sort_by(|&a, &b| relevance[b].total_cmp(&relevance[a]));
    let kept_new: Vec<Candidate> = order.iter().map(|&i| kept[i].clone()).collect();
    let rel_new: Vec<f32> = order.iter().map(|&i| relevance[i]).collect();
    kept.clone_from_slice(&kept_new);
    relevance.copy_from_slice(&rel_new);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Chunk;

    fn cand(id: u32, fine: Option<f32>, bm25: Option<u16>) -> Candidate {
        Candidate {
            id,
            chunk: Chunk { file_id: 0, start_line: 1, end_line: 32 },
            path: format!("f{id}.rs"),
            score: id as f32,
            phrases: 1,
            fine: fine.map(|s| super::super::hit::Fine {
                start_line: 1,
                end_line: 4,
                score: s,
                phrase: 0,
            }),
            bm25_rank: bm25,
            decl_share: 0.0,
            path_share: 0.0,
        }
    }

    #[test]
    fn features_normalize_within_the_pool_and_flag_missing_channels() {
        let kept = vec![cand(0, Some(0.9), Some(1)), cand(1, Some(0.5), None), cand(2, None, None)];
        let f = features_of(&kept);
        assert_eq!(f[0][0], 1.0, "best fine normalizes to 1");
        assert_eq!(f[2][0], 0.0, "missing fine scores 0");
        assert_eq!(f[2][1], 1.0, "and raises the missing flag");
        assert_eq!(f[0][3], 1.0, "bm25 rank 1 -> reciprocal 1");
        assert_eq!(f[1][4], 1.0, "no bm25 rank -> missing flag");
        assert_eq!(f[0][8], 1.0, "a 32-line chunk is one window unit");
    }

    #[test]
    fn zero_weights_blend_is_order_neutral_and_drops_nothing() {
        let mut kept = vec![cand(0, Some(0.9), None), cand(1, Some(0.5), None)];
        let mut rel = vec![0.9f32, 0.5];
        blend(&mut kept, &mut rel, 1.0);
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[0].id, 0, "constant learned score preserves order");
    }
}
