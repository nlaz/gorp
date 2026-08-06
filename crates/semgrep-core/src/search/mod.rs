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
use crate::{ChunkParams, cache, keyword, store, text};
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

/// Declaration boost (RESEARCH.md §24.1): scale each fused score by
/// `1 + w · (share of query tokens declared in that chunk)`.
///
/// One implementation, called from both paths at the same point, for the reason
/// `rerank_maxsim` is: a scope that happens to be indexed must not answer a
/// query differently from one that is not
/// (`cold_and_warm_return_identical_results`).
///
/// Multiplicative because it has to work in three score spaces at once — raw
/// BM25, cosine, and RRF, whose fused scores are ~1e-3. An additive boost sized
/// for one of them swamps or vanishes in the others.
///
/// `text_of` returns the chunk body; a chunk whose text cannot be read scores
/// its share as 0 rather than being dropped, since a missing file is already
/// `materialize`'s problem and dropping here would silently change the pool.
pub(crate) fn apply_decl_boost(
    ranked: &mut [(u32, f32)],
    query: &str,
    opts: &SearchOptions,
    text_of: impl Fn(u32) -> Option<String> + Sync,
) {
    if opts.decl_boost <= 0.0 || ranked.is_empty() {
        return;
    }
    let qtokens: std::collections::HashSet<String> =
        text::token::tokens(query).into_iter().collect();
    if qtokens.is_empty() {
        return;
    }
    use rayon::prelude::*;
    let head = candidate_width(opts.k).min(ranked.len());
    let shares: Vec<f32> = ranked[..head]
        .par_iter()
        .map(|&(id, _)| {
            let Some(t) = text_of(id) else { return 0.0 };
            let decl = text::declaration_tokens(&t);
            if decl.is_empty() {
                return 0.0;
            }
            let n = qtokens.iter().filter(|q| decl.contains(*q)).count();
            n as f32 / qtokens.len() as f32
        })
        .collect();
    for (slot, share) in ranked[..head].iter_mut().zip(shares) {
        slot.1 *= 1.0 + opts.decl_boost * share;
    }
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
}

