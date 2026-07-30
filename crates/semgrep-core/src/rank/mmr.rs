//! Maximal marginal relevance: trading a little rank fidelity for results that
//! land in different places instead of clustering in one hot region.

use super::vec::distance;

/// Greedy maximal-marginal-relevance ordering.
///
/// Relevance is the min-max-normalized fused score; similarity is embedding
/// cosine. Items without a vector are treated as dissimilar to everything, so a
/// missing embedding never suppresses a result. Returns indices, best first.
pub fn mmr_order(
    scores: &[f32],
    vecs: &[Option<Vec<f32>>],
    k: usize,
    lambda: f32,
) -> Vec<usize> {
    let (lo, hi) =
        scores.iter().fold((f32::MAX, f32::MIN), |(lo, hi), &s| (lo.min(s), hi.max(s)));
    let span = (hi - lo).max(f32::EPSILON);
    let rel: Vec<f32> = scores.iter().map(|s| (s - lo) / span).collect();

    let mut selected: Vec<usize> = Vec::with_capacity(k);
    let mut remaining: Vec<usize> = (0..scores.len()).collect();
    while selected.len() < k && !remaining.is_empty() {
        let (pos, &best) = remaining
            .iter()
            .enumerate()
            .max_by(|&(_, &a), &(_, &b)| {
                let ma = lambda * rel[a] - (1.0 - lambda) * max_sim(a, &selected, vecs);
                let mb = lambda * rel[b] - (1.0 - lambda) * max_sim(b, &selected, vecs);
                ma.total_cmp(&mb).then(b.cmp(&a)) // tie → lower index (higher rank)
            })
            .unwrap();
        selected.push(best);
        remaining.swap_remove(pos);
    }
    selected
}

fn max_sim(i: usize, selected: &[usize], vecs: &[Option<Vec<f32>>]) -> f32 {
    let Some(vi) = &vecs[i] else { return 0.0 };
    selected
        .iter()
        .filter_map(|&s| vecs[s].as_ref())
        .map(|vs| 1.0 - distance(vi, vs))
        .fold(0.0f32, f32::max)
}
