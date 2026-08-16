//! The fine rerank: score every sub-window of a candidate's chunk against
//! the query and keep the best one.
//!
//! The fine space is deliberately self-contained — query and windows are both
//! embedded from raw text, with no path line, no SIF stats and no prose
//! render — which is what lets the cold and warm paths share this code with
//! no index state threaded in (RESEARCH.md §29.1).

use super::SearchOptions;
use super::hit::{Candidate, Fine};
use crate::{corpus, rank, text};
use std::path::Path;

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
/// Returns `(relevance for MMR, per-phrase best signals)`, both the products
/// of one scoring pass. The per-phrase maxes must be tracked *here*, across
/// every (candidate, retriever) evaluation — `Fine` keeps only each
/// candidate's winning phrase, so a phrase that always came second would
/// otherwise read spuriously floored (§31).
pub(super) fn fine_rerank(
    root: &Path,
    phrases: &[String],
    kept: &mut Vec<Candidate>,
    opts: &SearchOptions,
) -> (Vec<f32>, Vec<f32>) {
    let queries: Vec<Vec<i8>> = phrases
        .iter()
        .map(|p| {
            let mut q = text::embed_query(p);
            rank::normalize(&mut q);
            rank::quantize_i8(&q)
        })
        .collect();
    use rayon::prelude::*;
    let scored: Vec<(Option<Fine>, Vec<f32>)> = kept
        .par_iter()
        .map(|c| best_window(root, c, &queries, opts.fine_lines as usize))
        .collect();
    let mut per_phrase = vec![f32::NEG_INFINITY; phrases.len()];
    for (c, (fine, maxes)) in kept.iter_mut().zip(scored) {
        c.fine = fine;
        for (p, m) in maxes.into_iter().enumerate() {
            per_phrase[p] = per_phrase[p].max(m);
        }
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
    // overlap by half the shorter span collapse to the higher-ranked one —
    // which inherits the dropped window's retrievers (§31), same rule as the
    // chunk dedupe and for the same reason.
    let mut survivors: Vec<Candidate> = Vec::with_capacity(kept.len());
    for c in kept.drain(..) {
        let killer = c.fine.and_then(|f| {
            survivors.iter().position(|s| {
                s.path == c.path && s.fine.is_some_and(|sf| window_overlaps(&sf, &f))
            })
        });
        match killer {
            Some(j) => {
                survivors[j].phrases |= c.phrases;
                survivors[j].bm25_rank = match (survivors[j].bm25_rank, c.bm25_rank) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (a, b) => a.or(b),
                };
            }
            None => survivors.push(c),
        }
    }
    *kept = survivors;

    // Relevance for MMR: the blended score for scored candidates; unscored
    // ones trail below the scored minimum, keeping their relative order.
    let scored_min =
        kept.iter().filter(|c| c.fine.is_some()).map(blended).fold(f32::INFINITY, f32::min);
    let base = if scored_min.is_finite() { scored_min } else { 0.0 };
    let mut tail = 0;
    let relevance = kept
        .iter()
        .map(|c| {
            if c.fine.is_some() {
                blended(c)
            } else {
                tail += 1;
                base - 0.001 * tail as f32
            }
        })
        .collect();
    (relevance, per_phrase)
}

/// The best `fine_lines`-tall window of one chunk, scored against every
/// phrase that retrieved this candidate — each window embedded ONCE and
/// dotted per retriever, so a doubly-retrieved chunk does not pay double
/// embedding. Ties go to the earliest window, and the winner is trimmed of
/// blank edge lines before its span is recorded.
///
/// Also returns this candidate's best score per phrase (NEG_INFINITY for
/// phrases that did not retrieve it), which `fine_rerank` folds into the
/// per-phrase floor signals.
fn best_window(
    root: &Path,
    c: &Candidate,
    queries: &[Vec<i8>],
    w: usize,
) -> (Option<Fine>, Vec<f32>) {
    let mut maxes = vec![f32::NEG_INFINITY; queries.len()];
    let Some(body) = corpus::lines(root, &c.path, &c.chunk) else {
        return (None, maxes);
    };
    let lines: Vec<&str> = body.lines().collect();
    if lines.is_empty() {
        return (None, maxes);
    }
    let w = w.min(lines.len());
    let mut best: Option<(usize, u8, f32)> = None;
    for start in 0..=lines.len() - w {
        let window = &lines[start..start + w];
        if window.iter().all(|l| l.trim().is_empty()) {
            continue;
        }
        let mut v = text::embed_query(&window.join("\n"));
        rank::normalize(&mut v);
        let v_i8 = rank::quantize_i8(&v);
        for (p, q_i8) in queries.iter().enumerate() {
            if c.phrases & (1 << p) == 0 {
                continue;
            }
            let score = 1.0 - rank::dot_distance_i8(q_i8, &v_i8);
            maxes[p] = maxes[p].max(score);
            match best {
                Some((_, _, s)) if s >= score => {}
                _ => best = Some((start, p as u8, score)),
            }
        }
    }
    let Some((start, phrase, score)) = best else {
        return (None, maxes);
    };
    let mut lo = start;
    let mut hi = start + w - 1;
    while lo < hi && lines[lo].trim().is_empty() {
        lo += 1;
    }
    while hi > lo && lines[hi].trim().is_empty() {
        hi -= 1;
    }
    let fine = Fine {
        start_line: c.chunk.start_line + lo as u32,
        end_line: c.chunk.start_line + hi as u32,
        score,
        phrase,
    };
    (Some(fine), maxes)
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
