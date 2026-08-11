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
    /// Results returned. **5 since §26.3**, down from 10.
    ///
    /// Ten one-line results was the shape before passages existed. With a
    /// passage attached to each, results 6-10 carry half the payload and
    /// about a tenth of the value — §25 measured ranks 6-10 adding 10 points
    /// of coverage (71.3% → 81.4%) against rank 1's 41. Cutting to five with
    /// an 18-line passage is the only configuration measured that beats the
    /// old one-line default on **cost, turns, latency and accuracy at once**
    /// (§26.3): −12% per run, 8.01 turns against 9.17, and accuracy tied.
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
    /// span, before the lower-scoring one is dropped as a near-duplicate.
    /// 0 (the default) = any shared line at all.
    ///
    /// **Measured and not adopted** (RESEARCH.md §24.2). Chunks are strided, so
    /// every chunk overlaps its neighbours and the default rule thins a single
    /// file's results to a greedy non-overlapping subset — which really does
    /// delete answers: on the §24 `update_sources` case the chunk holding the
    /// declaration is dropped because two higher-scoring neighbours each
    /// contain a *call site*, and 0.5 brings it back at rank 3.
    ///
    /// That case is not the population. Over 2,188 real file-scoped agent
    /// queries the main effect of 0.5 is −0.003 [−0.011, +0.005] strict and
    /// **−0.009 [−0.017, −0.000] overlap** — a small significant *loss*,
    /// against a registered floor of +0.02. The single-case rescue does not
    /// generalize: keeping neighbours crowds the top-k with one file's chunks
    /// more often than it rescues the right one. Kept as a flag because it is
    /// measured, and because the §24.1 kill condition was written in advance.
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
    /// Weight of the declaration boost (RESEARCH.md §24.2). 0 = off.
    ///
    /// A chunk that *declares* the identifier a query names and a chunk that
    /// *calls* it used to score alike; §24.0 measured 85 searches where the
    /// agent named the function, the call sites came back, and no returned
    /// chunk reached the declaration. This scales each fused score by
    /// `1 + w · share of query tokens declared in that chunk`.
    ///
    /// **On by default at 0.5** — the first engine change since §20 to beat an
    /// unrendered index on real agent queries, and it does so everywhere it was
    /// measured: file scopes +0.039 strict / +0.048 overlap, directory scopes
    /// +0.017 bm25, replicated across two independent campaigns. Costs ~1.4 ms,
    /// which is the 30 candidate chunks it re-reads and is flat in corpus size
    /// (3% of a kernel query, ~9% of a small one).
    ///
    /// 0.5 rather than a larger weight by §24.3's registered parsimony rule:
    /// the effect is flat from 0.5 to 4.0 (+0.046 to +0.045, every CI excluding
    /// zero), so the boost acts as a reordering signal rather than a magnitude,
    /// and the smallest weight that buys it is the safest default — a large `w`
    /// would let one declared token dominate a fused score in a corpus that
    /// would never show it.
    pub decl_boost: f32,
    /// How many lines of each hit to show, centred on the best-matching line
    /// and clamped to the chunk. **Default 18** (RESEARCH.md §26.3).
    ///
    /// One monotone integer rather than a boolean plus a width, so every value
    /// is a real display and no caller has to combine two flags to describe
    /// one: `1` is the matched line alone (what shipped before §26), `18` is
    /// the default, and anything ≥ the chunk size is the whole passage.
    ///
    /// §25.2 measured the whole passage against a single line over 1,120 agent
    /// sessions: file-reopening fell 1.729 → 0.921 and sessions ran two turns
    /// shorter. §26.2 then measured 18 lines against the whole passage and
    /// **18 lines is worse** — it gives back +0.243 [+0.121, +0.364] of that
    /// reduction, an interval excluding zero, so the shortening is a measured
    /// loss rather than an equivalent. The coverage curve that motivated 18
    /// (94% of the coverage for 46% of the bytes) predicted behaviour and did
    /// not deliver it, which is §25's own lesson a second time.
    ///
    /// §26.3 then changed the question. Scored on **cost at constant accuracy**
    /// rather than on file-reopening, 18 lines at `k=5` is the cheapest thing
    /// measured: −16% against the whole passage [−0.060, −0.015] and −12%
    /// against the pre-§26 single line, with accuracy tied in every contrast.
    /// So the default is 18 again, for a different reason than it was 18 the
    /// first time — **it is worse at the endpoint §26.1 registered and better
    /// at the one the tool is actually for.** Both are true and the second is
    /// the one being optimised.
    ///
    /// That reversal was an endpoint switch made after seeing the data, which
    /// is what pre-registration exists to prevent. It is recorded as such in
    /// §26.3 rather than presented as the plan all along, and the cost claim
    /// behind it is one campaign on an endpoint that has already failed to
    /// replicate once (§25's +18% became §26's +5%).
    ///
    /// **0 defers to [`passage_chars`], which is the shipped mechanism.** A
    /// line budget survives only so §26's arms reproduce under their own flag.
    pub passage_lines: u32,
    /// Characters of each hit to show, grown line by line around the match
    /// until the next line would exceed the budget. **Default 800**
    /// (RESEARCH.md §26.4). 0 shows the matched line alone.
    ///
    /// A line is not a unit of content, and budgeting by lines prices prose
    /// and code differently for the same nominal window. Measured at 18 lines
    /// per hit, k=5, with the per-line cap active: the kernel spends 2,761
    /// bytes a search, vscode 4,165 and Wikipedia **10,048** — a 3.6× spread
    /// for output that is nominally identical. At 600 characters the same
    /// three spend 5,492 / 8,413 / **2,321** — prose falls by 83% and the
    /// worst corpus by 38%, because prose gets ~4 lines where C gets ~20 and
    /// both get the same amount to *read*.
    ///
    /// It does not equalise the three, and the first attempt at this assumed
    /// it would. Roughly half of printed output is the per-line `path:line:`
    /// prefix, which scales with line count rather than content, so a content
    /// budget hands short-line C more lines and more overhead. Charging
    /// [`LINE_OVERHEAD`] recovers part of that and the path part is not
    /// knowable here. The goal this serves is a **bounded worst case**, which
    /// it delivers; a flat cost across languages it does not.
    ///
    /// 800 because it is the **equivalence point**, not because it is the
    /// cheapest: over 109 real agent searches at k=5 it scores 51.4% with
    /// 2,880 bytes a search against 18 lines' 51.4% with 2,853 — the same
    /// behaviour, to the search. 600 costs 2,140 and scores 48.6%, three
    /// searches fewer on 109, which is noise and might well be free. It is
    /// not taken, because changing the *unit* and the *effective size*
    /// together would leave the next campaign unable to say which one moved.
    /// Re-tuning the size is a separate question with its own answer.
    ///
    /// The same unit as [`ChunkParams::budget`], for the same reason (§20.2).
    pub passage_chars: u32,
    /// Carry the names each hit's chunk declares (RESEARCH.md §25.1). Display
    /// only. The cheaper half of the same idea — name what is in the window
    /// rather than printing all of it: 314 bytes against 12,079, reaching 88%
    /// of the same gap.
    pub defines: bool,
    /// Second-stage rerank: score every [`fine_lines`](Self::fine_lines)-line
    /// sub-window of each candidate chunk against the query and let the best
    /// window's score order the final list, with the window itself becoming
    /// the hit's span and passage (RESEARCH.md §28.2).
    ///
    /// The §28 head-to-head located sg's deficit in the *last inch*: agents
    /// anchor the line range they act on to whatever span the tool displays,
    /// and a ~32-line chunk window routinely ends lines away from the target
    /// (27% of sg's losses, 2.3× ripgrep's rate). This trades the chunk-sized
    /// answer for the few lines inside it that actually match.
    ///
    /// Scoring is cosine of the sub-window's embedding against the query's,
    /// through i8 quantization on both sides — deliberately in its *own*
    /// space (raw text, no path line, no SIF, no prose render) rather than
    /// the index's, so a cold and a warm search compute identical fine
    /// scores from the file text alone and the parity invariant holds with
    /// no index state involved.
    pub fine_rerank: bool,
    /// Sub-window height for the fine rerank, in lines. 4 by default: the
    /// median gold region agents hunt is a handful of lines, and a window
    /// this size shows the matched construct with one line of context on
    /// each side without re-importing the dilution the rerank exists to fix.
    pub fine_lines: u32,
    /// Blend of fine-window score vs coarse chunk score when ordering the
    /// final list: 1.0 = pure fine (default), 0.0 = coarse order with fine
    /// windows only choosing each hit's display span. Both min-max
    /// normalized within the candidate pool before blending, since the two
    /// live on incomparable scales (a fused RRF score is ~1e-2).
    pub fine_blend: f32,
    /// The caller asked for a specific passage shape (`--passage-chars`,
    /// `--passage-lines`, or `--full`), so the fine window still picks each
    /// hit's anchor and rank but the *displayed* cut follows the request.
    /// False by default: the fine window is the passage.
    pub passage_override: bool,
    /// Refuse to answer below this score: when the *best* candidate's signal
    /// falls under the floor, the search returns zero hits, exit 1, and the
    /// footer says why (RESEARCH.md §28.2). 0 = off, which is the default
    /// until the floor is calibrated on replayed real agent queries.
    ///
    /// The §28 sessions showed why silence can beat an answer: sg returned
    /// content on 99% of calls, a plausible-looking chunk near (but not at)
    /// the target reads as an answer, and agents submitted non-gold files sg
    /// itself had displayed at 2× ripgrep's rate — while 17% of rg calls
    /// failing loudly is exactly what prompted agents to rephrase. This is
    /// that "colder, try again" signal for ranked search.
    ///
    /// Set-level, not per-hit: the floor answers "does this scope contain
    /// the concept at all". A weak tail behind a strong head is normal
    /// ranked output, and dropping hits one by one would silently shrink k.
    ///
    /// The signal is the fine-window cosine ([-1, 1], cross-query
    /// comparable). The fused score cannot serve: under the default maxsim
    /// head normalization the top fused score is a constant, and under RRF
    /// it is a pure function of rank — neither says anything about match
    /// quality. With `fine_rerank` off the floor falls back to the best
    /// chunk-embedding cosine via the same vectors MMR diversifies with.
    pub min_score: f32,
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
            k: 5,
            no_index: false,
            use_hnsw: true,
            check_stale: false,
            sem_weight: 0.2,
            diversify: true,
            mmr_lambda: 0.75,
            dedupe_overlap: 0.0,
            file_scope_window: 0,
            decl_boost: 0.5,
            passage_lines: 0,
            passage_chars: 800,
            defines: false,
            fine_rerank: true,
            fine_lines: 4,
            fine_blend: 1.0,
            passage_override: false,
            min_score: 0.0,
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
                chunk_start_line: None,
                chunk_end_line: None,
                // Exact mode has no chunk — its "span" is the matched line
                // itself — so there is no passage to cut and nothing a header
                // could say that the line does not already.
                lines: None,
                lines_from: None,
                defines: None,
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
