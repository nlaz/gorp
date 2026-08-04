//! MaxSim late interaction (RESEARCH.md §9.2), and the pre-fusion rerank built
//! on it (§9.4).
//!
//! Scoring what the caller has already read: computing token vectors for a
//! candidate needs the chunk's text off disk, which is `search`'s job. This
//! layer takes the resulting similarities and decides the order.

/// Head size when `maxsim_pool` is left at 0.
///
/// 96. Chosen for recall, and the same answer three independent measurements
/// have now given:
///
/// - §9.6's original sweep: semantic +0.03..0.06 R@5 over head 24.
/// - Head 32 vs 96 over three corpora: 96 worse or level in 5 of 6 cells,
///   significantly on etcd paraphrase (0.025 vs 0.060, CI [-0.065,-0.010]).
/// - Head 42 vs 96 pooled over four corpora: 96 wins paraphrase
///   (-0.0160, CI [-0.0306,-0.0015], p=0.052); direct inconclusive.
///
/// The effect concentrates in the *paraphrase* stratum, which is the shape you
/// would predict: a deep head matters most where the semantic list is weakest
/// and needs the most reordering. On identifier-style queries the right answer
/// is usually already near the top and a shallow head finds it.
///
/// Cost, etcd semantic warm: 12.5 ms at 96 against 8.7 ms at 32 and 8.3 ms at
/// 42 — about 4 ms. On the kernel the heads are within noise of each other
/// (67.6 vs 65.6 ms), because the scan dominates. Head 42 was measured and
/// beat neither: no better than 32 on recall, no cheaper in practice.
///
/// Note `head_size` takes `max(k * 3, AUTO_HEAD)`, so a caller asking for
/// `-k 50` reranks 150 rows rather than 96. The floor is what this constant
/// sets; large-k callers pay proportionally more.
const AUTO_HEAD: usize = 96;

/// How many of a ranked list's rows to rerank.
pub fn head_size(ranked_len: usize, k: usize, configured: usize) -> usize {
    let want = if configured > 0 { configured } else { (k * 3).max(AUTO_HEAD) };
    ranked_len.min(want)
}

/// Reorder a reranked head, blending MaxSim similarity with the order the
/// embeddings already produced.
///
/// Takes `(id, similarity, distance)` and returns `(id, pseudo-distance)`, so
/// the list keeps the ascending-is-better contract the rest of the pipeline
/// expects. Both signals are min-max normalized within the head before blending,
/// because they are not otherwise on comparable scales. `alpha` of 1.0 is pure
/// MaxSim, 0.0 keeps the embedding order.
///
/// This runs *before* RRF, not after. Post-fusion reranking let MaxSim override
/// BM25's exact-match signal instead of being fused with it, which measurably
/// hurt hybrid on code (§9.4).
pub fn blend_head(scored: &[(u32, f32, f32)], alpha: f32) -> Vec<(u32, f32)> {
    let alpha = alpha.clamp(0.0, 1.0);
    // A row whose text could not be read has no similarity to blend. Those are
    // held out of the normalization entirely and appended after it: they cannot
    // be scored, and they will be dropped at materialization anyway, so they
    // must not displace a row that *was* scored.
    let (scorable, unscorable): (Vec<_>, Vec<_>) =
        scored.iter().partition(|&&(_, sim, _)| sim.is_finite());

    let sim = normalize(scorable.iter().map(|&&(_, s, _)| s));
    // Negated: a smaller distance is a better score, and both inputs to the
    // blend have to point the same way.
    let emb = normalize(scorable.iter().map(|&&(_, _, d)| -d));

    let mut head: Vec<(u32, f32)> = scorable
        .iter()
        .zip(sim.iter().zip(emb.iter()))
        .map(|(&&(id, _, _), (&s, &e))| (id, -(alpha * s + (1.0 - alpha) * e)))
        .collect();
    head.sort_unstable_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
    // Blended scores fall in [-1, 0], so any positive value sorts strictly
    // behind every scored row.
    head.extend(unscorable.iter().map(|&&(id, _, _)| (id, 1.0)));
    head
}

/// Min-max to [0, 1]. A head where every value is equal maps to all-zero, which
/// leaves the blend to the other signal rather than dividing by zero.
///
/// Non-finite inputs are excluded from the range and then floored to 0. A
/// candidate whose file went unreadable arrives as `NEG_INFINITY`, and including
/// it made `lo` infinite, `span` infinite, and *every* normalized value
/// `(x + inf)/inf` — that is, NaN. One missing file was enough to turn the whole
/// reranked head into NaN scores and scramble its order.
fn normalize(xs: impl Iterator<Item = f32>) -> Vec<f32> {
    let xs: Vec<f32> = xs.collect();
    let (lo, hi) = xs
        .iter()
        .filter(|x| x.is_finite())
        .fold((f32::MAX, f32::MIN), |(l, h), &x| (l.min(x), h.max(x)));
    if lo > hi {
        // Nothing finite to scale against.
        return vec![0.0; xs.len()];
    }
    let span = (hi - lo).max(f32::EPSILON);
    xs.into_iter().map(|x| if x.is_finite() { (x - lo) / span } else { 0.0 }).collect()
}

