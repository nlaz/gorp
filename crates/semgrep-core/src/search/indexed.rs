//! The warm path: an index answers, with a read-repair overlay fused in.
//!
//! Read top to bottom, `run` is the whole query: load what this mode needs,
//! build the row space, rank lexically, rank semantically, fuse, materialize.
//! Each stage is a function; the reranking knobs (PRF, MaxSim) are stages that
//! pass their input through when switched off.

use super::rows::Rows;
use super::{Prelude, SearchOptions, SearchReport, SearchResult, hit};
use crate::rank::bm25::Postings;
use crate::rank::{self, Mode, maxsim, prf};
use crate::store::{LoadNeeds, LoadedIndex};
use crate::trace::{SCHEDULE_WARM, Stage, Trace, elapsed_ms};
use crate::{cache, corpus, text};
use anyhow::Result;
use std::time::Instant;

pub fn run(
    d: &cache::Discovered,
    query: &str,
    opts: &SearchOptions,
    prelude: &Prelude,
) -> Result<SearchResult> {
    let pool = super::FUSION_POOL.max(opts.k);
    let mut trace = Trace::new(SCHEDULE_WARM);
    trace.replay(prelude);

    let idx = LoadedIndex::load_dir(&d.index_dir, &d.root, load_needs(d, opts, pool))?;
    for (stage, ms) in [
        (Stage::LoadMeta, idx.timings.meta_ms),
        (Stage::LoadChunks, idx.timings.chunks_ms),
        (Stage::LoadBm25, idx.timings.bm25_ms),
        (Stage::LoadMmap, idx.timings.mmap_ms),
        (Stage::LoadHnsw, idx.timings.hnsw_ms),
        (Stage::LoadSif, idx.timings.sif_ms),
    ] {
        trace.record(stage, ms);
    }

    let (repair, repair_outcome) =
        cache::repair::scope(d, &idx, opts.repair_max_drift, &mut trace);
    // Handed back up rather than decided here: this path can see that the entry
    // is too stale to patch, but replacing it is `search`'s job — it owns the
    // write-through build and the re-discovery that follows it.
    if let cache::repair::RepairOutcome::DriftTooLarge { dirty, total, .. } = repair_outcome {
        return Err(anyhow::Error::new(super::DriftTooLarge { dirty, total }));
    }
    let rows = Rows::new(&idx, repair, &d.prefix);

    let lexical = trace.time(Stage::RankBm25, || rank_lexical(&idx, &rows, query, opts, pool));
    let lexical = expand_query(&idx, &rows, query, lexical, opts, d, pool, &mut trace);
    let (semantic, used_hnsw) = rank_semantic(&idx, &rows, query, opts, d, pool, &mut trace);
    let ranked = trace.time(Stage::RankFuse, || {
        rank::fuse(opts.mode, lexical, semantic, super::fused_width(pool), opts.sem_weight)
    });
    // Post-fusion variant: MaxSim reorders the FUSED list. The pre-fusion call
    // inside rank_semantic is skipped in this mode, so the rerank happens once
    // either way.
    let ranked = if opts.maxsim_post && opts.rerank_maxsim {
        rerank_fused(&rows, query, ranked, opts, d, &idx, &mut trace)
    } else {
        ranked
    };

    let cands = trace.time(Stage::Candidates, || {
        candidates(&rows, ranked, &d.prefix, super::candidate_width(opts.k))
    });
    let hits =
        hit::finalize(&d.root, query, cands, opts, &d.prefix, &mut trace, |c| rows.vector(c.id));

    Ok(SearchResult {
        report: SearchReport {
            used_index: true,
            used_hnsw,
            stale_files: if opts.check_stale {
                idx.stale_files().unwrap_or(0)
            } else {
                rows.n_dirty()
            },
            n_chunks_considered: rows.len(),
            files_walked: idx.meta.files.len(),
            repair: repair_outcome,
            stages: trace.finish(),
            ..Default::default()
        },
        hits,
    })
}

/// Which index components this query actually needs off disk.
///
/// The HNSW cap is the interesting part. A one-shot CLI process pays full graph
/// deserialization per query — about 20 s for a kernel-scale 3.1 GB `hnsw.bin`,
/// against 3.5 s to brute-force the mmap'd matrix instead. So the graph is only
/// worth loading while it is small. A persistent server would amortize the load
/// and should always use it.
fn load_needs(d: &cache::Discovered, opts: &SearchOptions, pool: usize) -> LoadNeeds {
    const HNSW_LOAD_CAP_BYTES: u64 = 1 << 30;
    let graph_is_small = std::fs::metadata(d.index_dir.join("hnsw.bin"))
        .map(|m| m.len() < HNSW_LOAD_CAP_BYTES)
        .unwrap_or(false);
    LoadNeeds {
        bm25: matches!(opts.mode, Mode::Bm25 | Mode::Hybrid),
        // HNSW returns up to a compile-time 128 candidates, so a larger pool
        // has to brute-force regardless.
        hnsw: matches!(opts.mode, Mode::Semantic | Mode::Hybrid)
            && opts.use_hnsw
            && pool <= 128
            && graph_is_small,
    }
}

