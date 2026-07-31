//! The search API: orchestration, and turning ranked ids into hits.
//!
//! `search` picks the path — exact, warm (an index answers), or cold (a
//! streaming pass over the corpus) — and everything below it is shared. The two
//! ranked paths live in `indexed` and `stream`; `hit` is their common tail,
//! where candidates become displayable results.

mod hit;
mod indexed;
mod rows;
mod stream;

use crate::cache::repair::RepairOutcome;
use crate::keyword::KeywordOptions;
pub use crate::rank::Mode;
use crate::trace::{
    Bucket, SCHEDULE_KEYWORD, Stage, Stages, Trace, elapsed_ms,
};
use crate::{ChunkParams, cache, keyword, store};
use anyhow::Result;
use std::path::Path;
use std::time::Instant;

/// How many candidates each ranked engine contributes to fusion.
pub(crate) const FUSION_POOL: usize = 128;

/// How wide the fused list is before candidates are selected from it.
///
/// Deliberately wider than the `k * 3` that survives: the indexed path filters
/// to the query's subtree *after* fusing, so a subdirectory query needs slack or
/// out-of-scope rows eat every slot. Both paths use it so they stay the same
/// function of their inputs — the streaming path fused straight to `k * 3`, which
/// happened to give the same top-k for a whole-root query and would not have
/// once anything filtered.
pub(crate) fn fused_width(pool: usize) -> usize {
    pool * 2
}

/// How many fused rows become candidates. Three per requested hit, so span
/// dedupe and MMR have something to choose between.
pub(crate) fn candidate_width(k: usize) -> usize {
    k * 3
}

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
    /// How many times cache discovery ran inside the engine for this query. The
    /// CLI resolves again on its own before and after, which is how one exact
    /// miss reaches three.
    pub discover_calls: u32,
    /// Files repaired-around this query (read-repair overlay), or the full
    /// stale count when `check_stale` was requested.
    pub stale_files: usize,
    pub n_chunks_considered: usize,
    /// Why the warm path did or did not repair. A duration cannot distinguish
    /// a throttled check from a clean tree from a failed walk.
    pub repair: RepairOutcome,
    /// Performance provenance: every stage on this path's schedule, in order,
    /// zero-filled where a stage did not run. Fixed shape, so two runs are
    /// comparable without special-casing which optional stages fired.
    pub stages: Stages,
    /// Wall time for the whole call, the only independently measured duration
    /// here. Everything below is derived from `stages`.
    pub total_ms: f64,
}

impl SearchReport {
    /// Corpus walk. Cold path only — zero warm, where the equivalent cost is
    /// [`SearchReport::load_ms`]. These were one overloaded field, printed as
    /// `walk/load=`, which is what a field meaning two things looks like.
    pub fn walk_ms(&self) -> f64 {
        self.stages.bucket_ms(Bucket::Walk)
    }

    /// Reading an index off disk. Warm path only.
    pub fn load_ms(&self) -> f64 {
        self.stages.bucket_ms(Bucket::Load)
    }

    /// Scoring and fusing.
    pub fn rank_ms(&self) -> f64 {
        self.stages.bucket_ms(Bucket::Rank)
    }

    /// The write-through index build this query paid for, if any.
    pub fn build_ms(&self) -> f64 {
        self.stages.bucket_ms(Bucket::Build)
    }

    pub fn accounted_ms(&self) -> f64 {
        self.stages.accounted_ms()
    }

    /// Wall time no stage claims. The honest "what we still cannot see" number:
    /// it is what every instrumentation gap showed up as, and a test bounds it
    /// so the next untimed step fails the build instead of widening it quietly.
    pub fn unattributed_ms(&self) -> f64 {
        (self.total_ms - self.accounted_ms()).max(0.0)
    }
}

pub struct SearchResult {
    pub hits: Vec<SearchHit>,
    pub report: SearchReport,
}

/// Stages measured before a path is chosen — discovery, and the write-through
/// build — replayed into whichever path ends up answering. Both schedules carry
/// these stages, so the cost of getting to a query lands in the same report as
/// the query it delayed rather than only in `total_ms`.
pub(crate) type Prelude = Vec<(Stage, f64)>;

