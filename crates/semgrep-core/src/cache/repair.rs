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
    Repaired {
        added: usize,
        modified: usize,
        deleted: usize,
        /// Chunks in the in-memory overlay. Unbounded — see RESEARCH.md §8's
        /// unimplemented delta-size threshold.
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
pub fn scope(
    d: &cache::Discovered,
    idx: &store::LoadedIndex,
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
    let scope_abs = if d.prefix.is_empty() { d.root.clone() } else { d.root.join(&d.prefix) };
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

    let t0 = Instant::now();
    let mut delta = Delta {
        chunks: Vec::new(),
        paths: Vec::new(),
        vecs: Vec::new(),
        bm25: Bm25Index::new(),
    };
    let mut texts: Vec<String> = Vec::new();
    for path in drift.stale_paths() {
        let Some(text) = corpus::read_text(&d.root.join(path)) else { continue };
        for (chunk, slice) in corpus::chunk_lines(0, &text, &idx.meta.params) {
            let doc = corpus::doc_text(path, slice);
            delta.bm25.add_doc(&doc);
            delta.chunks.push(chunk);
            delta.paths.push(path.clone());
            texts.push(doc);
        }
    }
    delta.bm25.finalize();
    // Delta vectors must live in the same space as the base matrix.
    let mut vecs: Vec<[f32; crate::EMBED_DIM]> = match &idx.sif {
        Some(s) => texts.iter().map(|t| text::embed_sif(t, s)).collect(),
        None => ese::encode(texts.iter()),
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
