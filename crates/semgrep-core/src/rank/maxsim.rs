//! MaxSim late interaction (RESEARCH.md §9.2).

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
