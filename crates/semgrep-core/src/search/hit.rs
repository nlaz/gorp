//! Candidates into displayable hits: span dedupe, MMR diversification, and
//! re-reading the best line out of the file.

use super::{SearchHit, SearchOptions};
use super::materialize::{materialize, overlaps};
use super::rerank::fine_rerank;
use crate::rank::{self, mmr};
use crate::text::token as tokenize;
use crate::trace::{Stage, Trace};
use crate::{Chunk, text};
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
        let pin = |want: usize,
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

