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

/// A passage is a window cut around the matched line, so it usually starts
/// partway into the chunk — and `out.rs` numbers its lines from `lines_from`.
/// Numbering from `start_line` instead misnumbers every line of every result,
/// silently, and the line number is the thing a caller navigates by. The
/// snapshot cannot catch it (it would just record the wrong numbers) and
/// `cold_and_warm` cannot either (both paths would be wrong together).
#[test]
fn a_passage_reports_where_it_actually_starts() {
    let dir = tempfile::tempdir().unwrap();
    // 60 lines; only line 40 mentions the query, so the match is mid-chunk.
    let mut body: Vec<String> = (1..=60).map(|i| format!("fn filler{i}() {{}}")).collect();
    body[39] = "fn alpha() {}".into();
    std::fs::write(dir.path().join("x.rs"), body.join("\n") + "\n").unwrap();

    let cands = vec![Candidate {
        id: 0,
        chunk: Chunk { file_id: 0, start_line: 1, end_line: 60 },
        path: "x.rs".into(),
        score: 1.0,
    }];
    let opts = SearchOptions { k: 1, passage_lines: 18, ..Default::default() };
    let mut trace = crate::trace::Trace::new(crate::trace::SCHEDULE_WARM);
    let hits = finalize(dir.path(), "alpha", cands, &opts, "", &mut trace, |_| None);

    let h = &hits[0];
    assert_eq!(h.line, 40, "the match is line 40");
    assert_eq!(h.start_line, 1, "the chunk still starts at 1");
    // 8 before, the match, 9 after.
    assert_eq!(h.lines_from, Some(32), "the passage starts at 40-8, not at the chunk start");
    let body = h.lines.as_ref().expect("a passage was requested");
    assert_eq!(body.len(), 18);
    assert_eq!(body[0], "fn filler32() {}", "lines_from must index the real first line");
    assert_eq!(body[8], "fn alpha() {}", "the match sits 8 lines into the passage");
    assert_eq!(body[17], "fn filler49() {}");
}

#[test]
fn one_passage_line_is_the_pre_26_behaviour() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("x.rs"), "fn a() {}\nfn alpha() {}\nfn b() {}\n").unwrap();
    let cands = vec![Candidate {
        id: 0,
        chunk: Chunk { file_id: 0, start_line: 1, end_line: 3 },
        path: "x.rs".into(),
        score: 1.0,
    }];
    let opts = SearchOptions { k: 1, passage_lines: 1, ..Default::default() };
    let mut trace = crate::trace::Trace::new(crate::trace::SCHEDULE_WARM);
    let hits = finalize(dir.path(), "alpha", cands, &opts, "", &mut trace, |_| None);
    // No passage at all, so the CLI prints exactly the one line it always did.
    assert!(hits[0].lines.is_none(), "passage_lines=1 must carry no passage");
    assert_eq!(hits[0].line, 2);
}

#[test]
fn any_overlap_collapses_at_the_default() {
    // The shipped rule: two chunks sharing 8 of 32 lines are one hit, higher
    // rank wins. §24.2 measured the alternative and did not adopt it.
    let hits = dedupe_case((1, 32), (25, 56), 0.0);
    assert_eq!(hits.len(), 1, "any shared line should collapse at frac 0");
    assert_eq!(hits[0].start_line, 1);
}

#[test]
fn a_threshold_lets_neighbouring_chunks_survive() {
    // Chunks are strided, so *every* chunk overlaps its neighbours: at window
    // 32 / overlap 8 they share 25%, under a 50% threshold. This is the §24.1
    // arm — it does rescue the declaration chunk on the `update_sources` case,
    // and it still lost 0.009 overlap@5 across 2,188 real agent queries by
    // crowding the top-k with one file. Kept measurable, not default.
    let hits = dedupe_case((1, 32), (25, 56), 0.5);
    assert_eq!(hits.len(), 2, "8 of 32 shared lines is under a 0.5 threshold");
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