/// MaxSim late-interaction score: each query token finds its best-matching
/// chunk token; the (weighted) sum is the score. Higher is better. One strong
/// identifier match can't be averaged away by boilerplate — the failure mode
/// of pooled single-vector scoring.
/// Dot product of two unit vectors, summed in 8 independent lanes.
///
/// The lanes are the point. Written as `zip(..).map(|(a, b)| a * b).sum()` this
/// is a strictly-ordered float reduction, and float addition is not associative,
/// so LLVM is not permitted to reassociate it into vector adds — it emits scalar
/// code no matter how the bounds checks are arranged. That is a different
/// blocker from the one in [`crate::rank::dot_distance_i8`], where integer
/// arithmetic vectorizes as soon as the indexing lets it, and it shipped the
/// same way: the release binary contained no `fmla` at all. Splitting the
/// accumulator chooses a summation order up front, which LLVM *can* vectorize.
/// Measured over the head this reranks, 4.4× (1.34 → 5.91 G MAC/s).
///
/// Unlike the i8 scan this is not bit-identical — the reordered sum differs in
/// the last ulp, which can reorder otherwise-tied candidates.
#[inline]
fn dot(q: &[f32], d: &[f32]) -> f32 {
    let mut lanes = [0f32; 8];
    let mut cq = q.chunks_exact(8);
    let mut cd = d.chunks_exact(8);
    for (x, y) in cq.by_ref().zip(cd.by_ref()) {
        for l in 0..8 {
            lanes[l] += x[l] * y[l];
        }
    }
    let mut s: f32 = lanes.iter().sum();
    for (x, y) in cq.remainder().iter().zip(cd.remainder()) {
        s += x * y;
    }
    s
}

pub fn maxsim(query_toks: &[(f32, Vec<f32>)], doc_toks: &[(f32, Vec<f32>)]) -> f32 {
    let mut score = 0.0f32;
    for (w, q) in query_toks {
        let mut best = f32::NEG_INFINITY;
        for (_, d) in doc_toks {
            // Unit vectors: dot = cosine similarity.
            let sim = dot(q, d);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// alpha 1.0 is pure MaxSim: the head must come back ordered by similarity,
    /// regardless of the distances it arrived with.
    #[test]
    fn pure_maxsim_orders_by_similarity() {
        // (id, sim, dist) — id 2 has the best similarity but the worst distance.
        let scored = [(0u32, 0.1f32, 0.10f32), (1, 0.5, 0.20), (2, 0.9, 0.90)];
        let out = blend_head(&scored, 1.0);
        assert_eq!(out.iter().map(|&(id, _)| id).collect::<Vec<_>>(), [2, 1, 0]);
    }

    /// alpha 0.0 keeps the embedding order, so a rerank that is switched off by
    /// blend cannot silently reshuffle the list.
    #[test]
    fn zero_blend_keeps_embedding_order() {
        let scored = [(0u32, 0.1f32, 0.10f32), (1, 0.5, 0.20), (2, 0.9, 0.90)];
        let out = blend_head(&scored, 0.0);
        assert_eq!(out.iter().map(|&(id, _)| id).collect::<Vec<_>>(), [0, 1, 2]);
    }

    /// The output must stay a pseudo-*distance*: ascending is better, because
    /// the rest of the pipeline sorts it that way.
    #[test]
    fn output_ascends_with_worseness() {
        let scored = [(0u32, 0.9f32, 0.1f32), (1, 0.5, 0.5), (2, 0.1, 0.9)];
        let out = blend_head(&scored, 0.75);
        assert!(out.windows(2).all(|w| w[0].1 <= w[1].1), "not ascending: {out:?}");
    }

    /// Every input id comes back exactly once — a rerank may reorder, never drop.
    #[test]
    fn reranking_is_a_permutation() {
        let scored: Vec<(u32, f32, f32)> =
            (0..20).map(|i| (i, (i as f32 * 7.0) % 5.0, (i as f32 * 3.0) % 4.0)).collect();
        let mut ids: Vec<u32> =
            blend_head(&scored, 0.5).into_iter().map(|(id, _)| id).collect();
        ids.sort_unstable();
        assert_eq!(ids, (0..20).collect::<Vec<u32>>());
    }

    /// A head where every similarity is identical must not divide by zero, and
    /// must fall back to the other signal rather than producing NaNs.
    #[test]
    fn a_flat_signal_does_not_produce_nans() {
        let scored = [(0u32, 0.5f32, 0.1f32), (1, 0.5, 0.2), (2, 0.5, 0.3)];
        let out = blend_head(&scored, 0.5);
        assert!(out.iter().all(|&(_, s)| s.is_finite()), "got {out:?}");
        assert_eq!(out.iter().map(|&(id, _)| id).collect::<Vec<_>>(), [0, 1, 2]);
    }

    /// An unreadable candidate scores NEG_INFINITY. It must sink, not poison the
    /// normalization for everything else.
    #[test]
    fn an_unreadable_candidate_sinks_without_poisoning_the_head() {
        let scored = [(0u32, f32::NEG_INFINITY, 0.5f32), (1, 0.4, 0.2), (2, 0.8, 0.3)];
        let out = blend_head(&scored, 1.0);
        assert_eq!(out.last().unwrap().0, 0, "the unreadable row must sort last");
        assert!(out.iter().all(|&(_, s)| s.is_finite()), "got {out:?}");
    }

    #[test]
    fn head_size_respects_the_configured_override_and_the_list_length() {
        assert_eq!(head_size(500, 10, 0), 96, "auto head is 96 at k=10");
        assert_eq!(head_size(500, 50, 0), 150, "auto head grows with k*3");
        assert_eq!(head_size(500, 10, 24), 24, "an explicit pool wins");
        assert_eq!(head_size(12, 10, 96), 12, "never past the end of the list");
    }
}
