//! The search API: orchestration, and turning ranked ids into hits.
//!
//! `search` picks the path — exact, warm (an index answers), or cold (a
//! streaming pass over the corpus) — and everything below it is shared. The two
//! ranked paths live in `indexed` and `stream`; `hit` is their common tail,
//! where candidates become displayable results.

mod hit;
mod indexed;
mod stream;

use crate::keyword::KeywordOptions;
pub use crate::rank::Mode;
use crate::{ChunkParams, cache, keyword, store};
use anyhow::Result;
use std::path::Path;
use std::time::Instant;

/// How many candidates each ranked engine contributes to fusion.
pub(crate) const FUSION_POOL: usize = 128;

#[derive(Debug, Clone)]
pub struct SearchOptions {
    pub mode: Mode,
    pub k: usize,
    /// Force the streaming path even when a .semgrep index exists.
    pub no_index: bool,
    /// Use the HNSW graph when present (else exact brute force).
    pub use_hnsw: bool,
    /// Re-walk the corpus after an indexed search to count stale files.
    /// Costs a full directory walk (~1s on 80k files), so off by default.
    pub check_stale: bool,
    /// Weight of the semantic list in hybrid RRF (BM25 is 1.0). Evals showed
    /// equal weighting lets a weak semantic list dilute BM25.
    pub sem_weight: f32,
    /// MMR diversity reranking: trade a little rank fidelity for results
    /// spread across different files/regions instead of near-duplicates.
    pub diversify: bool,
    /// MMR lambda: 1.0 = pure relevance, 0.0 = pure diversity.
    pub mmr_lambda: f32,
    /// PRF (pseudo-relevance feedback): expand the query with this many
    /// discriminative terms from the first pass's top hits, then re-rank
    /// lexically (RESEARCH.md §9.3). 0 = off.
    pub prf_terms: usize,
    /// Rerank the candidate pool by MaxSim late interaction (§9.2).
    pub rerank_maxsim: bool,
    /// MaxSim rerank head size (0 = auto: k*3, min 24).
    pub maxsim_pool: usize,
    /// Blend of MaxSim vs original embedding order within the reranked
    /// head: 1.0 = pure MaxSim (default), 0.0 = original order.
    pub maxsim_blend: f32,
    pub params: ChunkParams,
    pub keyword: KeywordOptions,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            mode: Mode::Hybrid,
            k: 10,
            no_index: false,
            use_hnsw: true,
            check_stale: false,
            sem_weight: 0.2,
            diversify: true,
            mmr_lambda: 0.75,
            prf_terms: 0,
            rerank_maxsim: false,
            maxsim_pool: 0,
            maxsim_blend: 1.0,
            params: ChunkParams::default(),
            keyword: KeywordOptions::default(),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SearchHit {
    pub path: String,
    pub start_line: u32,
    pub end_line: u32,
    /// Best-matching line within the chunk (== start_line for keyword mode).
    pub line: u32,
    pub text: String,
    pub score: f32,
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct SearchReport {
    pub used_index: bool,
    pub used_hnsw: bool,
    /// This query's cold pass was persisted as a cache entry (write-through).
    pub wrote_cache: bool,
    /// Files repaired-around this query (read-repair overlay), or the full
    /// stale count when `check_stale` was requested.
    pub stale_files: usize,
    pub n_chunks_considered: usize,
    pub walk_ms: u128,
    pub rank_ms: u128,
    pub total_ms: u128,
    /// Performance provenance: ordered (stage, ms) pairs covering the whole
    /// query so bottlenecks are attributable from a single --stats line.
    pub stages: Vec<(String, f64)>,
}

pub struct SearchResult {
    pub hits: Vec<SearchHit>,
    pub report: SearchReport,
}

pub fn search(root: &Path, query: &str, opts: &SearchOptions) -> Result<SearchResult> {
    let t0 = Instant::now();
    if opts.mode == Mode::Keyword {
        let hits = keyword::scan(root, query, &opts.keyword)?;
        let hits = hits
            .into_iter()
            .map(|h| SearchHit {
                path: h.path,
                start_line: h.line as u32,
                end_line: h.line as u32,
                line: h.line as u32,
                text: h.text,
                score: 1.0,
            })
            .collect();
        return Ok(SearchResult {
            hits,
            report: SearchReport { total_ms: t0.elapsed().as_millis(), ..Default::default() },
        });
    }

    // The index is a cache (RESEARCH.md §8): resolve one for this scope —
    // local, ancestor, or central — and on a full miss, write-through: the
    // cold pass is the same work as a build, so persist it and answer warm.
    let mut wrote_cache = false;
    let discovered = if opts.no_index {
        None
    } else {
        cache::discover(root).or_else(|| {
            let canon = std::fs::canonicalize(root).ok()?;
            let build = store::BuildOptions { params: opts.params, ..Default::default() };
            cache::write_cache_entry(&canon, &build, |_, _| {}).ok()?;
            wrote_cache = true;
            cache::discover(root)
        })
    };

    // A cache entry is disposable: if it cannot be read for *any* reason —
    // truncated write, half-deleted directory, a format this binary predates
    // — that is a miss, not the caller's problem. Discard it and answer from
    // the streaming path, which repopulates on the way through. A repo-local
    // `.semgrep` is an explicit artifact, so its failures still propagate.
    let mut result = match discovered {
        Some(d) => match indexed::run(&d, query, opts) {
            Ok(r) => r,
            Err(_) if d.from_cache => {
                let _ = std::fs::remove_dir_all(&d.index_dir);
                stream::run(root, query, opts)?
            }
            Err(e) => return Err(e),
        },
        None => stream::run(root, query, opts)?,
    };
    result.report.wrote_cache = wrote_cache;
    result.report.total_ms = t0.elapsed().as_millis();
    Ok(result)
}

#[cfg(test)]
mod tests;
