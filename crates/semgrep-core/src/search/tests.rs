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
        phrases: 1,
        fine: None,
        bm25_rank: None,
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
        phrases: 1,
        fine: None,
        bm25_rank: None,
    };
    let cands = vec![span(0, a, 1.0), span(1, b, 0.9)];
    let opts = SearchOptions { k: 2, dedupe_overlap: frac, ..Default::default() };
    let mut trace = crate::trace::Trace::new(crate::trace::SCHEDULE_WARM);
    finalize(dir.path(), &crate::search::Query::parse("alpha"), cands, &opts, "", &mut trace, |_| None).hits
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
        phrases: 1,
        fine: None,
        bm25_rank: None,
    }];
    let opts = SearchOptions { k: 1, passage_lines: 18, passage_override: true, ..Default::default() };
    let mut trace = crate::trace::Trace::new(crate::trace::SCHEDULE_WARM);
    let hits = finalize(dir.path(), &crate::search::Query::parse("alpha"), cands, &opts, "", &mut trace, |_| None).hits;

    let h = &hits[0];
    assert_eq!(h.line, 40, "the match is line 40");
    // With the fine rerank on (the default), the hit's span is the fine
    // window and the chunk bound moves to `chunk_start_line` (§28.2).
    assert!(h.start_line <= 40 && 40 <= h.end_line, "the fine span holds the match");
    assert_eq!(h.chunk_start_line, Some(1), "the chunk bound is still reported");
    // 8 before, the match, 9 after.
    assert_eq!(h.lines_from, Some(32), "the passage starts at 40-8, not at the chunk start");
    let body = h.lines.as_ref().expect("a passage was requested");
    assert_eq!(body.len(), 18);
    assert_eq!(body[0], "fn filler32() {}", "lines_from must index the real first line");
    assert_eq!(body[8], "fn alpha() {}", "the match sits 8 lines into the passage");
    assert_eq!(body[17], "fn filler49() {}");
}

/// A character budget spends itself on content, so a file of long lines gets
/// fewer of them and a file of short lines gets more — which is the point.
#[test]
fn a_character_budget_buys_content_not_lines() {
    let dir = tempfile::tempdir().unwrap();
    // Two files, same byte size per line-group: 40 short lines vs 4 long ones.
    std::fs::write(dir.path().join("short.rs"),
        (1..=40).map(|i| if i == 20 { "fn alpha() {}\n".to_string() }
                         else { format!("fn s{i}() {{}}\n") }).collect::<String>()).unwrap();
    std::fs::write(dir.path().join("long.rs"),
        (1..=40).map(|i| if i == 20 { format!("fn alpha() {{}} {}\n", "x".repeat(300)) }
                         else { format!("fn l{i}() {{}} {}\n", "y".repeat(300)) }).collect::<String>()).unwrap();

    let run = |file: &str| {
        let cands = vec![Candidate {
            id: 0,
            chunk: Chunk { file_id: 0, start_line: 1, end_line: 40 },
            path: file.into(),
            score: 1.0,
            phrases: 1,
            fine: None,
        bm25_rank: None,
        }];
        let opts = SearchOptions { k: 1, passage_lines: 0, passage_chars: 800, passage_override: true, ..Default::default() };
        let mut trace = crate::trace::Trace::new(crate::trace::SCHEDULE_WARM);
        let hits = finalize(dir.path(), &crate::search::Query::parse("alpha"), cands, &opts, "", &mut trace, |_| None).hits;
        let h = &hits[0];
        let body = h.lines.clone().expect("a passage");
        (body.len(), body.iter().map(|l| l.chars().count() + 12).sum::<usize>())
    };
    let (n_short, cost_short) = run("short.rs");
    let (n_long, cost_long) = run("long.rs");

    assert!(n_short > n_long * 4, "short lines should buy far more of them: {n_short} vs {n_long}");
    // Both land under the budget, which is the property a line budget lacks.
    assert!(cost_short <= 800, "short file over budget: {cost_short}");
    assert!(cost_long <= 800 || n_long == 1, "long file over budget: {cost_long}");
}