/// Same, for a scope that is one file: everything (RESEARCH.md §24.1).
///
/// `k * 3` is a corpus-scale economy — it bounds how many chunks pay for a
/// vector and a dedupe comparison when the pool is millions. A single file
/// yields a median 56 chunks, so at k=10 the cap can drop the chunk holding
/// the answer before dedupe or MMR ever sees it. There is nothing to save.
pub(crate) fn file_scope_candidate_width() -> usize {
    usize::MAX
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
    /// How much two same-file chunks must overlap, as a share of the shorter
    /// span, before the lower-scoring one is dropped as a near-duplicate
    /// (RESEARCH.md §24.1). 0 = the pre-§24 rule: any shared line at all.
    ///
    /// Chunks are strided, so every chunk overlaps its neighbours and the old
    /// rule thinned a single file's results to a greedy non-overlapping subset
    /// — deleting the chunk that held the answer whenever a neighbour holding a
    /// *call site* outscored it.
    pub dedupe_overlap: f32,
    /// Chunk window to use when the scope *is* one file, in lines. 0 = off,
    /// use [`params`](Self::params) as given (RESEARCH.md §24.1).
    ///
    /// The 32-line window is sized for corpus-scale indexing; the median gold
    /// function agents hunt is 12 lines, so a chunk routinely pools the target
    /// with several of its neighbours. A file scope can afford better — it
    /// never resolves an index, so this can never key a cache entry, and the
    /// whole search is ~45 ms over a few dozen chunks. Also lifts the
    /// [`candidate_width`] cap, which at k=10 admits only 30 chunks and can
    /// exclude the answer on a file that yields more.
    pub file_scope_window: u32,
    /// Weight of the declaration boost (RESEARCH.md §24.1). 0 = off.
    ///
    /// A chunk that *declares* the identifier a query names and a chunk that
    /// *calls* it currently score alike; §24.0 measured 85 searches where the
    /// agent named the function, the call sites came back, and no returned
    /// chunk reached the declaration. This scales each fused score by
    /// `1 + w · share of query tokens declared in that chunk`.
    pub decl_boost: f32,
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
    /// Rerank AFTER RRF instead of before it, so MaxSim reorders the fused
    /// list rather than only the semantic branch (§13.11). Experimental:
    /// §9.4 rejected post-fusion reranking, but did so at blend 1.0 (pure
    /// override) and with the NaN bug of FIXES.md #9 still live.
    pub maxsim_post: bool,
    pub params: ChunkParams,
    /// Share of a scope that may drift before a cache entry is rebuilt rather
    /// than repaired. 0 disables the bound and repairs any amount of drift,
    /// which is what the engine did before it had one
    /// ([`cache::repair::DEFAULT_MAX_DRIFT`]).
    pub repair_max_drift: f32,
    /// Called once, before a write-through build begins, when this query is the
    /// first ranked search of its scope and is therefore about to pay for an
    /// index. The engine owns this because the engine is what resolves the
    /// index: a caller that wants to print "caching this scope" otherwise has
    /// to re-derive the answer with its own `cache::discover`, which is a
    /// second canonicalization and generation scan per query (SIMULATION.md
    /// §4). A plain `fn` rather than a boxed closure so `SearchOptions` stays
    /// `Clone` and `Debug`.
    pub on_first_search: Option<fn()>,
    pub keyword: KeywordOptions,
    /// Prose-render text before embedding (RESEARCH.md §14.2). Drives the cold
    /// path and the write-through build; the warm path takes the index's own
    /// `meta.embed_preproc` instead — stored vectors dictate the space.
    pub embed_preproc: text::EmbedPreproc,
    /// How the path line of `doc_text` is rendered (RESEARCH.md §20). Read from
    /// `meta.path_render` on the warm path, for the same reason.
    pub path_render: text::PathRender,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            // Semantic-first: RESEARCH.md §14. Hybrid stays available as an
            // explicit mode; it returns as default when semantic carries it.
            mode: Mode::Semantic,
            k: 10,
            no_index: false,
            use_hnsw: true,
            check_stale: false,
            sem_weight: 0.2,
            diversify: true,
            mmr_lambda: 0.75,
            dedupe_overlap: 0.5,
            file_scope_window: 0,
            decl_boost: 0.0,
            prf_terms: 0,
            rerank_maxsim: false,
            maxsim_pool: 0,
            maxsim_blend: 1.0,
            maxsim_post: false,
            params: ChunkParams::default(),
            repair_max_drift: cache::repair::DEFAULT_MAX_DRIFT,
            on_first_search: None,
            keyword: KeywordOptions::default(),
            embed_preproc: text::EmbedPreproc::None,
            path_render: text::PathRender::Full,
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
    /// Files in this query's scope: walked on the cold path, the index's file
    /// table warm. Paired with `n_chunks_considered` it separates "this scope
    /// is empty" from "this scope has files and none of them could be read" —
    /// the second being the signature of the §16.11 file-scope bug, which
    /// reported an ordinary miss for the whole time it existed. A ranked search
    /// over a readable scope cannot return zero, so a zero needs an explanation
    /// that "rephrase the query" does not give.
    pub files_walked: usize,
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

/// A cache entry so far out of date that patching it costs more than replacing
/// it. Raised by the warm path and answered by [`search`] with a rebuild.
///
/// A distinct type rather than a message because the arm that catches it has to
/// be told apart from the "this entry is unreadable" arm sitting right next to
/// it: both discard the entry, but one streams and the other rebuilds, and
/// matching on a string would make that distinction a typo away from wrong.
#[derive(Debug, Clone, Copy)]
pub struct DriftTooLarge {
    pub dirty: usize,
    pub total: usize,
}

impl std::fmt::Display for DriftTooLarge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} of {} files drifted; rebuilding is cheaper", self.dirty, self.total)
    }
}

