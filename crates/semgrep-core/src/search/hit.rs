//! Candidates into displayable hits: span dedupe, MMR diversification, and
//! re-reading the best line out of the file.

use super::{SearchHit, SearchOptions};
use crate::rank::{self, mmr};
use crate::text::token as tokenize;
use crate::trace::{Stage, Trace};
use crate::{Chunk, corpus, text};
use std::collections::HashSet;
use std::path::Path;

#[derive(Clone)]
pub struct Candidate {
    /// Chunk id == row in the chunk table / embedding matrix.
    pub id: u32,
    pub chunk: Chunk,
    pub path: String,
    pub score: f32,
    /// The best few-line window inside this chunk, once the fine rerank has
    /// scored it. `None` until then, and stays `None` when the rerank is off
    /// or the file could not be re-read.
    pub fine: Option<Fine>,
}

/// The fine rerank's verdict on one candidate: the sub-window of its chunk
/// that matches the query best, and how well (cosine in the fine space,
/// [-1, 1], comparable across queries).
#[derive(Debug, Clone, Copy)]
pub struct Fine {
    /// 1-based, inclusive, blank edges already trimmed.
    pub start_line: u32,
    pub end_line: u32,
    pub score: f32,
}

/// What finalization produced, and whether the score floor refused it.
/// `best_signal` is populated whenever a floor was set — on success too, so a
/// calibration campaign can join score to outcome from the report alone.
pub struct Finalized {
    pub hits: Vec<SearchHit>,
    pub floored: bool,
    pub best_signal: Option<f32>,
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
) -> Finalized {
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
    let mut kept: Vec<Candidate> = trace.time(Stage::FinalizeDedupe, || {
        let mut kept: Vec<Candidate> = Vec::with_capacity(cands.len());
        for c in cands.drain(..) {
            let dup = kept.iter().any(|k| k.path == c.path && overlaps(k, &c, opts.dedupe_overlap));
            if !dup {
                kept.push(c);
            }
        }
        kept
    });

    // Fine rerank: pick the best few-line window inside each surviving chunk
    // and let the windows' scores order the list (RESEARCH.md §28.2). Between
    // dedupe and MMR because dedupe is a property of the chunks and MMR must
    // rank what will actually be shown.
    let relevance: Option<Vec<f32>> = trace.time(Stage::FinalizeFine, || {
        (opts.fine_rerank && opts.fine_lines >= 1)
            .then(|| fine_rerank(root, query, &mut kept, opts))
    });

    // The score floor (§28.2): set-level, before MMR and truncation, on the
    // best candidate's cross-query-comparable signal. Judged here — the tail
    // both paths share — so a cached scope refuses exactly what an uncached
    // one refuses.
    let best_signal = (opts.min_score > 0.0 && !kept.is_empty())
        .then(|| trace.time(Stage::FinalizeFine, || pool_signal(query, &kept, &relevance, &vec_of)))
        .flatten();
    if let Some(s) = best_signal
        && s < opts.min_score
    {
        return Finalized { hits: Vec::new(), floored: true, best_signal };
    }

    // MMR: greedily pick relevant-but-dissimilar candidates so the top-k
    // surfaces different parts of the corpus instead of one hot region.
    let order: Vec<usize> = if opts.diversify && kept.len() > opts.k && opts.k > 1 {
        let vecs: Vec<Option<Vec<f32>>> =
            trace.time(Stage::FinalizeVectors, || kept.iter().map(&vec_of).collect());
        let scores: Vec<f32> = match &relevance {
            Some(r) => r.clone(),
            None => kept.iter().map(|c| c.score).collect(),
        };
        trace.time(Stage::FinalizeMmr, || {
            mmr::mmr_order(&scores, &vecs, opts.k, opts.mmr_lambda)
        })
    } else {
        (0..kept.len().min(opts.k)).collect()
    };

    let hits = trace.time(Stage::FinalizeMaterialize, || {
        let query_tokens: HashSet<String> = tokenize::tokens(query).into_iter().collect();
        let mut hits = Vec::with_capacity(opts.k);
        for i in order {
            let c = &kept[i];
            if let Some(mut hit) = materialize(root, c, &query_tokens, opts) {
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
    });
    Finalized { hits, floored: false, best_signal }
}

/// The floor's signal for the pool: the best fine-window cosine when the fine
/// rerank ran, else the best chunk-embedding cosine through the same stored
/// quantization MMR diversifies with. `None` when nothing was scorable —
/// every file unreadable — in which case the floor abstains rather than
/// refusing on no evidence.
fn pool_signal(
    query: &str,
    kept: &[Candidate],
    relevance: &Option<Vec<f32>>,
    vec_of: &impl Fn(&Candidate) -> Option<Vec<f32>>,
) -> Option<f32> {
    let s = if relevance.is_some() {
        kept.iter()
            .filter_map(|c| c.fine.map(|f| f.score))
            .fold(f32::NEG_INFINITY, f32::max)
    } else {
        let mut q = text::embed_query(query);
        rank::normalize(&mut q);
        let q = rank::as_stored(&q);
        kept.iter()
            .filter_map(vec_of)
            .map(|v| 1.0 - rank::distance(&q, &v))
            .fold(f32::NEG_INFINITY, f32::max)
    };
    s.is_finite().then_some(s)
}

/// Score every `fine_lines`-tall window of each candidate's chunk against the
/// query, reorder the candidates by their best window, and return the
/// relevance scores MMR should rank by (index-aligned with `kept`).
///
/// The fine space is deliberately self-contained: query and windows are both
/// embedded from raw text — no path line, no SIF stats, no prose render — so
/// the score is a pure function of the query string and the file bytes.
/// That is what lets the cold and warm paths share this code with no index
/// state threaded in, which is the parity invariant's whole demand.
///
/// A candidate whose file cannot be re-read keeps `fine: None` and sinks to
/// the tail in its original relative order — the same "unscorable sinks, is
/// not dropped" rule the MaxSim head uses, because dropping here would
/// silently change the pool that dedupe already shaped.
fn fine_rerank(
    root: &Path,
    query: &str,
    kept: &mut Vec<Candidate>,
    opts: &SearchOptions,
) -> Vec<f32> {
    let q_i8 = {
        let mut q = text::embed_query(query);
        rank::normalize(&mut q);
        rank::quantize_i8(&q)
    };
    use rayon::prelude::*;
    let fines: Vec<Option<Fine>> = kept
        .par_iter()
        .map(|c| best_window(root, c, &q_i8, opts.fine_lines as usize))
        .collect();
    for (c, fine) in kept.iter_mut().zip(fines) {
        c.fine = fine;
    }

    // Order: scored candidates by blended score, unscored after them in their
    // surviving coarse order. Blending happens on min-max normalized values
    // because fine cosine and fused coarse scores live on incomparable scales.
    let scored: Vec<&Candidate> = kept.iter().filter(|c| c.fine.is_some()).collect();
    let (f_lo, f_hi) = min_max(scored.iter().map(|c| c.fine.expect("filtered").score));
    let (c_lo, c_hi) = min_max(scored.iter().map(|c| c.score));
    let norm = |v: f32, lo: f32, hi: f32| if hi > lo { (v - lo) / (hi - lo) } else { 1.0 };
    let blended = |c: &Candidate| {
        let f = norm(c.fine.expect("scored").score, f_lo, f_hi);
        let coarse = norm(c.score, c_lo, c_hi);
        opts.fine_blend * f + (1.0 - opts.fine_blend) * coarse
    };
    let mut order: Vec<usize> = (0..kept.len()).collect();
    order.sort_by(|&a, &b| match (&kept[a].fine, &kept[b].fine) {
        (Some(_), Some(_)) => blended(&kept[b]).total_cmp(&blended(&kept[a])),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    });
    let reordered: Vec<Candidate> = order.into_iter().map(|i| kept[i].clone()).collect();
    *kept = reordered;

    // Two strided neighbours can elect the *same* lines from their shared
    // region; showing both restates one answer twice. Same-file windows that
    // overlap by half the shorter span collapse to the higher-ranked one.
    let mut survivors: Vec<Candidate> = Vec::with_capacity(kept.len());
    for c in kept.drain(..) {
        let dup = c.fine.is_some_and(|f| {
            survivors.iter().any(|s| {
                s.path == c.path
                    && s.fine.is_some_and(|sf| window_overlaps(&sf, &f))
            })
        });
        if !dup {
            survivors.push(c);
        }
    }
    *kept = survivors;

    // Relevance for MMR: the blended score for scored candidates; unscored
    // ones trail below the scored minimum, keeping their relative order.
    let scored_min = kept
        .iter()
        .filter(|c| c.fine.is_some())
        .map(blended)
        .fold(f32::INFINITY, f32::min);
    let base = if scored_min.is_finite() { scored_min } else { 0.0 };
    let mut tail = 0;
    kept.iter()
        .map(|c| {
            if c.fine.is_some() {
                blended(c)
            } else {
                tail += 1;
                base - 0.001 * tail as f32
            }
        })
        .collect()
}

/// The best `fine_lines`-tall window of one chunk, scored against the
/// pre-quantized query vector. Ties go to the earliest window, and the winner
/// is trimmed of blank edge lines before its span is recorded.
fn best_window(root: &Path, c: &Candidate, q_i8: &[i8], w: usize) -> Option<Fine> {
    let body = corpus::lines(root, &c.path, &c.chunk)?;
    let lines: Vec<&str> = body.lines().collect();
    if lines.is_empty() {
        return None;
    }
    let w = w.min(lines.len());
    let mut best: Option<(usize, f32)> = None;
    for start in 0..=lines.len() - w {
        let window = &lines[start..start + w];
        if window.iter().all(|l| l.trim().is_empty()) {
            continue;
        }
        let mut v = text::embed_query(&window.join("\n"));
        rank::normalize(&mut v);
        let score = 1.0 - rank::dot_distance_i8(q_i8, &rank::quantize_i8(&v));
        match best {
            Some((_, s)) if s >= score => {}
            _ => best = Some((start, score)),
        }
    }
    let (start, score) = best?;
    let mut lo = start;
    let mut hi = start + w - 1;
    while lo < hi && lines[lo].trim().is_empty() {
        lo += 1;
    }
    while hi > lo && lines[hi].trim().is_empty() {
        hi -= 1;
    }
    Some(Fine {
        start_line: c.chunk.start_line + lo as u32,
        end_line: c.chunk.start_line + hi as u32,
        score,
    })
}

/// Do two fine windows in the same file share at least half the shorter one?
fn window_overlaps(a: &Fine, b: &Fine) -> bool {
    let lo = a.start_line.max(b.start_line);
    let hi = a.end_line.min(b.end_line);
    if lo > hi {
        return false;
    }
    let shared = (hi - lo + 1) as f32;
    let span = |f: &Fine| (f.end_line - f.start_line + 1) as f32;
    shared >= 0.5 * span(a).min(span(b))
}

fn min_max(vals: impl Iterator<Item = f32>) -> (f32, f32) {
    vals.fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), v| (lo.min(v), hi.max(v)))
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
///
/// When the fine rerank chose a window, the hit's span *is* that window and
/// the best-line search runs inside it — the agent-facing span must be the
/// tight one (RESEARCH.md §28.2). The whole chunk is still read and collected,
/// because a caller who explicitly asked for a wider passage
/// (`passage_override`) gets it cut from the chunk, anchored at the window.
fn materialize(
    root: &Path,
    c: &Candidate,
    query_tokens: &HashSet<String>,
    opts: &SearchOptions,
) -> Option<SearchHit> {
    let chunk = c.chunk;
    let text = corpus::read_text(&corpus::resolve(root, &c.path))?;
    // The span the best-line argmax runs over: the fine window when one was
    // chosen, else the whole chunk.
    let (span_start, span_end) = match &c.fine {
        Some(f) => (f.start_line, f.end_line),
        None => (chunk.start_line, chunk.end_line),
    };
    // Whether display shows exactly the fine window (the default with fine on)
    // or a cut of the chunk (no fine, or an explicit passage request).
    let window_is_passage = c.fine.is_some() && !opts.passage_override;
    // Both display extras are collected in this loop rather than by re-reading
    // the file in the CLI: the chunk's text is already in hand exactly once
    // here, and a second reader would be a second chance to disagree about
    // which lines a chunk covers.
    // Every line of the chunk is collected even when only a window will be
    // shown: the window is centred on the best-matching line, and which line
    // that is only becomes known once the loop below has finished.
    let want_lines = window_is_passage
        || opts.passage_lines > 1
        || (opts.passage_lines == 0 && opts.passage_chars > 0);
    let mut lines: Option<Vec<String>> = want_lines.then(Vec::new);
    let mut defines: Option<Vec<String>> = opts.defines.then(Vec::new);
    // Ranked by (query-token overlap, carries a word at all), first line
    // winning ties. The second term exists because the fine rerank made the
    // first one tie far more often: over a 32-line chunk some line almost
    // always shared a token with the query, but inside a 4-line window the
    // overlap is frequently zero everywhere, and the old first-wins fallback
    // then anchored the hit on whatever led the window — a bare `{` or `)`
    // in 8.3% of recorded snapshot hits. Overlap still dominates, so a line
    // that genuinely matches is never passed over for a prettier one.
    let mut best: Option<((usize, bool), u32, &str)> = None;
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
        if line.trim().is_empty() || line_no < span_start || line_no > span_end {
            continue;
        }
        let mut overlap = 0usize;
        tokenize::for_each_token(line, |tok| {
            if query_tokens.contains(tok) {
                overlap += 1;
            }
        });
        let rank = (overlap, line.chars().any(|c| c.is_alphanumeric()));
        match &best {
            Some((b, _, _)) if *b >= rank => {}
            _ => best = Some((rank, line_no, line)),
        }
    }
    let (_, line, line_text) = best?;
    // Cut the collected chunk down to the passage, and report where the cut
    // starts. `out.rs` numbers printed lines from `lines_from`, not from
    // `start_line` — without that the whole passage is misnumbered, which is
    // worse than showing nothing, since the line number is what the caller
    // navigates by.
    let (lines, lines_from) = match lines {
        None => (None, None),
        Some(all) => {
            let first = chunk.start_line;
            let (lo, hi) = if window_is_passage {
                ((span_start - first) as usize, (span_end - first) as usize)
            } else if opts.passage_lines > 0 {
                // Legacy line budget, kept so §26's campaign arms reproduce
                // under their own flag. 8 before / 9 after at 18: measured,
                // not chosen — a stronger forward bias loses coverage (§26.1).
                let at = (line - first) as usize;
                let before = ((opts.passage_lines - 1) / 2) as usize;
                let lo = at.saturating_sub(before);
                (lo, (lo + opts.passage_lines as usize - 1).min(all.len() - 1))
            } else {
                let at = (line - first) as usize;
                grow_to_budget(&all, at, opts.passage_chars)
            };
            let hi = hi.min(all.len().saturating_sub(1));
            let lo = lo.min(hi);
            let cut: Vec<String> = all[lo..=hi].to_vec();
            (Some(cut), Some(first + lo as u32))
        }
    };
    let (start_line, end_line) = match &c.fine {
        Some(f) => (f.start_line, f.end_line),
        None => (chunk.start_line, chunk.end_line),
    };
    Some(SearchHit {
        path: c.path.clone(),
        start_line,
        end_line,
        line,
        text: line_text.to_string(),
        score: c.fine.map_or(c.score, |f| f.score),
        chunk_start_line: c.fine.map(|_| chunk.start_line),
        chunk_end_line: c.fine.map(|_| chunk.end_line),
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