#[test]
fn one_passage_line_is_the_pre_25_behaviour() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("x.rs"), "fn a() {}\nfn alpha() {}\nfn b() {}\n").unwrap();
    let cands = vec![Candidate {
        id: 0,
        chunk: Chunk { file_id: 0, start_line: 1, end_line: 3 },
        path: "x.rs".into(),
        score: 1.0,
        phrases: 1,
        fine: None,
        bm25_rank: None,
    }];
    let opts = SearchOptions { k: 1, passage_lines: 1, passage_chars: 0, passage_override: true, ..Default::default() };
    let mut trace = crate::trace::Trace::new(crate::trace::SCHEDULE_WARM);
    let hits = finalize(dir.path(), &crate::search::Query::parse("alpha"), cands, &opts, "", &mut trace, |_| None).hits;
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

/// A chunk shorter than the fine window scores as itself: the window clamps
/// to the chunk and the whole chunk is the passage.
#[test]
fn a_short_chunk_is_its_own_fine_window() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("x.rs"), "fn alpha() {}\nfn beta() {}\n").unwrap();
    let cands = vec![Candidate {
        id: 0,
        chunk: Chunk { file_id: 0, start_line: 1, end_line: 2 },
        path: "x.rs".into(),
        score: 1.0,
        phrases: 1,
        fine: None,
        bm25_rank: None,
    }];
    let opts = SearchOptions { k: 1, ..Default::default() };
    let mut trace = crate::trace::Trace::new(crate::trace::SCHEDULE_WARM);
    let hits = finalize(dir.path(), &crate::search::Query::parse("alpha"), cands, &opts, "", &mut trace, |_| None).hits;
    assert_eq!((hits[0].start_line, hits[0].end_line), (1, 2));
    assert_eq!(hits[0].chunk_start_line, Some(1), "fine ran even on a short chunk");
}

/// Two strided neighbours that elect the same lines from their shared region
/// collapse to one hit — the fine-window dedupe (§28.2). The chunk-level
/// dedupe cannot catch this at a 0.5 threshold (25% chunk overlap), but both
/// chunks' best window is the one distinctive line they share.
#[test]
fn neighbours_electing_the_same_window_collapse() {
    let dir = tempfile::tempdir().unwrap();
    // 56 filler lines; lines 27-29 are the only distinctive region, inside
    // the 25-32 overlap of chunks (1,32) and (25,56).
    let mut body: Vec<String> = (1..=56).map(|i| format!("let filler{i} = {i};")).collect();
    body[26] = "fn compute_backoff_delay(attempt: u32) {".into();
    body[27] = "    let exp = base_ms << attempt;".into();
    body[28] = "}".into();
    std::fs::write(dir.path().join("x.rs"), body.join("\n") + "\n").unwrap();
    let span = |id, (s, e), score| Candidate {
        id,
        chunk: Chunk { file_id: 0, start_line: s, end_line: e },
        path: "x.rs".into(),
        score,
        phrases: 1,
        fine: None,
        bm25_rank: None,
    };
    let cands = vec![span(0, (1, 32), 1.0), span(1, (25, 56), 0.9)];
    let opts = SearchOptions { k: 2, dedupe_overlap: 0.5, ..Default::default() };
    let mut trace = crate::trace::Trace::new(crate::trace::SCHEDULE_WARM);
    let hits =
        finalize(dir.path(), &crate::search::Query::parse("compute backoff delay"), cands, &opts, "", &mut trace, |_| None).hits;
    assert_eq!(hits.len(), 1, "the same elected window must not appear twice");
}

/// The fine rerank is a pure function of the query and the file bytes: two
/// runs over the same inputs agree exactly, including scores.
#[test]
fn the_fine_rerank_is_deterministic() {
    let dir = tempfile::tempdir().unwrap();
    let body: Vec<String> =
        (1..=40).map(|i| format!("fn handler_{i}(job: Job) -> Result<()> {{}}")).collect();
    std::fs::write(dir.path().join("x.rs"), body.join("\n") + "\n").unwrap();
    let run = || {
        let cands = vec![Candidate {
            id: 0,
            chunk: Chunk { file_id: 0, start_line: 1, end_line: 40 },
            path: "x.rs".into(),
            score: 1.0,
            phrases: 1,
            fine: None,
        bm25_rank: None,
        }];
        let opts = SearchOptions { k: 1, ..Default::default() };
        let mut trace = crate::trace::Trace::new(crate::trace::SCHEDULE_WARM);
        let hits = finalize(dir.path(), &crate::search::Query::parse("route a job to a handler"), cands, &opts, "", &mut trace, |_| None).hits;
        (hits[0].start_line, hits[0].end_line, hits[0].score)
    };
    assert_eq!(run(), run());
}

// ---------------------------------------------------------------------------
// §31: the phrase split rule
// ---------------------------------------------------------------------------

