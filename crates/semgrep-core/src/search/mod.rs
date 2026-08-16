//! The search API: orchestration, and turning ranked ids into hits.
//!
//! `search` picks the path — exact, warm (an index answers), or cold (a
//! streaming pass over the corpus) — and everything below it is shared. The two
//! ranked paths live in `indexed` and `stream`; `hit` is their common tail,
//! where candidates become displayable results.

mod checklist;
mod hit;
mod indexed;
mod materialize;
mod options;
mod query;
mod rerank;
mod rows;
mod stream;
mod unit;

pub use options::SearchOptions;
pub use query::{MAX_PHRASES, split_phrases};
pub(crate) use query::{Query, merge_interleave};

use crate::cache::repair::RepairOutcome;
pub use crate::rank::Mode;
use crate::trace::{
    Bucket, SCHEDULE_KEYWORD, Stage, Stages, Trace, elapsed_ms,
};
use crate::{cache, keyword, store, text};
use anyhow::Result;
use std::path::Path;
use std::time::Instant;

/// `passage_lines` meaning "every line of the chunk", which is the default.
///
/// A sentinel rather than the chunk size, because the chunk size is itself a
/// parameter (`--window`) and a default that had to track it would be a second
/// place for the two to disagree.
pub const WHOLE_PASSAGE: u32 = u32::MAX;

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

/// How many fused rows become candidates. **Six per requested hit** — 30 at
/// the default k=5 — so span dedupe, the fine rerank and MMR have something to
/// choose between.
///
/// Was three per hit, which was sized for a stage that only *reordered* whole
/// chunks. The fine rerank (§29.1) changed what the pool is for: it re-scores
/// each candidate down to its best few lines, so a chunk whose coarse rank is
/// mediocre because 28 of its 32 lines are irrelevant is exactly the chunk the
/// fine pass exists to rescue — and at `k * 3` it never reached the pass. A
/// wider pool is where that rescue has room to happen.
///
/// The cost is per-candidate file reads, paid twice: the declaration boost
/// re-reads this same head (they share this width deliberately, so the boost
/// acts on exactly the rows that become candidates), and the fine rerank reads
/// them again. Measured below in `SearchOptions::fine_rerank`'s doc.
pub(crate) fn candidate_width(k: usize) -> usize {
    k * 6
}

/// Does this search need the lexical channel at all? True in the lexical
/// modes, and in semantic mode when `bm25_pin` demands BM25's opinion for
/// the display guarantee (§32.4a). Both paths gate their BM25 work on this,
/// which is what keeps cold == warm when the pin is on.
pub(crate) fn wants_lexical(opts: &SearchOptions) -> bool {
    matches!(opts.mode, Mode::Bm25 | Mode::Hybrid)
        || ((opts.bm25_pin > 0 || opts.bridge_expand > 0)
            && !matches!(opts.mode, Mode::Keyword))
}

/// Append the lexical head's ids to a fused/semantic ranking so the
/// `bm25_pin` guarantee has candidates to pin: ids already ranked stay
/// where they are, missing ones join the tail below the current minimum, in
/// lexical order. Called at the same point on both paths (after the
/// declaration boost, before candidate materialization).
pub(crate) fn append_bm25_pins(
    mut ranked: Vec<(u32, f32)>,
    bm25_head: &[u32],
    opts: &SearchOptions,
) -> Vec<(u32, f32)> {
    if opts.bm25_pin == 0 || bm25_head.is_empty() {
        return ranked;
    }
    let floor = ranked.last().map_or(0.0, |r| r.1);
    let present: std::collections::HashSet<u32> = ranked.iter().map(|r| r.0).collect();
    for (i, id) in bm25_head.iter().take(opts.bm25_pin).enumerate() {
        if !present.contains(id) {
            ranked.push((*id, floor - 0.001 * (i as f32 + 1.0)));
        }
    }
    ranked
}