pub fn search(root: &Path, query: &str, opts: &SearchOptions) -> Result<SearchResult> {
    let t0 = Instant::now();
    if opts.mode == Mode::Keyword {
        return keyword_search(root, query, opts, t0);
    }

    // The index is a cache (RESEARCH.md §8): resolve one for this scope —
    // local, ancestor, or central — and on a full miss, write-through: the
    // cold pass is the same work as a build, so persist it and answer warm.
    let mut prelude: Prelude = Vec::new();
    let mut wrote_cache = false;
    let mut discover_calls = 0u32;
    let mut discover_ms = 0.0;

    let discover = |discover_ms: &mut f64, discover_calls: &mut u32| {
        let t = Instant::now();
        let d = cache::discover(root, &opts.params);
        *discover_ms += elapsed_ms(t);
        *discover_calls += 1;
        d
    };

    let discovered = if opts.no_index {
        None
    } else {
        match discover(&mut discover_ms, &mut discover_calls) {
            Some(d) => Some(d),
            None => build_through(root, opts, &mut prelude)
                .then(|| {
                    wrote_cache = true;
                    discover(&mut discover_ms, &mut discover_calls)
                })
                .flatten(),
        }
    };
    prelude.push((Stage::Discover, discover_ms));

    // A cache entry is disposable: if it cannot be read for *any* reason —
    // truncated write, half-deleted directory, a format this binary predates
    // — that is a miss, not the caller's problem. Discard it and answer from
    // the streaming path, which repopulates on the way through. A repo-local
    // `.semgrep` is an explicit artifact, so its failures still propagate.
    let mut result = match discovered {
        Some(d) => match indexed::run(&d, query, opts, &prelude) {
            Ok(r) => r,
            Err(_) if d.from_cache => {
                let _ = std::fs::remove_dir_all(&d.index_dir);
                stream::run(root, query, opts, &prelude)?
            }
            Err(e) => return Err(e),
        },
        None => stream::run(root, query, opts, &prelude)?,
    };
    result.report.wrote_cache = wrote_cache;
    result.report.discover_calls = discover_calls;
    result.report.total_ms = elapsed_ms(t0);
    Ok(result)
}

/// Write-through on a miss. Returns whether an entry was written; the build's
/// own stage report is folded into `prelude` either way, because a build that
/// failed partway still spent the time.
fn build_through(root: &Path, opts: &SearchOptions, prelude: &mut Prelude) -> bool {
    let Ok(canon) = std::fs::canonicalize(root) else { return false };
    let build = store::BuildOptions { params: opts.params, ..Default::default() };
    match cache::write_cache_entry(&canon, &build, |_, _| {}) {
        Ok((_, stats)) => {
            prelude.extend(stats.stages.iter().map(|r| (r.stage, r.ms)));
            true
        }
        Err(_) => false,
    }
}

/// Exact mode. Reported nothing at all before this: `-e --stats` printed
/// `chunks=0` and no provenance line, so the one mode an agent reaches for
/// first was the one mode with no cost attribution.
fn keyword_search(
    root: &Path,
    query: &str,
    opts: &SearchOptions,
    t0: Instant,
) -> Result<SearchResult> {
    let mut trace = Trace::new(SCHEDULE_KEYWORD);
    let raw = trace.time(Stage::KeywordScan, || keyword::scan(root, query, &opts.keyword))?;
    let hits = trace.time(Stage::FinalizeMaterialize, || {
        raw.into_iter()
            .map(|h| SearchHit {
                path: h.path,
                start_line: h.line as u32,
                end_line: h.line as u32,
                line: h.line as u32,
                text: h.text,
                score: 1.0,
            })
            .collect()
    });
    Ok(SearchResult {
        hits,
        report: SearchReport {
            stages: trace.finish(),
            total_ms: elapsed_ms(t0),
            ..Default::default()
        },
    })
}

#[cfg(test)]
mod tests;
