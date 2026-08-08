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
            if let Some(mut hit) = materialize(root, &c.path, c.chunk, c.score, &query_tokens, opts) {
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

/// What one printed line costs beyond its own text: a line number and its
/// separators.
///
/// Charged because a budget that counts only content does not control output.
/// Measured over three corpora at a 600-character content budget, the kernel
/// spent 5,694 bytes a search and Wikipedia 1,826 — the *inverse* of the
/// problem the budget was added to fix, because 30-character C lines buy 20
/// lines of `path:line:` overhead where 180-character prose lines buy 2.
/// Roughly half of real output is this tax, and a content budget is blind to
/// it (RESEARCH.md §26.4).
///
/// A known under-count: when the caller searched a directory the CLI also
/// prefixes each line with its path, which the engine cannot size because it
/// does not know whether the CLI will print one. Under-charging under-fills,
/// which is the safe direction.
const LINE_OVERHEAD: u32 = 12;

/// Grow a window outward from `at` until the next line would exceed `budget`
/// characters, and return its inclusive bounds.
///
/// Line-aligned because half a line of code is not a thing worth showing, and
/// symmetric because §26.1 measured a forward bias and it loses coverage. The
/// matched line is always included even when it alone exceeds the budget: a
/// hit that returns nothing is worse than a hit that returns one long line,
/// and `max_columns` already bounds how long that line can print.
///
/// Costs each line by its length in the *file*, not by what will be printed.
/// The CLI clips long lines separately (`out::MAX_COLUMNS`), so a budget spent
/// on a 5,000-character minified line buys one line here and prints ~200
/// characters. That under-fills rather than over-fills, which is the safe
/// direction: the two caps compose to a bound, never to a surprise. Keeping
/// the print width out of the engine also keeps one display concern in one
/// place instead of two that can disagree.
fn grow_to_budget(lines: &[String], at: usize, budget: u32) -> (usize, usize) {
    let cost = |s: &String| s.chars().count() as u32 + LINE_OVERHEAD;
    let (mut lo, mut hi) = (at, at);
    let mut used = cost(&lines[at]);
    loop {
        let mut grew = false;
        // After first, then before, so an odd character of slack lands below
        // the match — the same asymmetry the line budget uses.
        if hi + 1 < lines.len() && used + cost(&lines[hi + 1]) <= budget {
            hi += 1;
            used += cost(&lines[hi]);
            grew = true;
        }
        if lo > 0 && used + cost(&lines[lo - 1]) <= budget {
            lo -= 1;
            used += cost(&lines[lo]);
            grew = true;
        }
        if !grew {
            break;
        }
    }
    (lo, hi)
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
    opts: &SearchOptions,
) -> Option<SearchHit> {
    let text = corpus::read_text(&corpus::resolve(root, rel_path))?;
    // Both display extras are collected in this loop rather than by re-reading
    // the file in the CLI: the chunk's text is already in hand exactly once
    // here, and a second reader would be a second chance to disagree about
    // which lines a chunk covers.
    // Every line of the chunk is collected even when only a window will be
    // shown: the window is centred on the best-matching line, and which line
    // that is only becomes known once the loop below has finished.
    let want_lines = opts.passage_lines > 1 || (opts.passage_lines == 0 && opts.passage_chars > 0);
    let mut lines: Option<Vec<String>> = want_lines.then(Vec::new);
    let mut defines: Option<Vec<String>> = opts.defines.then(Vec::new);
    let mut best: Option<(usize, u32, &str)> = None;
    for (i, line) in text.lines().enumerate() {
        let line_no = i as u32 + 1;
        if line_no < chunk.start_line {
            continue;
        }
        if line_no > chunk.end_line {
            break;
        }
        if let Some(v) = lines.as_mut() {
            v.push(line.to_string());
        }
        if let Some(v) = defines.as_mut() {
            v.extend(crate::text::declared_names(line));
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
    // Cut the collected chunk down to `passage_lines` centred on the match, and
    // report where the cut starts. `out.rs` numbers printed lines from
    // `lines_from`, not from `start_line` — without that the whole passage is
    // misnumbered, which is worse than showing nothing, since the line number
    // is what the caller navigates by.
    let (lines, lines_from) = match lines {
        None => (None, None),
        Some(all) => {
            let first = chunk.start_line;
            let at = (line - first) as usize;
            let (lo, hi) = if opts.passage_lines > 0 {
                // Legacy line budget, kept so §26's campaign arms reproduce
                // under their own flag. 8 before / 9 after at 18: measured,
                // not chosen — a stronger forward bias loses coverage (§26.1).
                let before = ((opts.passage_lines - 1) / 2) as usize;
                let lo = at.saturating_sub(before);
                (lo, (lo + opts.passage_lines as usize - 1).min(all.len() - 1))
            } else {
                grow_to_budget(&all, at, opts.passage_chars)
            };
            let cut: Vec<String> = all[lo..=hi].to_vec();
            (Some(cut), Some(first + lo as u32))
        }
    };
    Some(SearchHit {
        path: rel_path.to_string(),
        start_line: chunk.start_line,
        end_line: chunk.end_line,
        line,
        text: line_text.to_string(),
        score,
        lines,
        lines_from,
        // Dedupe late rather than while collecting: a name declared twice in
        // one window says nothing a header should repeat, and the order the
        // file declares them in is the order worth showing.
        defines: defines.map(|mut v| {
            let mut seen = HashSet::new();
            v.retain(|n| seen.insert(n.clone()));
            v
        }),
    })
}