#[test]
fn phrases_split_on_bare_and_grep_escaped_pipes() {
    use crate::search::split_phrases;
    assert_eq!(split_phrases("retry backoff | session token"),
               vec!["retry backoff", "session token"]);
    // The grep spelling, verbatim from the s27 logs (§31): the escape is
    // separator syntax and must not leak into the left phrase.
    assert_eq!(split_phrases(r"def dup_add\|def dup_sub\|def dup_mul"),
               vec!["def dup_add", "def dup_sub", "def dup_mul"]);
}

#[test]
fn double_pipe_is_code_not_a_separator() {
    use crate::search::split_phrases;
    // Verbatim from the s31 logs: a pasted line whose || is JavaScript's OR.
    let pasted = "typeof vnode === 'string' || typeof vnode === 'number'";
    assert_eq!(split_phrases(pasted), vec![pasted]);
    // And || wins over an adjacent escape reading.
    assert_eq!(split_phrases(r"a\||b"), vec![r"a\||b"]);
}

#[test]
fn degenerate_pipe_queries_never_yield_nothing() {
    use crate::search::split_phrases;
    assert_eq!(split_phrases("|"), vec!["|"]);
    assert_eq!(split_phrases(" | | "), vec![" | | "]);
    assert_eq!(split_phrases("a |  | b"), vec!["a", "b"]);
    // A no-pipe query is byte-preserved, trailing backslash and all.
    assert_eq!(split_phrases(r"foo\"), vec![r"foo\"]);
}

#[test]
fn phrase_count_is_capped_at_the_bitmask_bound() {
    use crate::search::{MAX_PHRASES, split_phrases};
    let q = (0..12).map(|i| format!("p{i}")).collect::<Vec<_>>().join(" | ");
    assert_eq!(split_phrases(&q).len(), MAX_PHRASES);
}

#[test]
fn merge_interleave_unions_retrievers_and_normalizes_per_phrase() {
    use crate::search::merge_interleave;
    let c = |id: u32, score: f32| Candidate {
        id,
        chunk: Chunk { file_id: 0, start_line: 1, end_line: 4 },
        path: format!("f{id}.rs"),
        score,
        phrases: 1,
        fine: None,
        bm25_rank: None,
    };
    // Phrase 0 ranks [1, 2]; phrase 1 ranks [2, 3] on a wildly different
    // score scale — chunk 2 is retrieved by both.
    let merged = merge_interleave(vec![
        vec![c(1, 9.0), c(2, 5.0)],
        vec![c(2, 0.019), c(3, 0.011)],
    ]);
    let ids: Vec<u32> = merged.iter().map(|m| m.id).collect();
    assert_eq!(ids, vec![1, 2, 3], "round-robin by rank, deduped");
    assert_eq!(merged[1].phrases, 0b11, "the shared chunk answers for both phrases");
    assert_eq!(merged[0].phrases, 0b01);
    assert_eq!(merged[2].phrases, 0b10);
    // Scores normalized within each phrase's list: both rank-1s become 1.0.
    assert!((merged[0].score - 1.0).abs() < 1e-6);
}

/// Fuzz the split rule with a deterministic LCG: whatever the input, the
/// invariants hold — never empty, never more than MAX_PHRASES, no empty
/// phrase, and a pipe-free query is returned byte-identical.
#[test]
fn split_phrases_never_panics_and_holds_its_invariants() {
    use crate::search::{MAX_PHRASES, split_phrases};
    let alphabet: Vec<char> =
        "ab |\\|_()'\"$.*^[]{}?!:;/#@-=+~%&<>,\u{0}\u{FF5C}é中".chars().collect();
    let mut state: u64 = 0x5EED_CAFE;
    let mut next = move || {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (state >> 33) as usize
    };
    for _ in 0..5000 {
        let len = next() % 40;
        let q: String = (0..len).map(|_| alphabet[next() % alphabet.len()]).collect();
        let phrases = split_phrases(&q);
        assert!(!phrases.is_empty(), "empty split for {q:?}");
        assert!(phrases.len() <= MAX_PHRASES, "over cap for {q:?}");
        if !q.contains('|') {
            // Identity outranks the other invariants: a pipe-free query must
            // reach the engine byte-for-byte, exactly as it did pre-§31 —
            // including the empty query, whose one "phrase" is empty.
            assert_eq!(phrases, vec![q.clone()], "pipe-free must be identity");
        } else {
            // A piped query yields non-empty phrases (a lone "|b" trims to
            // one clean phrase), or falls back to itself whole when nothing
            // survives the split.
            assert!(
                phrases.iter().all(|p| !p.is_empty()) || phrases == vec![q.clone()],
                "piped: {q:?} -> {phrases:?}"
            );
        }
    }
}

/// §32.4a's lexical-drown fix: with `bm25_pin` set, a chunk BM25 ranked at
/// its head gets a display slot even when the semantic ordering never would
/// have shown it — and with the pin off, behavior is untouched.
#[test]
fn bm25_pin_guarantees_a_lexical_slot() {
    let dir = tempfile::tempdir().unwrap();
    for (name, body) in [
        ("a.rs", "fn alpha() {}\n"),
        ("b.rs", "fn alpha_helper() {}\n"),
        ("c.rs", "fn unrelated() {}\n"),
    ] {
        std::fs::write(dir.path().join(name), body.repeat(10)).unwrap();
    }
    let mk = |id, path: &str, score, bm25| Candidate {
        id,
        chunk: Chunk { file_id: 0, start_line: 1, end_line: 8 },
        path: path.into(),
        score,
        phrases: 1,
        fine: None,
        bm25_rank: bm25,
    };
    // c.rs is BM25's #1 but sits at the tail of the semantic order.
    let cands = || vec![
        mk(0, "a.rs", 1.0, None),
        mk(1, "b.rs", 0.9, None),
        mk(2, "c.rs", 0.1, Some(1)),
    ];
    let mut trace = crate::trace::Trace::new(crate::trace::SCHEDULE_WARM);
    let q = crate::search::Query::parse("alpha");

    let off = SearchOptions { k: 2, ..Default::default() };
    let hits = finalize(dir.path(), &q, cands(), &off, "", &mut trace, |_| None).hits;
    assert!(hits.iter().all(|h| h.path != "c.rs"), "unpinned tail must not display");

    let on = SearchOptions { k: 2, bm25_pin: 1, ..Default::default() };
    let hits = finalize(dir.path(), &q, cands(), &on, "", &mut trace, |_| None).hits;
    assert_eq!(hits.len(), 2, "pin replaces, never grows the display");
    assert!(hits.iter().any(|h| h.path == "c.rs"), "bm25 #1 must hold a slot");
    assert_eq!(hits.last().unwrap().path, "c.rs", "the pin fills from the tail");
}

/// §32.4a's rank-1 kill: the fine rerank may outrank the coarse winner but,
/// with the guard on, may not evict it from the display.
#[test]
fn coarse_top_survives_the_fine_rerank() {
    let dir = tempfile::tempdir().unwrap();
    // The coarse winner never mentions the query; the two runners-up do, so
    // the fine windows outscore it and — at k=2 — push it off the display.
    std::fs::write(dir.path().join("top.rs"), "fn zebra_quux() {}\n".repeat(10)).unwrap();
    std::fs::write(dir.path().join("m1.rs"), "fn alpha() {}\n".repeat(10)).unwrap();
    std::fs::write(dir.path().join("m2.rs"), "fn alpha_beta() {}\n".repeat(10)).unwrap();
    let mk = |id, path: &str, score| Candidate {
        id,
        chunk: Chunk { file_id: 0, start_line: 1, end_line: 8 },
        path: path.into(),
        score,
        phrases: 1,
        fine: None,
        bm25_rank: None,
    };
    let cands = || vec![mk(0, "top.rs", 1.0), mk(1, "m1.rs", 0.9), mk(2, "m2.rs", 0.8)];
    let mut trace = crate::trace::Trace::new(crate::trace::SCHEDULE_WARM);
    let q = crate::search::Query::parse("alpha");

    let off = SearchOptions { k: 2, ..Default::default() };
    let base = finalize(dir.path(), &q, cands(), &off, "", &mut trace, |_| None).hits;
    assert!(
        base.iter().all(|h| h.path != "top.rs"),
        "premise: the fine rerank demotes the coarse winner off a k=2 display \
         (got {:?})",
        base.iter().map(|h| h.path.clone()).collect::<Vec<_>>(),
    );

    let on = SearchOptions { k: 2, keep_coarse_top: true, ..Default::default() };
    let hits = finalize(dir.path(), &q, cands(), &on, "", &mut trace, |_| None).hits;
    assert_eq!(hits.len(), 2);
    assert!(hits.iter().any(|h| h.path == "top.rs"), "the coarse winner keeps a slot");
}
