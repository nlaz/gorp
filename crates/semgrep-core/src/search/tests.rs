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

#[test]
fn overlapping_spans_dedupe_keeps_higher_rank() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("x.rs"), "fn alpha() {}\n".repeat(60)).unwrap();
    let cands = vec![
        Candidate {
            id: 0,
            chunk: Chunk { file_id: 0, start_line: 1, end_line: 32 },
            path: "x.rs".into(),
            score: 1.0,
        },
        Candidate {
            id: 1,
            chunk: Chunk { file_id: 0, start_line: 25, end_line: 56 },
            path: "x.rs".into(),
            score: 0.9,
        },
    ];
    let opts = SearchOptions { k: 2, ..Default::default() };
    let mut trace = crate::trace::Trace::new(crate::trace::SCHEDULE_WARM);
    let hits = finalize(dir.path(), "alpha", cands, &opts, "", &mut trace, |_| None);
    assert_eq!(hits.len(), 1, "overlapping same-file spans should collapse");
    assert_eq!(hits[0].start_line, 1);
}
