//! The warm path: an index answers, with a read-repair overlay fused in.
//!
//! Read top to bottom, `run` is the whole query: load what this mode needs,
//! build the row space, rank lexically, rank semantically, fuse, materialize.
//! Each stage is a function; the reranking knobs (PRF, MaxSim) are stages that
//! pass their input through when switched off.

use super::rows::Rows;
use super::trace::{Trace, elapsed_ms};
use super::{SearchOptions, SearchReport, SearchResult, hit};
use crate::rank::{self, Mode, maxsim, prf};
use crate::store::{LoadNeeds, LoadedIndex};
use crate::{cache, corpus, text};
use anyhow::Result;
use std::time::Instant;

pub fn run(d: &cache::Discovered, query: &str, opts: &SearchOptions) -> Result<SearchResult> {
    let pool = super::FUSION_POOL.max(opts.k);
    let mut trace = Trace::new();

    let t_load = Instant::now();
    let idx = LoadedIndex::load_dir(&d.index_dir, &d.root, load_needs(d, opts, pool))?;
    let load_ms = t_load.elapsed().as_millis();
    for (stage, ms) in [
        ("load:meta", idx.timings.meta_ms),
        ("load:chunks", idx.timings.chunks_ms),
        ("load:bm25", idx.timings.bm25_ms),
        ("load:mmap", idx.timings.mmap_ms),
        ("load:hnsw", idx.timings.hnsw_ms),
    ] {
        trace.record(stage, ms);
    }

    let repair = cache::repair::scope(d, &idx, &mut trace);
    let rows = Rows::new(&idx, repair);

    let t_rank = Instant::now();
    let lexical = trace.time("rank:bm25", || rank_lexical(&idx, &rows, query, opts, pool));
    let lexical = expand_query(&idx, &rows, query, lexical, opts, d, pool, &mut trace);
    let (semantic, used_hnsw) = rank_semantic(&idx, &rows, query, opts, d, pool, &mut trace);
    let ranked = trace.time("rank:fuse", || {
        rank::fuse(opts.mode, lexical, semantic, pool * 2, opts.sem_weight)
    });
    let rank_ms = t_rank.elapsed().as_millis();

    let cands = candidates(&rows, ranked, &d.prefix, opts.k * 3);
    let hits = trace.time("finalize", || {
        hit::finalize(&d.root, query, cands, opts, &d.prefix, |c| rows.vector(c.id))
    });

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
            walk_ms: load_ms,
            rank_ms,
            stages: trace.into_stages(),
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
    let delta = rows.delta_bm25().map(|b| b.query(query, pool)).unwrap_or_default();
    rows.merge(base.query(query, pool), delta, pool, true)
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

    trace.time("rank:prf", || {
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
    // with the same corpus statistics or the distances mean nothing.
    let q = trace.time("rank:embed-query", || {
        let mut q = match &idx.sif {
            Some(s) => text::embed_sif(query, s),
            None => text::embed_query(query),
        };
        rank::normalize(&mut q);
        q
    });

    let use_graph = idx.hnsw.is_some() && opts.use_hnsw && pool <= 128;
    let start = Instant::now();
    let base = match (&idx.hnsw, use_graph) {
        (Some(graph), true) => {
            graph.search(&q).into_iter().map(|(dist, id)| (id, dist)).take(pool).collect()
        }
        _ => rank::brute_force_top_k_i8(&rank::quantize_i8(&q), idx.emb_matrix_i8(), pool),
    };
    // Delta distances are f32 against the base's dequantized i8 ones.
    // Quantization was verified quality-neutral, so the two scales merge.
    let delta: Vec<(u32, f32)> = rows
        .delta_vectors()
        .iter()
        .enumerate()
        .map(|(j, v)| (j as u32, rank::distance(&q, v)))
        .collect();
    let ranked = rows.merge(base, delta, pool, false);
    trace.record(if use_graph { "rank:ann" } else { "rank:brute" }, elapsed_ms(start));

    let ranked = rerank_maxsim(rows, query, ranked, opts, d, idx, trace);
    (ranked, use_graph)
}

/// MaxSim rerank over the head of the semantic list. Passes through when
/// disabled, when the list is empty, or when the query has no token vectors.
#[allow(clippy::too_many_arguments)]
fn rerank_maxsim(
    rows: &Rows,
    query: &str,
    ranked: Vec<(u32, f32)>,
    opts: &SearchOptions,
    d: &cache::Discovered,
    idx: &LoadedIndex,
    trace: &mut Trace,
) -> Vec<(u32, f32)> {
    if !opts.rerank_maxsim || ranked.is_empty() {
        return ranked;
    }
    let query_tokens = text::token_vectors(query, idx.sif.as_ref());
    if query_tokens.is_empty() {
        return ranked;
    }

    trace.time("rank:maxsim", || {
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
                        let doc = text::token_vectors(&corpus::doc_text(&path, &text), None);
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
