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
    /// Which phrases of the query retrieved this candidate, as a bitmask over
    /// `Query::phrases` (bit 0 = first phrase). Single-phrase queries set bit
    /// 0 everywhere. Every dedupe that drops a candidate must union its mask
    /// into the survivor (RESEARCH.md §31): a phrase whose only representative
    /// dies by stride accident would otherwise read spuriously floored.
    pub phrases: u8,
    /// The best few-line window inside this chunk, once the fine rerank has
    /// scored it. `None` until then, and stays `None` when the rerank is off
    /// or the file could not be re-read.
    pub fine: Option<Fine>,
    /// This chunk's 1-based rank in the lexical (BM25) channel, when that
    /// channel ran and ranked it near its head. Provenance for the
    /// `bm25_pin` display guarantee (§32.4a: the shipped semantic mode never
    /// consults BM25, and real misses sat in BM25's top five). Dedupe
    /// survivors keep the better (smaller) rank, same rule as `phrases`.
    pub bm25_rank: Option<u16>,
    /// The structural boost's shares for this chunk (§35.1): fraction of query
    /// tokens declared in it / present in its path tail. 0.0 when the boost
    /// did not run or the chunk sat outside the boosted head. Candidate-local
    /// facts carried as features for the learned checklist (§35.2); a dedupe
    /// survivor keeps its own — they describe the surviving chunk, not the
    /// group.
    pub decl_share: f32,
    pub path_share: f32,
}

/// The fine rerank's verdict on one candidate: the sub-window of its chunk
/// that matches the query best, and how well (cosine in the fine space,
/// [-1, 1], comparable across queries — and across phrases, which is what
/// lets a merged multi-phrase pool be ordered by one number).
#[derive(Debug, Clone, Copy)]
pub struct Fine {
    /// 1-based, inclusive, blank edges already trimmed.
    pub start_line: u32,
    pub end_line: u32,
    pub score: f32,
    /// Index of the phrase this window scored best against (0 for a
    /// single-phrase query).
    pub phrase: u8,
}

