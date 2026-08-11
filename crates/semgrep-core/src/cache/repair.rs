//! Read-repair: keeping a warm answer true of the tree as it is now.
//!
//! A cache entry describes the corpus as it was when written. Before serving
//! from one, diff the live tree under the query scope against its file table; if
//! anything drifted, index the drifted files in memory and fuse that overlay
//! into the ranking. Chunks of files that changed or vanished are tombstoned so
//! their indexed text can never be served.
//!
//! Validation is throttled by `SEMGREP_CACHE_TTL_SECS` (default 60; 0 = always).
//! RESEARCH.md §8 calls this read-repair and §8.1 lazy fill; they are the same
//! mechanism seen from two directions.

use crate::rank::bm25::Bm25Index;
use crate::text::{self};
use crate::trace::{Stage, Trace, elapsed_ms};
use crate::{Chunk, cache, corpus, rank, store};
use std::collections::HashSet;
use std::time::Instant;

/// Why a warm query did or did not repair.
///
/// Durations cannot answer this. `repair:walk = 0` means the TTL was fresh *or*
/// the walk failed *or* nothing drifted, and those are three different stories:
/// the first is a deliberate throttle, the second is silently degraded
/// correctness, the third is the healthy case. Reporting the category alongside
/// the timings is what makes the second one visible at all — it was a bare
/// `.ok()?` that returned `None` exactly like the healthy case.
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum RepairOutcome {
    /// This path does not repair: the cold path and exact mode never do.
    #[default]
    NotApplicable,
    /// The last validation was inside `SEMGREP_CACHE_TTL_SECS`, so no drift
    /// walk ran and the answer may be stale by design.
    TtlFresh { marker_age_secs: u64 },
    /// The scope walk failed. The index is served **unrepaired**, and the TTL
    /// marker has already been written, so the next query inside the window
    /// will not retry either.
    WalkFailed,
    /// The tree matches the index under this scope.
    NoDrift,
    /// Too much of the scope drifted to be worth patching: the entry is stale
    /// enough that rebuilding it is cheaper than repairing it, once. Only ever
    /// reported for a central-cache entry, which the engine can rebuild behind
    /// the caller's back; a repo-local `.semgrep/` keeps repairing however far
    /// it has drifted, because it belongs to the user and serving them stale
    /// text to save time is not a trade the engine gets to make.
    /// `dirty / total` is the ratio that crossed the threshold. Carried as the
    /// two counts rather than the quotient so the record stays integer-exact and
    /// the outcome keeps `Eq` — a float here would be compared for equality by
    /// every test that matches on this enum.
    DriftTooLarge { dirty: usize, total: usize },
    Repaired {
        added: usize,
        modified: usize,
        deleted: usize,
        /// Chunks in the in-memory overlay, bounded by [`max_drift_ratio`].
        delta_chunks: usize,
    },
}

/// In-memory index over the files the cache doesn't know correctly: changed,
/// new, or never-covered. Fused with the warm base at rank time so answers
/// are always true of the current tree (RESEARCH.md §8 "read-repair", §8.1
/// "lazy fill" — same mechanism).
pub struct Delta {
    pub chunks: Vec<Chunk>,
    /// Index-root-relative path per delta chunk.
    pub paths: Vec<String>,
    /// One quantized embedding per delta chunk, in exactly the representation
    /// `emb.bin` holds. Storing f32 here instead made the overlay score by
    /// full-precision cosine while the base scored quantized dot products, so a
    /// repaired answer differed from a rebuilt one for no reason to do with the
    /// query.
    pub vecs: Vec<Vec<i8>>,
    pub bm25: Bm25Index,
}

/// The overlay: what the cache does not know correctly, indexed in memory.
///
/// Fields are public because `search::indexed` fuses this into its ranking and
/// has to address delta rows directly. The id arithmetic that does so is the
/// most delicate part of the warm path; giving it a type of its own is the next
/// step, and these can go back to private then.
pub struct Repair {
    /// Base file_ids whose chunks must not be served (changed or deleted).
    pub tombstones: HashSet<u32>,
    pub delta: Delta,
    pub n_dirty: usize,
}

/// The share of a scope that may drift before repairing it stops being worth it.
///
/// RESEARCH.md §8 mechanism 2 specified this — "if the delta exceeds a threshold
/// (say >5% of files — branch switch), treat the whole query as a miss" — and it
/// was never implemented, so `repair` re-read, re-chunked and **re-embedded**
/// every drifted file on every query past the TTL, forever. Measured on tokio:
/// 131 ms at 50% drift against a 127 ms full cold pass, 197 ms at 100%, and it
/// never amortizes, because the overlay is discarded and rebuilt each time
/// (SIMULATION.md §1.3).
///
/// 5% is well clear of the loop this must not disturb — editing three files of
/// 865 is 0.35% — and the curve past it is steep enough that the exact number
/// barely matters: at 5% a rebuild pays for itself in about five queries.
///
/// A plain constant, reached through `SearchOptions`, rather than something read
/// from the environment behind a `OnceLock`. Latching a tunable per process is
/// what makes `cache_base` untestable (FIXES.md, open item 3), and a threshold
/// with no test that crosses it is not a threshold.
pub const DEFAULT_MAX_DRIFT: f32 = 0.05;