/// Structural boost (RESEARCH.md §24.1 declarations, §35.1 paths): scale each
/// fused score by `(1 + w_decl · decl_share) · (1 + w_path · path_share)`,
/// where `decl_share` is the fraction of query tokens declared in the chunk
/// and `path_share` the fraction appearing in the path's tail (last two
/// segments, tokenized as BM25 tokenizes them).
///
/// One implementation, called from both paths at the same point, for the reason
/// `rerank_maxsim` is: a scope that happens to be indexed must not answer a
/// query differently from one that is not
/// (`cold_and_warm_return_identical_results`).
///
/// Multiplicative because it has to work in three score spaces at once — raw
/// BM25, cosine, and RRF, whose fused scores are ~1e-3. An additive boost sized
/// for one of them swamps or vanishes in the others. The two terms compose
/// multiplicatively too, so either weight at 0 is exactly a no-op for its term.
///
/// `source_of` returns the chunk's path and body; a chunk whose text cannot be
/// read scores its declaration share as 0 rather than being dropped, since a
/// missing file is already `materialize`'s problem and dropping here would
/// silently change the pool.
///
/// Returns `(id, decl_share, path_share)` for the boosted head, in pre-boost
/// order: the learned checklist consumes the shares as features.
pub(crate) fn apply_structural_boost(
    ranked: &mut [(u32, f32)],
    query: &str,
    opts: &SearchOptions,
    source_of: impl Fn(u32) -> (String, Option<String>) + Sync,
) -> Vec<(u32, f32, f32)> {
    if (opts.decl_boost <= 0.0 && opts.path_boost <= 0.0) || ranked.is_empty() {
        return Vec::new();
    }
    let qtokens: std::collections::HashSet<String> =
        text::token::tokens(query).into_iter().collect();
    if qtokens.is_empty() {
        return Vec::new();
    }
    use rayon::prelude::*;
    let head = candidate_width(opts.k).min(ranked.len());
    let shares: Vec<(u32, f32, f32)> = ranked[..head]
        .par_iter()
        .map(|&(id, _)| {
            let (path, text) = source_of(id);
            let decl_share = match text {
                Some(t) if opts.decl_boost > 0.0 => {
                    let decl = text::declaration_tokens(&t);
                    if decl.is_empty() {
                        0.0
                    } else {
                        let n = qtokens.iter().filter(|q| decl.contains(*q)).count();
                        n as f32 / qtokens.len() as f32
                    }
                }
                _ => 0.0,
            };
            let path_share = {
                let tail = text::prose::tail_segments(&path, 2);
                let ptoks: std::collections::HashSet<String> =
                    text::token::tokens(tail).into_iter().collect();
                if ptoks.is_empty() {
                    0.0
                } else {
                    let n = qtokens.iter().filter(|q| ptoks.contains(*q)).count();
                    n as f32 / qtokens.len() as f32
                }
            };
            (id, decl_share, path_share)
        })
        .collect();
    for (slot, &(_, decl_share, path_share)) in ranked[..head].iter_mut().zip(&shares) {
        slot.1 *= (1.0 + opts.decl_boost.max(0.0) * decl_share)
            * (1.0 + opts.path_boost.max(0.0) * path_share);
    }
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
    shares
}

/// Same, for a scope that is one file: everything (RESEARCH.md §24.1).
///
/// [`candidate_width`] is a corpus-scale economy — it bounds how many chunks
/// pay for a vector and a dedupe comparison when the pool is millions. A
/// single file yields a median 56 chunks, so the cap can drop the chunk
/// holding the answer before dedupe or MMR ever sees it. There is nothing to
/// save.
pub(crate) fn file_scope_candidate_width() -> usize {
    usize::MAX
}