/// What finalization produced, and whether the score floor refused it.
/// `best_signal` is populated whenever a floor was set — on success too, so a
/// calibration campaign can join score to outcome from the report alone.
pub struct Finalized {
    pub hits: Vec<SearchHit>,
    pub floored: bool,
    pub best_signal: Option<f32>,
    /// Per-phrase best signals, `Some` only for a multi-phrase query with a
    /// floor set — the footer's "nothing matched 'X'" reads from this.
    pub phrase_signals: Option<Vec<f32>>,
    /// Which phrases the floor refused, as a bitmask aligned with
    /// `Query::phrases`. The decision is made here, once — the footer must
    /// not re-derive it, because it does not know the threshold.
    pub floored_mask: u8,
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
    q: &super::Query,
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
            match kept
                .iter_mut()
                .find(|k| k.path == c.path && overlaps(k, &c, opts.dedupe_overlap))
            {
                // The dropped near-duplicate's retrievers survive on its
                // killer (§31): two phrases hitting adjacent strided chunks
                // of one function collapse to one candidate that still
                // answers for both. Its lexical rank survives the same way.
                Some(k) => {
                    k.phrases |= c.phrases;
                    k.bm25_rank = match (k.bm25_rank, c.bm25_rank) {
                        (Some(a), Some(b)) => Some(a.min(b)),
                        (a, b) => a.or(b),
                    };
                }
                None => kept.push(c),
            }
        }
        kept
    });

    // The pre-fine winner, remembered before any later stage can demote it.
    // §32.4a measured the fine rerank evicting a coarse rank-1 from the
    // display entirely; with the guard on, that candidate may be outranked
    // but never dropped.
    let coarse_top_id: Option<u32> =
        (opts.keep_coarse_top).then(|| kept.first().map(|c| c.id)).flatten();

    // Fine rerank: pick the best few-line window inside each surviving chunk
    // and let the windows' scores order the list (RESEARCH.md §28.2, §31).
    // Between dedupe and MMR because dedupe is a property of the chunks and
    // MMR must rank what will actually be shown.
    let fine_out: Option<(Vec<f32>, Vec<f32>)> = trace.time(Stage::FinalizeFine, || {
        (opts.fine_rerank && opts.fine_lines >= 1)
            .then(|| fine_rerank(root, &q.phrases, &mut kept, opts))
    });
    let mut relevance: Option<Vec<f32>> = fine_out.as_ref().map(|(r, _)| r.clone());

    // The score floor (§28.2, per-phrase since §31): judged here — the tail
    // both paths share — so a cached scope refuses exactly what an uncached
    // one refuses. A floored phrase's exclusive candidates drop while the
    // other phrases still answer; only all-phrases-floored refuses outright.
    let mut phrase_signals: Option<Vec<f32>> = None;
    let mut best_signal: Option<f32> = None;
    let mut kept_floored_mask: u8 = 0;
    if opts.min_score > 0.0 && !kept.is_empty() {
        let signals = match &fine_out {
            Some((_, per_phrase)) => per_phrase.clone(),
            None => trace.time(Stage::FinalizeFine, || {
                pool_signal_coarse(&q.phrases, &kept, &vec_of)
            }),
        };
        let finite_max =
            signals.iter().copied().filter(|s| s.is_finite()).fold(f32::NEG_INFINITY, f32::max);
        best_signal = finite_max.is_finite().then_some(finite_max);
        // A phrase with no finite signal (nothing scorable) abstains rather
        // than counting as floored — refusing on no evidence is worse than
        // answering weakly.
        let floored_mask: u8 = signals
            .iter()
            .enumerate()
            .filter(|(_, s)| s.is_finite() && **s < opts.min_score)
            .fold(0u8, |m, (i, _)| m | (1 << i));
        let all_mask: u8 = (0..q.phrases.len()).fold(0u8, |m, i| m | (1 << i));
        if q.is_multi() {
            phrase_signals = Some(signals);
        }
        if floored_mask == all_mask && best_signal.is_some() {
            return Finalized {
                hits: Vec::new(),
                floored: true,
                best_signal,
                phrase_signals,
                floored_mask,
            };
        }
        kept_floored_mask = floored_mask;
        if floored_mask != 0 {
            // Candidates only a floored phrase retrieved carry nothing the
            // surviving phrases asked for. The relevance vector filters in
            // lockstep — recomputing it would lose the blended semantics
            // `fine_rerank` already resolved.
            let keep_idx: Vec<bool> =
                kept.iter().map(|c| c.phrases & !floored_mask != 0).collect();
            let mut it = keep_idx.iter();
            kept.retain(|_| *it.next().expect("aligned"));
            if let Some(r) = relevance.as_mut() {
                let mut it = keep_idx.iter();
                r.retain(|_| *it.next().expect("aligned"));
            }
        }
    }

    // The learned checklist (§35.2): rewrite the relevance MMR consumes and
    // reorder `kept` in lockstep — the reorder matters because MMR is skipped
    // outright on short pools, where `kept`'s own order is final. After the
    // floor on purpose: the floor is judged on fine cosine (a calibrated
    // physical signal), and a learned score must not move it.
    if opts.learned_blend > 0.0 && !kept.is_empty() {
        trace.time(Stage::FinalizeRerank, || {
            let mut rel = relevance
                .take()
                .unwrap_or_else(|| kept.iter().map(|c| c.score).collect());
            super::checklist::blend(&mut kept, &mut rel, opts.learned_blend);
            relevance = Some(rel);
        });
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
        // One token set per phrase, so the best-line anchor inside a hit is
        // chosen by the phrase that retrieved it, not by a union that could
        // anchor phrase A's window on phrase B's word.
        let phrase_tokens: Vec<HashSet<String>> =
            q.phrases.iter().map(|p| tokenize::tokens(p).into_iter().collect()).collect();
        let materialize_one = |c: &Candidate| -> Option<SearchHit> {
            let toks = &phrase_tokens[c.fine.map_or(0, |f| f.phrase as usize)];
            let mut hit = materialize(root, c, toks, opts)?;
            if !strip.is_empty()
                && let Some(rest) = hit.path.strip_prefix(&format!("{strip}/"))
            {
                hit.path = rest.to_string();
            }
            if q.is_multi() {
                hit.phrase = c.fine.map(|f| f.phrase as u32);
            }
            Some(hit)
        };
        let mut hits = Vec::with_capacity(opts.k);
        let mut used: Vec<usize> = Vec::new();
        // Which kept-index each display slot holds, kept in lockstep with
        // `hits` through every later swap — `used` is the consumed set and
        // cannot answer "is candidate X still on screen" once a swap evicts.
        let mut slots: Vec<usize> = Vec::new();
        for i in order {
            if let Some(hit) = materialize_one(&kept[i]) {
                hits.push(hit);
                used.push(i);
                slots.push(i);
            }
            if hits.len() == opts.k {
                break;
            }
        }
        // Representation pass (§31): a phrase that retrieved candidates and
        // was not floored deserves at least its best hit in the top-k —
        // otherwise one hot phrase eats every slot an agent will read. One
        // bounded swap per absent phrase, replacing a hit of the
        // most-represented phrase, lowest-ranked first.
        if q.is_multi() {
            for p in 0..q.phrases.len() as u8 {
                if kept_floored_mask & (1 << p) != 0 {
                    continue; // a floored phrase never pins (§31)
                }
                let present = hits
                    .iter()
                    .any(|h| h.phrase == Some(p as u32));
                if present {
                    continue;
                }
                let Some(best) = (0..kept.len())
                    .filter(|i| !used.contains(i))
                    .filter(|&i| kept[i].fine.is_some_and(|f| f.phrase == p))
                    .max_by(|&a, &b| {
                        let fa = kept[a].fine.expect("filtered").score;
                        let fb = kept[b].fine.expect("filtered").score;
                        fa.total_cmp(&fb)
                    })
                else {
                    continue;
                };
                let Some(hit) = materialize_one(&kept[best]) else { continue };
                if hits.len() < opts.k {
                    hits.push(hit);
                    used.push(best);
                    slots.push(best);
                    continue;
                }
                // Evict the lowest-ranked hit of the most-represented phrase.
                let mut counts = std::collections::HashMap::new();
                for h in &hits {
                    *counts.entry(h.phrase).or_insert(0usize) += 1;
                }
                let Some((victim_idx, _)) = hits
                    .iter()
                    .enumerate()
                    .max_by_key(|(i, h)| (counts[&h.phrase], *i))
                else {
                    continue;
                };
                hits[victim_idx] = hit;
                slots[victim_idx] = best;
                used.push(best);
            }
        }
        // Display guarantees (§32.4a), after every rank-driven swap so they
        // cannot be undone. Each pin claims at most one slot, evicting from
        // the tail; a slot already holding a pinned candidate is never the
        // victim of a later pin. Floored candidates were filtered out of
        // `kept` above, so a pin can never resurrect a refusal.
        let is_pinned = |kept_i: usize, kept: &[Candidate]| {
            coarse_top_id == Some(kept[kept_i].id)
                || (opts.bm25_pin > 0
                    && kept[kept_i].bm25_rank.is_some_and(|r| r as usize <= opts.bm25_pin))
        };
        let mut pin = |want: usize,
                       hits: &mut Vec<SearchHit>,
                       slots: &mut Vec<usize>,
                       used: &mut Vec<usize>| {
            if slots.contains(&want) {
                return;
            }
            let Some(hit) = materialize_one(&kept[want]) else { return };
            if hits.len() < opts.k {
                hits.push(hit);
                slots.push(want);
                used.push(want);
                return;
            }
            let Some(victim) =
                (0..hits.len()).rev().find(|&s| !is_pinned(slots[s], &kept))
            else {
                return;
            };
            hits[victim] = hit;
            slots[victim] = want;
            used.push(want);
        };
        if let Some(tid) = coarse_top_id
            && let Some(want) = kept.iter().position(|c| c.id == tid)
        {
            pin(want, &mut hits, &mut slots, &mut used);
        }
        if opts.bm25_pin > 0 {
            let mut wants: Vec<usize> = (0..kept.len())
                .filter(|&i| {
                    kept[i].bm25_rank.is_some_and(|r| r as usize <= opts.bm25_pin)
                })
                .collect();
            wants.sort_by_key(|&i| kept[i].bm25_rank.expect("filtered"));
            for want in wants {
                pin(want, &mut hits, &mut slots, &mut used);
            }
        }
        hits
    });
    Finalized { hits, floored: false, best_signal, phrase_signals, floored_mask: kept_floored_mask }
}

