//! Building an index: one corpus pass, folded into a chunk table, a BM25 store,
//! and a quantized embedding matrix.

use super::{BuildOptions, BuildStats, FORMAT_VERSION, IndexMeta, index_dir};
use crate::rank::bm25::Bm25Index;
use crate::text::SifStats;
use crate::trace::{SCHEDULE_BUILD, Stage, Trace, elapsed_ms};
use crate::{Chunk, EMBED_DIM, corpus};
use anyhow::{Context, Result};
use std::path::Path;
use std::time::Instant;

mod embed;
mod sif;

use embed::EmbedWriter;

/// Every file the index format is made of. `meta.json` is last on purpose — see
/// `publish`.
const ARTIFACTS: [&str; 6] =
    ["meta.json", "chunks.bin", "bm25.flat", "emb.bin", "hnsw.bin", "sif.bin"];

/// Build (or fully rebuild) the repo-local `.semgrep/` index for `root`.
pub fn build(
    root: &Path,
    opts: &BuildOptions,
    progress: impl FnMut(usize, usize),
) -> Result<BuildStats> {
    build_at(&index_dir(root), root, opts, progress)
}

/// Build an index describing `root` into an arbitrary directory (repo-local
/// `.semgrep/` or a central cache entry).
pub fn build_at(
    dir: &Path,
    root: &Path,
    opts: &BuildOptions,
    progress: impl FnMut(usize, usize),
) -> Result<BuildStats> {
    let t_total = Instant::now();
    let mut trace = Trace::new(SCHEDULE_BUILD);

    let files = trace.time(Stage::BuildWalk, || corpus::walk(root, &opts.params))?;
    std::fs::create_dir_all(dir)?;
    unpublish(dir);

    let sif = trace.time(Stage::BuildSif, || opts.sif.then(|| sif::count(root, &files, opts)));
    let IndexPass { chunks, bm25, emb_rows, mut stats } =
        index_pass(dir, root, &files, opts, sif.as_ref(), progress, &mut trace)?;
    // The three tables must describe the same chunks in the same order. This is
    // the invariant the whole format rests on, and it is cheap to assert.
    debug_assert_eq!(chunks.len(), emb_rows, "chunk table and emb.bin disagree");
    debug_assert_eq!(chunks.len(), bm25.n_docs(), "chunk table and BM25 disagree");

    let meta = IndexMeta {
        version: FORMAT_VERSION,
        dims: EMBED_DIM,
        params: opts.params,
        n_chunks: chunks.len() as u64,
        has_hnsw: opts.hnsw,
        sif: sif.is_some(),
        files,
    };
    trace.time(Stage::BuildWrite, || publish(dir, &meta, &chunks, &bm25, sif.as_ref()))?;

    stats.n_chunks = chunks.len();
    stats.index_bytes = ARTIFACTS
        .iter()
        .filter_map(|name| std::fs::metadata(dir.join(name)).ok())
        .map(|m| m.len())
        .sum();
    stats.stages = trace.finish();
    stats.total_ms = elapsed_ms(t_total);
    Ok(stats)
}

/// Make the index unfindable before modifying it.
///
/// A rebuild overwrites `emb.bin` in place, so leaving the old `meta.json`
/// readable would let a concurrent query pair a stale file table with a
/// half-rewritten matrix. Absent costs that query a streaming pass; mixed gives
/// it a wrong answer.
fn unpublish(dir: &Path) {
    let _ = std::fs::remove_file(dir.join("meta.json"));
}

/// What one corpus pass produced. `emb.bin` and `hnsw.bin` are already on disk;
/// `emb_rows` is how many rows went into the matrix, for the lockstep check.
struct IndexPass {
    chunks: Vec<Chunk>,
    bm25: Bm25Index,
    emb_rows: usize,
    stats: BuildStats,
}