fn rank_lexical(
    idx: &LoadedIndex,
    rows: &Rows,
    query: &str,
    opts: &SearchOptions,
    pool: usize,
) -> Vec<(u32, f32)> {
    let Some(base) = idx.bm25.as_ref().filter(|_| lexical_mode(opts.mode)) else {
        return Vec::new();
    };
    // The overlay holds only the files that drifted, so it scores against the
    // base's corpus statistics rather than its own handful of documents —
    // otherwise the two lists being merged are on different scales.
    let rest = rank::Rest {
        n_docs: base.n_docs(),
        total_len: base.total_len(),
        df: &|token| base.df(token),
    };
    let delta = rows
        .delta_bm25()
        .map(|b| rank::top_k_within(b, query, pool, Some(&rest)))
        .unwrap_or_default();
    // Base ids are row ids here, so the scope predicate applies directly. The
    // overlay is already scoped — `repair::scope` only walks the query's subtree
    // — and `merge` re-checks both anyway.
    let in_scope = |id: u32| rows.serves(id);
    let base_hits = rank::top_k_scoped(
        base,
        query,
        pool,
        None,
        (!rows.whole_corpus()).then_some(&in_scope as &dyn Fn(u32) -> bool),
    );
    rows.merge(base_hits, delta, pool, true)
}

fn lexical_mode(mode: Mode) -> bool {
    matches!(mode, Mode::Bm25 | Mode::Hybrid)
}

/// PRF: re-run the lexical pass with a query grown from its own top hits.
/// Passes `lexical` straight through when disabled or when there is no lexical
/// index to mine.
#[allow(clippy::too_many_arguments)]
fn expand_query(
    idx: &LoadedIndex,
    rows: &Rows,
    query: &str,
    lexical: Vec<(u32, f32)>,
    opts: &SearchOptions,
    d: &cache::Discovered,
    pool: usize,
    trace: &mut Trace,
) -> Vec<(u32, f32)> {
    if opts.prf_terms == 0 {
        return lexical;
    }
    let Some(store) = &idx.bm25 else { return lexical };

    trace.time(Stage::RankPrf, || {
        // Feedback comes from the head of the first pass; ten is what the eval
        // used, and reading more costs a file read each for less signal.
        let texts: Vec<String> = lexical
            .iter()
            .take(10)
            .filter_map(|&(id, _)| {
                let (chunk, path) = rows.chunk(id);
                corpus::lines(&d.root, &path, &chunk)
            })
            .collect();
        let terms = prf::expansion_terms(query, &texts, store, opts.prf_terms);
        if terms.is_empty() {
            return lexical;
        }
        let expanded = prf::expand(query, &terms);
        rank_lexical(idx, rows, &expanded, opts, pool)
    })
}

/// Returns the semantic list and whether the graph answered it.
fn rank_semantic(
    idx: &LoadedIndex,
    rows: &Rows,
    query: &str,
    opts: &SearchOptions,
    d: &cache::Discovered,
    pool: usize,
    trace: &mut Trace,
) -> (Vec<(u32, f32)>, bool) {
    if !matches!(opts.mode, Mode::Semantic | Mode::Hybrid) {
        return (Vec::new(), false);
    }

    // A SIF index pools chunks by rarity weight, so the query has to be pooled
    // with the same corpus statistics or the distances mean nothing. Same for
    // prose rendering: the index's own meta decides, never a flag — the stored
    // vectors dictate the space (RESEARCH.md §14.2).
    let q = trace.time(Stage::RankEmbedQuery, || {
        let rendered = text::prose_render(query, idx.meta.embed_preproc);
        let mut q = match &idx.sif {
            Some(s) => text::embed_sif(&rendered, s),
            None => text::embed_query(&rendered),
        };
        rank::normalize(&mut q);
        q
    });

    // A scoped query cannot use the graph. HNSW returns its own bounded
    // neighbour list and there is no way to tell it to skip rows, so filtering
    // its output is the filter-after-truncate mistake again — the brute-force
    // scan takes the predicate instead, and under a scope it is *cheaper*
    // because most rows never reach the kernel.
    let use_graph = idx.hnsw.is_some() && opts.use_hnsw && pool <= 128 && rows.whole_corpus();
    let start = Instant::now();
    let quantized = rank::quantize_i8(&q);
    let in_scope = |id: u32| rows.serves(id);
    let base = match (&idx.hnsw, use_graph) {
        (Some(graph), true) => {
            graph.search(&q).into_iter().map(|(dist, id)| (id, dist)).take(pool).collect()
        }
        _ => rank::brute_force_top_k_i8_where(
            &quantized,
            idx.emb_matrix_i8(),
            pool,
            (!rows.whole_corpus())
                .then_some(&in_scope as &(dyn Fn(u32) -> bool + Sync)),
        ),
    };
    // The same kernel and the same quantized query the base was scored with, so
    // the two lists being merged are on one scale.
    let delta: Vec<(u32, f32)> = rows
        .delta_vectors()
        .iter()
        .enumerate()
        .map(|(j, v)| (j as u32, rank::dot_distance_i8(&quantized, v)))
        .collect();
    let ranked = rows.merge(base, delta, pool, false);
    // Both stages are on the schedule and one of them is always zero, so the
    // report has the same keys whether or not the graph answered.
    trace.record(if use_graph { Stage::RankAnn } else { Stage::RankBrute }, elapsed_ms(start));

    let ranked = rerank_maxsim(rows, query, ranked, opts, d, idx, trace);
    (ranked, use_graph)
}

