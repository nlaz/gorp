//! [`SearchOptions`]: every knob the ranked paths read, and its default.
//!
//! Its own module because it is the widest type in the crate — the CLI
//! mirrors it flag for flag — and because a knob's documentation is the
//! only place its measured effect is recorded. `search` reads it; nothing
//! here computes.

use super::Mode;
use crate::keyword::KeywordOptions;
use crate::{ChunkParams, cache, text};

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
    /// Force the streaming path even when a .gorp index exists.
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
    /// [`candidate_width`] cap, which can otherwise exclude the answer on a
    /// file that yields more chunks than the cap admits.
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
    /// Weight of the path term of the structural boost: scores in the boosted
    /// head are scaled by `1 + path_boost · path_share`, where `path_share` is
    /// the fraction of query tokens found in the path's last two segments
    /// (tokenized as BM25 tokenizes them). **Default 0.0 — off**, pending the
    /// §35.1 gate.
    ///
    /// Path tokens already reach both retrieval channels as content via
    /// `path_render: Full`, so this flag measures the *increment* of an
    /// explicit rank-time boost over path-as-content. Same head, same file
    /// reads, and the same multiplicative form as `decl_boost` — the two terms
    /// compose, and either at 0 is exactly a no-op for its term.
    pub path_boost: f32,
    /// Emit [`HitFeatures`] on every hit (RESEARCH.md §35.2). Not a CLI flag —
    /// the CLI sets it from `GORP_DUMP_FEATURES=1`, keeping the flag
    /// surface clean; the only consumer is the checklist training dump.
    pub debug_features: bool,
    /// The learned checklist's share of the final relevance (RESEARCH.md
    /// §35.2): `relevance = (1-b)·base + b·learned`, both sides in [0, 1].
    /// **Default 0.5 since §35.6** — the full-corpus gate on replayed real
    /// agent queries: rank_func +0.012 [+0.005, +0.020] on directory scopes
    /// and +0.010 [+0.002, +0.020] on file scopes, both function metrics
    /// moving together, file rank +0.025, and the bm25 tripwire *improved*
    /// (+0.010) rather than merely not regressing.
    ///
    /// 0.5 and not the 1.0 arm, although 1.0 measured larger on directory
    /// scopes (+0.019): the dose is monotone, but 1.0's file-scope CIs touch
    /// zero, it carries 3× the discordance, and half the physical
    /// (fine-cosine) signal is kept as the out-of-distribution hedge — the
    /// weights were trained on nine mostly-Python repos. Raising the dose is
    /// a registered follow-up gated on an off-distribution floor, not a
    /// tuning knob.
    pub learned_blend: f32,
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
    /// Ship each ranked hit's unit-view rows ([`SearchHit::unit_rows`],
    /// RESEARCH.md §34): the fine window snapped off bare closers/openers
    /// and framed by its enclosing declaration, computed by
    /// `search::unit`. On by default — this is the shipped display — and
    /// yielding to [`passage_override`](Self::passage_override): a caller
    /// who asked for a passage shape gets exactly that shape, which is
    /// also what keeps the §26 arms and the snapshot's `--passage-lines 1`
    /// pin byte-identical. `--no-unit` restores the bare fine-window
    /// passage as the A/B control.
    pub unit_view: bool,
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
    /// Never let a later stage evict the pre-fine top candidate from the
    /// display: it may be outranked, not dropped. §32.4a measured the fine
    /// rerank demoting a coarse rank-1 clean out of the top-k. Off by
    /// default until the offline gates place it.
    pub keep_coarse_top: bool,
    /// Run the lexical (BM25) channel even in semantic mode and guarantee
    /// its top-N chunks a display slot each (0 = off). §32.4a: the shipped
    /// semantic-only mode never consults BM25, and real agent misses sat in
    /// BM25's top five on identifier queries. Costs a lexical query per
    /// search when set. Pinned hits fill from the tail and never evict each
    /// other or the `keep_coarse_top` pin; the floor still wins.
    pub bm25_pin: usize,
    /// Bridge-file query expansion (§33): mine up to this many terms from
    /// the files that best cover the query's tokens and add them to the
    /// lexical scoring at [`Self::bridge_weight`]. 0 = off. Runs the lexical
    /// channel even in semantic mode (like `bm25_pin`); the semantic query
    /// embedding, fine rerank, floor and best-line anchor all keep the
    /// original phrases.
    pub bridge_expand: usize,
    /// Weight of a bridge expansion term relative to an original query
    /// token's 1.0. The prototype's full-weight concatenation demoted
    /// ordering-class regions out of the top-30 (§33 P1: −13); reduced
    /// weight is the fix under test.
    pub bridge_weight: f32,
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
            path_boost: 0.0,
            debug_features: false,
            learned_blend: 0.5,
            passage_lines: 0,
            passage_chars: 800,
            defines: false,
            fine_rerank: true,
            fine_lines: 4,
            fine_blend: 1.0,
            passage_override: false,
            unit_view: true,
            min_score: 0.0,
            keep_coarse_top: false,
            bridge_expand: 0,
            bridge_weight: 0.4,
            // 5 since §32.4b: on replayed real agent queries the pin is the
            // first engine change with a CI excluding zero (+0.014
            // [+0.007, +0.021] rank@5 on dir/root scopes, file scopes
            // untouched, both function metrics agreeing), and it re-displays
            // 20% of the §32.4a ranking-bucket misses. Cost: one lexical
            // query per ranked search (~88 ms warm at kernel scale).
            bm25_pin: 5,
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
