//! Maximal marginal relevance: trading a little rank fidelity for results that
//! land in different places instead of clustering in one hot region.

use super::vec::distance;

/// Greedy maximal-marginal-relevance ordering.
///
/// Relevance is the min-max-normalized fused score; similarity is embedding
/// cosine. Items without a vector are treated as dissimilar to everything, so a
/// missing embedding never suppresses a result. Returns indices, best first.
///
/// `files` and `saturation` add **identity** diversity beside MMR's content
/// diversity (RESEARCH.md §40). MMR spreads by embedding, which does not stop
/// the top-k being several chunks of one file: measured on real agent queries,
/// gorp returned **3.80 distinct files among its top-5 chunks**, so 1.2 slots
/// in every 5 repeated a file the reader had already been shown, and 67 of 191
/// queries wasted two or more. Two chunks of one file can sit far apart in
/// embedding space, and MMR has no reason to separate them — it never sees
/// which file anything came from.
///
/// The decay is applied *inside* the loop rather than folded into `scores`
/// beforehand, because "this is the second pick from that file" only becomes
/// true once the first pick has happened. `saturation = 1.0` is an exact
/// no-op, and a `files` slice of the wrong length disables it outright.
pub fn mmr_order(
    scores: &[f32],
    vecs: &[Option<Vec<f32>>],
    files: &[u32],
    k: usize,
    lambda: f32,
    saturation: f32,
) -> Vec<usize> {
    let (lo, hi) =
        scores.iter().fold((f32::MAX, f32::MIN), |(lo, hi), &s| (lo.min(s), hi.max(s)));
    let span = (hi - lo).max(f32::EPSILON);
    let rel: Vec<f32> = scores.iter().map(|s| (s - lo) / span).collect();

    let saturating = saturation < 1.0 && files.len() == scores.len();
    let mut picked: std::collections::HashMap<u32, u32> = std::collections::HashMap::new();
    // What a candidate's relevance is worth given what has already been taken
    // from its file: full for the first, `saturation` for the second,
    // `saturation²` for the third. A decay rather than a cap, so a file that
    // genuinely holds the two best answers can still supply both.
    let weight = |i: usize, picked: &std::collections::HashMap<u32, u32>| -> f32 {
        if !saturating {
            return 1.0;
        }
        match picked.get(&files[i]) {
            None | Some(0) => 1.0,
            Some(&n) => saturation.powi(n as i32),
        }
    };

    let mut selected: Vec<usize> = Vec::with_capacity(k);
    let mut remaining: Vec<usize> = (0..scores.len()).collect();
    while selected.len() < k && !remaining.is_empty() {
        let (pos, &best) = remaining
            .iter()
            .enumerate()
            .max_by(|&(_, &a), &(_, &b)| {
                let ma = lambda * rel[a] * weight(a, &picked)
                    - (1.0 - lambda) * max_sim(a, &selected, vecs);
                let mb = lambda * rel[b] * weight(b, &picked)
                    - (1.0 - lambda) * max_sim(b, &selected, vecs);
                ma.total_cmp(&mb).then(b.cmp(&a)) // tie → lower index (higher rank)
            })
            .unwrap();
        if saturating {
            *picked.entry(files[best]).or_insert(0) += 1;
        }
        selected.push(best);
        remaining.swap_remove(pos);
    }
    selected
}

fn max_sim(i: usize, selected: &[usize], vecs: &[Option<Vec<f32>>]) -> f32 {
    // `vecs` may be empty when this is used as a plain saturating greedy
    // selector (lambda = 1.0), so index defensively rather than panicking.
    let Some(vi) = vecs.get(i).and_then(|v| v.as_ref()) else { return 0.0 };
    selected
        .iter()
        .filter_map(|&s| vecs[s].as_ref())
        .map(|vs| 1.0 - distance(vi, vs))
        .fold(0.0f32, f32::max)
}