/// The floor's fallback signal when the fine rerank is off: per-phrase best
/// chunk-embedding cosine, each phrase judged only against the candidates it
/// retrieved, through the same stored quantization MMR diversifies with.
/// `NEG_INFINITY` for a phrase with nothing scorable, and the caller treats
/// that as abstention — refusing on no evidence is worse than answering.
fn pool_signal_coarse(
    phrases: &[String],
    kept: &[Candidate],
    vec_of: &impl Fn(&Candidate) -> Option<Vec<f32>>,
) -> Vec<f32> {
    phrases
        .iter()
        .enumerate()
        .map(|(p, phrase)| {
            let mut q = text::embed_query(phrase);
            rank::normalize(&mut q);
            let q = rank::as_stored(&q);
            kept.iter()
                .filter(|c| c.phrases & (1 << p) != 0)
                .filter_map(vec_of)
                .map(|v| 1.0 - rank::distance(&q, &v))
                .fold(f32::NEG_INFINITY, f32::max)
        })
        .collect()
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
/// Returns `(relevance for MMR, per-phrase best signals)`, both the products
/// of one scoring pass. The per-phrase maxes must be tracked *here*, across
/// every (candidate, retriever) evaluation — `Fine` keeps only each
/// candidate's winning phrase, so a phrase that always came second would
/// otherwise read spuriously floored (§31).
fn fine_rerank(
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
    for ((c, (fine, maxes))) in kept.iter_mut().zip(scored) {
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
                s.path == c.path
                    && s.fine.is_some_and(|sf| window_overlaps(&sf, &f))
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
    let scored_min = kept
        .iter()
        .filter(|c| c.fine.is_some())
        .map(blended)
        .fold(f32::INFINITY, f32::min);
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
    // The unit view (RESEARCH.md §34), computed here and not in the CLI for
    // the same one-reader reason as `lines`/`defines` above: the whole file
    // is already in hand. Three gates, each keeping a measured surface
    // byte-identical: `passage_override` (an asked-for passage shape wins,
    // which also pins the snapshot's `--passage-lines 1` recording),
    // `fine.is_some()` (`--no-fine` documents itself as pre-§28.2 output
    // byte for byte), and the option itself (`--no-unit`, the A/B control).
    let unit_rows = (opts.unit_view && !opts.passage_override && c.fine.is_some()).then(|| {
        let all: Vec<&str> = text.lines().collect();
        super::unit::compute(&all, &c.path, start_line, end_line)
    });
    Some(SearchHit {
        path: c.path.clone(),
        start_line,
        end_line,
        line,
        text: line_text.to_string(),
        score: c.fine.map_or(c.score, |f| f.score),
        chunk_start_line: c.fine.map(|_| chunk.start_line),
        chunk_end_line: c.fine.map(|_| chunk.end_line),
        // The caller (finalize) overwrites this for a multi-phrase query; a
        // single-phrase hit stays None so its JSON is unchanged.
        phrase: None,
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
        unit_rows,
        features: opts.debug_features.then(|| super::HitFeatures {
            coarse: c.score,
            fine: c.fine.map(|f| f.score),
            bm25_rank: c.bm25_rank,
            phrases: c.phrases,
            decl_share: c.decl_share,
            path_share: c.path_share,
            chunk_lines: chunk.end_line.saturating_sub(chunk.start_line) + 1,
        }),
    })
}