/// One row of the unit view: a real file line, raw — undedented and
/// unclipped, because dedent is a property of the displayed *block* and
/// width is the renderer's concern ([`out`]'s, in the CLI). Plain data with
/// no map keys, so [`SearchHit`]'s "cannot fail to serialize" contract
/// stands.
#[derive(Debug, Clone, serde::Serialize)]
pub struct UnitRow {
    pub line: u32,
    pub text: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SearchHit {
    pub path: String,
    /// Span of the hit as displayed: the fine sub-window when the fine rerank
    /// chose one, else the whole chunk. The *displayed* span is deliberately
    /// the primary one — agents anchor the line ranges they act on to what
    /// the tool prints (RESEARCH.md §28.2), so the tight span must be the
    /// prominent span and the chunk becomes the context field, not the other
    /// way around.
    pub start_line: u32,
    pub end_line: u32,
    /// Best-matching line within the span (== start_line for keyword mode).
    pub line: u32,
    pub text: String,
    pub score: f32,
    /// Bounds of the underlying chunk, when the fine rerank narrowed
    /// `start_line`/`end_line` below it. Absent otherwise, so the JSON
    /// contract is unchanged for consumers of the unfined shape.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunk_start_line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunk_end_line: Option<u32>,
    /// Which phrase of a multi-phrase query this hit answers (RESEARCH.md
    /// §31), 0-based into `SearchReport::phrases`. `None` for single-phrase
    /// queries — not `Some(0)` — so their JSON is byte-identical to before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phrase: Option<u32>,
    /// The passage shown for this hit, when the caller asked for more than one
    /// line ([`SearchOptions::passage_lines`], RESEARCH.md §26).
    ///
    /// `None` rather than an empty vec when off, and `skip_serializing_if` so
    /// the JSON contract is unchanged for every existing consumer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines: Option<Vec<String>>,
    /// File line number of `lines[0]`, and `None` exactly when `lines` is.
    ///
    /// Equal to `start_line` only when the passage happens to begin at the
    /// chunk boundary, which is why it exists: numbering a cut passage from
    /// `start_line` misnumbers every line of it. Skipped in JSON when absent,
    /// so a consumer that asked for no passage sees the schema it always saw.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines_from: Option<u32>,
    /// Names the chunk declares, when the caller asked for them
    /// ([`SearchOptions::defines`]). Same absent-by-default contract.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub defines: Option<Vec<String>>,
    /// The unit-view rows for this hit (RESEARCH.md §34): the snapped fine
    /// window plus the rows that de-orphan it (enclosing declaration, doc
    /// line, contiguous close), in file order with gaps ≤ 3 already
    /// filled. A jump between consecutive rows' line numbers is an elision
    /// the renderer marks. `None` when the unit view is off or the caller
    /// asked for an explicit passage shape, and skipped in JSON then —
    /// same absent-by-default contract as every optional field above.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit_rows: Option<Vec<UnitRow>>,
    /// Per-candidate ranking features (RESEARCH.md §35.2), present only when
    /// [`SearchOptions::debug_features`] asked for them — the training dump
    /// for the learned checklist rides the JSON output rather than a second
    /// format. Absent in every ordinary run, so the JSON contract is
    /// unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub features: Option<HitFeatures>,
}

/// The candidate-local facts the learned checklist scores (RESEARCH.md §35.2),
/// as they stood when the hit was materialized. Candidate-local on purpose:
/// anything query-global (query length, mode) is already recorded by whoever
/// asked for the dump, and anything index-global would break cold==warm.
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct HitFeatures {
    /// The fused (coarse) score, pre-fine.
    pub coarse: f32,
    /// Fine-window cosine, when the fine rerank scored this candidate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fine: Option<f32>,
    /// 1-based rank in the lexical head, when the BM25 channel ranked it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bm25_rank: Option<u16>,
    /// Retriever bitmask over the query's phrases.
    pub phrases: u8,
    pub decl_share: f32,
    pub path_share: f32,
    /// Chunk height in lines.
    pub chunk_lines: u32,
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
    /// Zero hits because nothing cleared [`SearchOptions::min_score`], as
    /// opposed to an empty or unreadable scope. The footer branches on this,
    /// and telemetry needs it to tell a floored refusal from a miss.
    pub floored: bool,
    /// The best candidate's floor signal, when a floor was set. Reported even
    /// on success so a calibration campaign can join score to outcome without
    /// re-running anything.
    pub best_signal: Option<f32>,
    /// How many phrases the query split into (RESEARCH.md §31). 1 for every
    /// query without a pipe. Lives here and not in the options envelope
    /// because the split happens inside the engine — the CLI never knows it.
    pub n_phrases: usize,
    /// Per-phrase floor signals, `Some` only for a multi-phrase query with a
    /// floor set; index-aligned with [`SearchReport::phrases`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phrase_signals: Option<Vec<f32>>,
    /// The phrase strings, `Some` only when the query split — the footer's
    /// per-phrase verdicts read from here rather than re-running the split.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phrases: Option<Vec<String>>,
    /// Which phrases the floor refused (bit i = phrase i). The engine's
    /// decision, exported — the footer cannot re-derive it without the
    /// threshold, and re-deriving decisions is how displays drift from
    /// engines.
    pub floored_mask: u8,
    /// Bridge-expansion terms actually applied (§33), `Some` only when
    /// `bridge_expand > 0` and mining produced any — the eval harness reads
    /// the fired-rate from here. Engine-derived, so it lives in the report,
    /// not the options envelope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bridge_terms: Option<Vec<String>>,
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
    // The phrase split lives here — after the keyword return, so `-e` keeps
    // regex `|`, and before path selection, so a cached scope and an uncached
    // one parse one query the same way (RESEARCH.md §31).
    let query = Query::parse(query);
    let query = &query;

    // A file scope is a different search, and cheap enough to do better
    // (RESEARCH.md §24.1). 55% of real agent searches name a single file, the
    // whole search is ~45 ms over a few dozen chunks, and `cache::discover`
    // bails on a non-directory root — so this can neither cost anything at
    // corpus scale nor key a cache entry, and there is no warm file scope for
    // `cold_and_warm_return_identical_results` to disagree with.
    // Not under function chunking: the 12-line-median-gold dilution this
    // override patches (§24.1) is exactly the defect definition-boundary
    // chunks remove at the source, and `with_window` would clear `function`
    // and silently re-window the one mode that does not need it.
    let file_opts;
    let opts = if opts.file_scope_window > 0 && root.is_file() && opts.params.function.is_none() {
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
                chunk_start_line: None,
                chunk_end_line: None,
                phrase: None,
                // Exact mode has no chunk — its "span" is the matched line
                // itself — so there is no passage to cut and nothing a header
                // could say that the line does not already.
                lines: None,
                lines_from: None,
                defines: None,
                unit_rows: None,
                features: None,
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