fn repair_ttl_secs() -> u64 {
    static TTL: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *TTL.get_or_init(|| {
        std::env::var("SEMGREP_CACHE_TTL_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(60)
    })
}

/// Where the "last validated at" timestamp for an index lives.
///
/// For a cache entry: inside it, where it doubles as the entry's access time
/// for LRU. For a repo-local `.semgrep/`, which is a committed artifact the
/// user owns: under the cache, because a search must not write into the
/// user's tree. Searching used to dirty a tracked directory.
fn check_marker(d: &cache::Discovered) -> std::path::PathBuf {
    if d.from_cache {
        return d.index_dir.join("last_check");
    }
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    d.index_dir.hash(&mut h);
    let checks = cache::cache_base().join("checks");
    let _ = std::fs::create_dir_all(&checks);
    checks.join(format!("{:016x}", h.finish()))
}

/// Throttled scoped validation: diff the live tree under the query scope
/// against the index's file table; build the overlay if anything drifted.
/// `max_drift` is the share of the scope above which the entry is reported as
/// [`RepairOutcome::DriftTooLarge`] instead of being patched; 0 disables the
/// bound. See [`DEFAULT_MAX_DRIFT`].
pub fn scope(
    d: &cache::Discovered,
    idx: &store::LoadedIndex,
    max_drift: f32,
    trace: &mut Trace,
) -> (Option<Repair>, RepairOutcome) {
    let marker = check_marker(d);
    let ttl = repair_ttl_secs();
    if ttl > 0 {
        let age = std::fs::metadata(&marker)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .map(|age| age.as_secs());
        if let Some(age) = age.filter(|&age| age < ttl) {
            return (None, RepairOutcome::TtlFresh { marker_age_secs: age });
        }
    }
    let _ = std::fs::write(&marker, b"");

    let t0 = Instant::now();
    let scope_abs =
        if d.prefix.is_empty() { d.root.clone() } else { corpus::resolve(&d.root, &d.prefix) };
    let Ok(live) = corpus::walk(&scope_abs, &idx.meta.params) else {
        // The marker was written above, so this failure also suppresses the
        // next TTL window's attempt. Reported rather than swallowed.
        trace.record(Stage::RepairWalk, elapsed_ms(t0));
        return (None, RepairOutcome::WalkFailed);
    };
    let drift = corpus::diff(&idx.meta.files, &live, &d.prefix);
    trace.record(Stage::RepairWalk, elapsed_ms(t0));
    if drift.is_empty() {
        return (None, RepairOutcome::NoDrift);
    }
    let (n_added, n_modified, n_deleted) =
        (drift.added.len(), drift.modified.len(), drift.deleted.len());
    let tombstones: HashSet<u32> = drift.tombstones().collect();
    let n_dirty = drift.len();

    // How much of this scope moved. The denominator is the larger of what is
    // there now and what the index has under the prefix, so a mass deletion —
    // where `live` is nearly empty — reads as near-total drift rather than as a
    // ratio above one.
    let indexed_here = if d.prefix.is_empty() {
        idx.meta.files.len()
    } else {
        idx.meta
            .files
            .iter()
            .filter(|f| {
                f.path.strip_prefix(&d.prefix).is_some_and(|r| r.starts_with('/'))
            })
            .count()
    };
    let total = live.len().max(indexed_here);
    let ratio = n_dirty as f32 / total.max(1) as f32;
    let threshold = max_drift;
    // Only for a cache entry: `search` answers this by rebuilding, which it may
    // only do to something it owns. A repo-local `.semgrep/` is the user's
    // artifact, so it repairs however far it has drifted and reports the
    // staleness rather than quietly serving around it.
    if d.from_cache && threshold > 0.0 && ratio > threshold {
        return (None, RepairOutcome::DriftTooLarge { dirty: n_dirty, total });
    }

    let t0 = Instant::now();
    let mut delta = Delta {
        chunks: Vec::new(),
        paths: Vec::new(),
        vecs: Vec::new(),
        bm25: Bm25Index::new(),
    };
    let mut texts: Vec<String> = Vec::new();
    for path in drift.stale_paths() {
        // Via `resolve`, not a raw join: a file root records the file's own
        // name as its relative path, and `root.join(rel)` would look for
        // `<file>/<file>` and fail ENOTDIR — silently, because `read_text`
        // maps every IO error to the same `None` it uses for binary files
        // (§16.11). Unreachable today only because `cache::discover` refuses
        // a non-directory root, which is a guard three layers away that
        // nothing here can see. Serving a file-scoped query from an ancestor
        // index is an obvious optimization; this is what keeps it from
        // resurrecting the bug.
        let Some(text) = corpus::read_text(&corpus::resolve(&d.root, path)) else { continue };
        for (chunk, slice) in corpus::chunk_lines(0, path, &text, &idx.meta.params) {
            let doc = corpus::doc_text(path, slice);
            delta.bm25.add_doc(&doc);
            delta.chunks.push(chunk);
            delta.paths.push(path.clone());
            texts.push(doc);
        }
    }
    delta.bm25.finalize();
    // Delta vectors must live in the same space as the base matrix — same SIF
    // stats, same prose rendering. The raw doc stays raw for BM25 above.
    let pp = idx.meta.embed_preproc;
    let pr = idx.meta.path_render;
    let rendered: Vec<std::borrow::Cow<'_, str>> =
        texts.iter().map(|t| text::prose_render_doc(t, pp, pr)).collect();
    let mut vecs: Vec<[f32; crate::EMBED_DIM]> = match &idx.sif {
        Some(s) => rendered.iter().map(|t| text::embed_sif(t, s)).collect(),
        None => ese::encode(rendered.iter()),
    };
    delta.vecs = vecs
        .iter_mut()
        .map(|v| {
            rank::normalize(v);
            rank::quantize_i8(v)
        })
        .collect();
    trace.record(Stage::RepairDelta, elapsed_ms(t0));
    let outcome = RepairOutcome::Repaired {
        added: n_added,
        modified: n_modified,
        deleted: n_deleted,
        delta_chunks: delta.chunks.len(),
    };
    (Some(Repair { tombstones, delta, n_dirty }), outcome)
}