impl std::error::Error for DriftTooLarge {}

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

    // A file scope is a different search, and cheap enough to do better
    // (RESEARCH.md §24.1). 55% of real agent searches name a single file, the
    // whole search is ~45 ms over a few dozen chunks, and `cache::discover`
    // bails on a non-directory root — so this can neither cost anything at
    // corpus scale nor key a cache entry, and there is no warm file scope for
    // `cold_and_warm_return_identical_results` to disagree with.
    let file_opts;
    let opts = if opts.file_scope_window > 0 && root.is_file() {
        file_opts = SearchOptions {
            params: opts.params.with_window(opts.file_scope_window),
            ..opts.clone()
        };
        &file_opts
    } else {
        opts
    };

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
            // Too stale to patch. Replace the entry rather than stream around
            // it: streaming answers this one query and keeps nothing, while a
            // rebuild makes every query after it warm again. That is the whole
            // case for the threshold — at 5% drift a rebuild pays for itself in
            // about five queries, and repairing charges full price forever
            // (SIMULATION.md §1.3).
            Err(e) if e.is::<DriftTooLarge>() => {
                let why = *e.downcast_ref::<DriftTooLarge>().expect("just matched");
                let mut rebuilt = None;
                if !opts.no_index && build_through(root, opts, &mut prelude) {
                    wrote_cache = true;
                    if let Some(fresh) = discover(&mut discover_ms, &mut discover_calls) {
                        // The bound is off for the retry, which is what makes
                        // "rebuild once" true rather than hopeful. A freshly
                        // built entry normally has no drift at all — but a scope
                        // the root walk excludes does not gain rows by rebuilding
                        // the root (a hidden directory is the case that found
                        // this), and re-raising here would cost a build *and* a
                        // stream on every single query. Patch whatever is left.
                        let patch = SearchOptions { repair_max_drift: 0.0, ..opts.clone() };
                        rebuilt = indexed::run(&fresh, query, &patch, &prelude).ok();
                    }
                }
                match rebuilt {
                    Some(mut r) => {
                        // The rebuilt entry reports `no_drift`, which is true of
                        // it and useless as an explanation: it would describe
                        // this query — a 170 ms one on tokio — exactly as it
                        // describes an 9 ms warm hit. What happened here is that
                        // the entry was too stale to patch, so say that, and let
                        // `wrote_cache` say what was done about it.
                        r.report.repair = cache::repair::RepairOutcome::DriftTooLarge {
                            dirty: why.dirty,
                            total: why.total,
                        };
                        r
                    }
                    // The rebuild failed or produced something unreadable. Fall
                    // through rather than retry: a query must still be answered.
                    None => stream::run(root, query, opts, &prelude)?,
                }
            }
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
    // Only build what discovery could serve back. `cache::discover` refuses a
    // non-directory root, so an entry keyed at a file has no possible reader:
    // every file-scoped search built a complete index, wrote it, failed to
    // re-discover it, and streamed anyway — then the budget sweep deleted the
    // entry it had just written, because it judges a root dead by `is_dir`.
    // `--stats` reported that round trip as `built_but_missed`, a shape the
    // trace names precisely because it is a bug. Agents scope to a file
    // constantly (47% of searches in the §16.10 campaign), so this ran on
    // roughly half of them, and the work had nowhere to go.
    //
    // Serving a file scope from an ancestor's index is the better answer and
    // the prefix machinery already exists; this is the guard that stops paying
    // for it twice in the meantime.
    if !canon.is_dir() {
        return false;
    }
    // Here and not at the call site: this is the first point at which a build is
    // certain, so the notice cannot fire for a scope that turns out to be
    // unresolvable. Keyword mode returns before any of this and `no_index` never
    // reaches it, which is the same pair of exemptions the CLI used to apply for
    // itself with a second `cache::discover`.
    if let Some(notify) = opts.on_first_search {
        notify();
    }
    let build = store::BuildOptions {
        params: opts.params,
        embed_preproc: opts.embed_preproc,
        path_render: opts.path_render,
        ..Default::default()
    };
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
