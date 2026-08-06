//! Candidates into displayable hits: span dedupe, MMR diversification, and
//! re-reading the best line out of the file.

use super::{SearchHit, SearchOptions};
use crate::rank::mmr;
use crate::text::token as tokenize;
use crate::trace::{Stage, Trace};
use crate::{Chunk, corpus};
use std::collections::HashSet;
use std::path::Path;

pub struct Candidate {
    /// Chunk id == row in the chunk table / embedding matrix.
    pub id: u32,
    pub chunk: Chunk,
    pub path: String,
    pub score: f32,
}

/// Shared tail of both search paths. `vec_of` supplies the embedding for a
/// candidate (by index into `cands`) when diversification needs it. `strip`
/// is the query's subtree prefix: candidate paths are index-root-relative
/// for file access, but hits display relative to the queried scope (the same
/// contract the streaming path has always had).
///
/// Timed in four parts rather than one, because they scale differently and a
/// single `finalize` number could not say which was hurting: dedupe is
/// quadratic in the candidate pool, `vec_of` is a dequantize per candidate warm
/// but a full *embedding* per candidate cold, MMR is O(k·n·dims), and
/// materialization is a file read per hit.
#[allow(clippy::too_many_arguments)]
pub fn finalize(
    root: &Path,
    query: &str,
    mut cands: Vec<Candidate>,
    opts: &SearchOptions,
    strip: &str,
    trace: &mut Trace,
    vec_of: impl Fn(&Candidate) -> Option<Vec<f32>>,
) -> Vec<SearchHit> {
    // Drop candidates that overlap an already-kept, higher-ranked candidate in
    // the same file by at least `opts.dedupe_overlap` of the shorter span —
    // near-duplicates, which is what this is for.
    //
    // It used to drop on *any* overlap, and that deletes answers. Chunks are
    // strided (window 32, overlap 8 by default), so every chunk overlaps its
    // neighbours by construction; within one file the rule therefore thins the
    // result set to a greedy non-overlapping subset. On the §24 `update_sources`
    // case that meant `ranked top 16 of 37 candidates`, with the chunk holding
    // the declaration dropped because two higher-scoring neighbours each
    // contained a *call site* of it — the answer removed before ranking rather
    // than ranked low. At 25% neighbour overlap a 0.5 threshold keeps neighbours
    // and still collapses true duplicates.
    let kept: Vec<Candidate> = trace.time(Stage::FinalizeDedupe, || {
        let mut kept: Vec<Candidate> = Vec::with_capacity(cands.len());
        for c in cands.drain(..) {
            let dup = kept.iter().any(|k| k.path == c.path && overlaps(k, &c, opts.dedupe_overlap));
            if !dup {
                kept.push(c);
            }
        }
        kept
    });

    // MMR: greedily pick relevant-but-dissimilar candidates so the top-k
    // surfaces different parts of the corpus instead of one hot region.
    let order: Vec<usize> = if opts.diversify && kept.len() > opts.k && opts.k > 1 {
        let vecs: Vec<Option<Vec<f32>>> =
            trace.time(Stage::FinalizeVectors, || kept.iter().map(&vec_of).collect());
        let scores: Vec<f32> = kept.iter().map(|c| c.score).collect();
        trace.time(Stage::FinalizeMmr, || {
            mmr::mmr_order(&scores, &vecs, opts.k, opts.mmr_lambda)
        })
    } else {
        (0..kept.len().min(opts.k)).collect()
    };

    trace.time(Stage::FinalizeMaterialize, || {
        let query_tokens: HashSet<String> = tokenize::tokens(query).into_iter().collect();
        let mut hits = Vec::with_capacity(opts.k);
        for i in order {
            let c = &kept[i];
            if let Some(mut hit) = materialize(root, &c.path, c.chunk, c.score, &query_tokens) {
                if !strip.is_empty()
                    && let Some(rest) = hit.path.strip_prefix(&format!("{strip}/"))
                {
                    hit.path = rest.to_string();
                }
                hits.push(hit);
            }
            if hits.len() == opts.k {
                break;
            }
        }
        hits
    })
}

/// Do two same-file chunks overlap by at least `frac` of the shorter span?
///
/// `frac <= 0` reproduces the original rule exactly — any shared line at all is
/// a duplicate — so the pre-§24 behaviour stays reachable as a control arm.
fn overlaps(a: &Candidate, b: &Candidate, frac: f32) -> bool {
    let lo = a.chunk.start_line.max(b.chunk.start_line);
    let hi = a.chunk.end_line.min(b.chunk.end_line);
    if lo > hi {
        return false;
    }
    let shared = (hi - lo + 1) as f32;
    if frac <= 0.0 {
        return true;
    }
    let span = |c: &Candidate| (c.chunk.end_line - c.chunk.start_line + 1) as f32;
    shared >= frac * span(a).min(span(b))
}

/// Turn a ranked chunk into a displayable hit: re-read the file and pick the
/// line with the highest query-token overlap (first non-empty line as
/// fallback). Skips chunks whose file vanished since ranking.
fn materialize(
    root: &Path,
    rel_path: &str,
    chunk: Chunk,
    score: f32,
    query_tokens: &HashSet<String>,
) -> Option<SearchHit> {
    let text = corpus::read_text(&corpus::resolve(root, rel_path))?;
    let mut best: Option<(usize, u32, &str)> = None;
    for (i, line) in text.lines().enumerate() {
        let line_no = i as u32 + 1;
        if line_no < chunk.start_line {
            continue;
        }
        if line_no > chunk.end_line {
            break;
        }
        if line.trim().is_empty() {
            continue;
        }
        let mut overlap = 0usize;
        tokenize::for_each_token(line, |tok| {
            if query_tokens.contains(tok) {
                overlap += 1;
            }
        });
        match &best {
            Some((b, _, _)) if *b >= overlap => {}
            _ => best = Some((overlap, line_no, line)),
        }
    }
    let (_, line, line_text) = best?;
    Some(SearchHit {
        path: rel_path.to_string(),
        start_line: chunk.start_line,
        end_line: chunk.end_line,
        line,
        text: line_text.to_string(),
        score,
    })
}
