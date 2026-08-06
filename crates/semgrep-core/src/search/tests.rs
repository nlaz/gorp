//! Fusion and finalization tests: the parts of search that are pure functions
//! of their inputs.

use super::SearchOptions;
use super::hit::{Candidate, finalize};
use crate::Chunk;
use crate::rank::{Mode, fuse};

fn cand(id: u32, path: &str, start: u32, score: f32) -> Candidate {
    Candidate {
        id,
        chunk: Chunk { file_id: 0, start_line: start, end_line: start + 7 },
        path: path.to_string(),
        score,
    }
}

#[test]
fn weighted_rrf_favors_bm25() {
    // doc 1 tops bm25, doc 2 tops semantic; equal ranks otherwise.
    let bm25 = vec![(1, 9.0), (2, 5.0)];
    let sem = vec![(2, 0.1), (1, 0.4)];
    let fused = fuse(Mode::Hybrid, bm25, sem, 10, 0.5);
    assert_eq!(fused[0].0, 1, "bm25 winner should lead at sem_weight<1");
    let equal = fuse(Mode::Hybrid, vec![(1, 9.0), (2, 5.0)], vec![(2, 0.1), (1, 0.4)], 10, 1.0);
    // sanity: at weight 1.0 the two docs tie on RRF and order falls to id
    assert_eq!(equal.len(), 2);
}

#[test]
fn mmr_prefers_diverse_over_redundant() {
    // a and b are near-identical vectors with top scores; c is distinct
    // with a slightly lower score. With diversity on, c should beat b.
    let cands = [cand(0, "a.rs", 1, 1.0), cand(1, "b.rs", 1, 0.95), cand(2, "c.rs", 1, 0.80)];
    let mut va = vec![0.0f32; 8];
    va[0] = 1.0;
    let mut vb = va.clone();
    vb[1] = 0.05; // nearly parallel to va
    let mut vc = vec![0.0f32; 8];
    vc[3] = 1.0; // orthogonal
    let vecs = vec![Some(va), Some(vb), Some(vc)];
    let scores: Vec<f32> = cands.iter().map(|c| c.score).collect();
    let order = crate::rank::mmr_order(&scores, &vecs, 2, 0.5);
    assert_eq!(order, vec![0, 2]);
}

/// `finalize` over two same-file spans at a given dedupe threshold.
fn dedupe_case(a: (u32, u32), b: (u32, u32), frac: f32) -> Vec<crate::search::SearchHit> {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("x.rs"), "fn alpha() {}\n".repeat(80)).unwrap();
    let span = |id, (s, e), score| Candidate {
        id,
        chunk: Chunk { file_id: 0, start_line: s, end_line: e },
        path: "x.rs".into(),
        score,
    };
    let cands = vec![span(0, a, 1.0), span(1, b, 0.9)];
    let opts = SearchOptions { k: 2, dedupe_overlap: frac, ..Default::default() };
    let mut trace = crate::trace::Trace::new(crate::trace::SCHEDULE_WARM);
    finalize(dir.path(), "alpha", cands, &opts, "", &mut trace, |_| None)
}

#[test]
fn any_overlap_collapses_at_frac_zero() {
    // The pre-§24 rule, kept reachable as the campaign's control arm: two
    // chunks sharing 8 of 32 lines are one hit, higher rank wins.
    let hits = dedupe_case((1, 32), (25, 56), 0.0);
    assert_eq!(hits.len(), 1, "any shared line should collapse at frac 0");
    assert_eq!(hits[0].start_line, 1);
}

#[test]
fn neighbouring_chunks_survive_at_the_default_threshold() {
    // The §24 fix. Chunks are strided, so *every* chunk overlaps its
    // neighbours: at window 32 / overlap 8 they share 25%, well under the 50%
    // that makes them near-duplicates. Collapsing them thinned a single file's
    // results to a greedy non-overlapping subset and deleted the chunk holding
    // the answer whenever a neighbour holding a call site outscored it.
    let hits = dedupe_case((1, 32), (25, 56), 0.5);
    assert_eq!(hits.len(), 2, "8 of 32 shared lines is not a near-duplicate");
}

#[test]
fn containment_collapses_at_every_threshold() {
    // A chunk wholly inside another really is redundant, and stays so however
    // the threshold moves — otherwise the fix would trade one failure for the
    // duplicate-results failure the dedupe exists to prevent.
    for frac in [0.0, 0.5, 1.0] {
        let hits = dedupe_case((1, 32), (5, 20), frac);
        assert_eq!(hits.len(), 1, "contained span should collapse at frac {frac}");
        assert_eq!(hits[0].start_line, 1);
    }
}