/// MaxSim rerank over the head of the semantic list. Passes through when
/// disabled, when the list is empty, or when the query has no token vectors.
#[allow(clippy::too_many_arguments)]
/// Post-fusion rerank, with the sign conversion the two contracts require.
///
/// `fuse` emits **higher-is-better** scores; `blend_head` consumes and emits
/// **lower-is-better** pseudo-distances. Feeding one to the other directly
/// inverts the fused order — measured, before this conversion existed, as
/// hybrid R@5 0.770 -> 0.058. The giveaway was that blend 0.3, which should
/// mostly preserve the original order, scored *worse* than blend 1.0: the
/// order it was preserving was upside down.
fn rerank_fused(
    rows: &Rows,
    query: &str,
    ranked: Vec<(u32, f32)>,
    opts: &SearchOptions,
    d: &cache::Discovered,
    idx: &LoadedIndex,
    trace: &mut Trace,
) -> Vec<(u32, f32)> {
    let as_dist: Vec<(u32, f32)> = ranked.iter().map(|&(id, s)| (id, -s)).collect();
    let o = SearchOptions { maxsim_post: false, ..opts.clone() };
    let out = rerank_maxsim(rows, query, as_dist, &o, d, idx, trace);
    out.into_iter().map(|(id, pd)| (id, -pd)).collect()
}

fn rerank_maxsim(
    rows: &Rows,
    query: &str,
    ranked: Vec<(u32, f32)>,
    opts: &SearchOptions,
    d: &cache::Discovered,
    idx: &LoadedIndex,
    trace: &mut Trace,
) -> Vec<(u32, f32)> {
    if !opts.rerank_maxsim || opts.maxsim_post || ranked.is_empty() {
        return ranked;
    }
    let pp = idx.meta.embed_preproc;
    let query_tokens = text::token_vectors(&text::prose_render(query, pp), idx.sif.as_ref());
    if query_tokens.is_empty() {
        return ranked;
    }

    trace.time(Stage::RankMaxsim, || {
        use rayon::prelude::*;
        let head = maxsim::head_size(ranked.len(), opts.k, opts.maxsim_pool);
        let scored: Vec<(u32, f32, f32)> = ranked[..head]
            .par_iter()
            .map(|&(id, dist)| {
                let (chunk, path) = rows.chunk(id);
                // A candidate whose file has become unreadable scores as far
                // away as possible rather than dropping out of the list.
                let sim = corpus::lines(&d.root, &path, &chunk)
                    .map(|text| {
                        let raw = corpus::doc_text(&path, &text);
                        let doc =
                            text::token_vectors(&text::prose_render(&raw, pp), None);
                        rank::maxsim(&query_tokens, &doc)
                    })
                    .unwrap_or(f32::NEG_INFINITY);
                (id, sim, dist)
            })
            .collect();

        let mut out = maxsim::blend_head(&scored, opts.maxsim_blend);
        out.extend_from_slice(&ranked[head..]);
        out
    })
}

/// Fused ids into candidates, dropping anything outside the query's subtree.
///
/// The scope filter runs before the truncation, not after: a query against a
/// subdirectory fuses over the whole corpus, and out-of-scope rows would
/// otherwise eat every result slot.
fn candidates(
    rows: &Rows,
    ranked: Vec<(u32, f32)>,
    prefix: &str,
    limit: usize,
) -> Vec<hit::Candidate> {
    let in_scope = |path: &str| {
        prefix.is_empty() || path.strip_prefix(prefix).is_some_and(|r| r.starts_with('/'))
    };
    ranked
        .into_iter()
        .filter_map(|(id, score)| {
            let (chunk, path) = rows.chunk(id);
            in_scope(&path).then_some(hit::Candidate { id, chunk, path, score })
        })
        .take(limit)
        .collect()
}