/// The corpus pass: read, chunk, tokenize, embed.
#[allow(clippy::too_many_arguments)]
fn index_pass(
    dir: &Path,
    root: &Path,
    files: &[crate::FileMeta],
    opts: &BuildOptions,
    sif: Option<&SifStats>,
    mut progress: impl FnMut(usize, usize),
    trace: &mut Trace,
) -> Result<IndexPass> {
    let t_pass = Instant::now();
    let mut writer = EmbedWriter::create(dir, opts.hnsw, sif)?;
    let mut bm25 = Bm25Index::new();
    let mut chunks: Vec<Chunk> = Vec::new();
    let mut stats = BuildStats { n_files: files.len(), ..Default::default() };

    // The fold closure cannot return, so the first write error is parked here
    // and stops the pass from being fed more work.
    let mut failed: Option<anyhow::Error> = None;
    corpus::pass(root, files, &opts.params, true, true, |i, work| {
        if failed.is_some() {
            return;
        }
        progress(i + 1, files.len());
        stats.bytes_indexed += work.bytes;
        for (chunk, doc, tokens) in work.docs {
            // Lockstep: one chunk id, one BM25 document, one embedding row, all
            // appended in the same order. corpus::pass guarantees file order.
            bm25.add_tokenized(tokens.expect("the build pass tokenizes"));
            chunks.push(chunk);
            if let Err(e) = writer.push(doc.expect("the build pass keeps text")) {
                failed = Some(e);
                return;
            }
        }
    });
    if let Some(e) = failed {
        return Err(e);
    }

    let (emb_rows, embed) = writer.finish(dir)?;
    // Reading, chunking and tokenizing is what is left of the pass once the two
    // things that dominate it — embedding and graph insertion — are taken out.
    // Derived rather than measured because the pass is a parallel fold: the work
    // interleaves across rayon workers and there is no single span to wrap.
    trace.record(Stage::BuildEmbed, embed.embed_ms);
    trace.record(Stage::BuildHnsw, embed.hnsw_ms);
    trace.record(
        Stage::BuildRead,
        (elapsed_ms(t_pass) - embed.embed_ms - embed.hnsw_ms).max(0.0),
    );

    // Term renumbering is part of producing bm25.flat, not part of the pass, so
    // it lands in the same bucket as writing it. `finish` sums repeats, so this
    // and `publish` add up under one name.
    let t_fin = Instant::now();
    bm25.finalize();
    trace.record(Stage::BuildWrite, elapsed_ms(t_fin));

    Ok(IndexPass { chunks, bm25, emb_rows, stats })
}

/// Write every artifact, `meta.json` last.
///
/// Writing `meta.json` is what publishes an index: `cache::discover` keys on it
/// and nothing else. When it went first, a concurrent reader could find the
/// entry, fail on the `chunks.bin` that did not exist yet, and — a cache load
/// failure being a miss — delete the directory this build was still writing.
fn publish(
    dir: &Path,
    meta: &IndexMeta,
    chunks: &[Chunk],
    bm25: &Bm25Index,
    sif: Option<&SifStats>,
) -> Result<()> {
    std::fs::write(dir.join("chunks.bin"), postcard::to_allocvec(chunks)?)?;
    std::fs::write(dir.join("bm25.flat"), super::bm25::to_flat_bytes(bm25))?;
    write_or_remove(&dir.join("sif.bin"), sif.map(postcard::to_allocvec).transpose()?)?;
    std::fs::write(dir.join("meta.json"), serde_json::to_vec_pretty(meta)?)?;
    Ok(())
}

/// Write `bytes`, or clear a stale artifact from a previous build with different
/// options — an index must not carry a `sif.bin` its `meta.json` disclaims.
fn write_or_remove(path: &Path, bytes: Option<Vec<u8>>) -> Result<()> {
    match bytes {
        Some(b) => {
            std::fs::write(path, b).with_context(|| format!("write {}", path.display()))?
        }
        None => {
            let _ = std::fs::remove_file(path);
        }
    }
    Ok(())
}
