//! Building an index: one corpus pass, folded into a chunk table, a BM25 store,
//! and a quantized embedding matrix.

use super::{BuildOptions, BuildStats, FORMAT_VERSION, IndexMeta, index_dir};
use crate::rank::bm25::Bm25Index;
use crate::text::SifStats;
use crate::trace::{SCHEDULE_BUILD, Stage, Trace, elapsed_ms};
use crate::{Chunk, EMBED_DIM, corpus};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::time::Instant;

mod embed;
mod sif;

use embed::EmbedWriter;

/// Every file the index format is made of. `meta.json` is last on purpose — see
/// `publish`.
const ARTIFACTS: [&str; 6] =
    ["meta.json", "chunks.bin", "bm25.flat", "emb.bin", "hnsw.bin", "sif.bin"];

/// Suffixes for the two directories that exist only during a swap. Both are
/// siblings of the entry they belong to, so the renames stay on one filesystem,
/// and both carry the pid so concurrent builds cannot collide on a name.
///
/// `cache` has to recognize these: they must not be discovered as entries, and
/// they must still be reclaimable if a build dies between the two renames.
pub const STAGING_SUFFIX: &str = ".building-";
pub const TRASH_SUFFIX: &str = ".trash-";

fn sibling(dir: &Path, suffix: &str) -> PathBuf {
    let name = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    dir.with_file_name(format!("{name}{suffix}{}", std::process::id()))
}

/// Where a build for `dir` assembles itself before being published.
pub fn staging_path(dir: &Path) -> PathBuf {
    sibling(dir, STAGING_SUFFIX)
}

/// True for a directory that is mid-swap rather than an index in its own right.
pub fn is_transient(dir: &Path) -> bool {
    let Some(name) = dir.file_name().and_then(|n| n.to_str()) else { return false };
    [STAGING_SUFFIX, TRASH_SUFFIX].iter().any(|s| {
        name.rsplit_once(s).is_some_and(|(stem, pid)| {
            !stem.is_empty() && !pid.is_empty() && pid.chars().all(|c| c.is_ascii_digit())
        })
    })
}

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
    build_staged(&staging_path(dir), dir, root, opts, progress)
}

/// Build into `staging`, then swap it into place at `dir` with two renames.
///
/// The staging directory is the whole point, and it is what closes the SIGBUS.
/// A build used to write `emb.bin` straight into the live entry, truncating a
/// file another process had already `mmap`ed — and a mapping whose backing file
/// is truncated faults on access, which is a signal, not an error anyone can
/// catch. Measured at 5–8 bad trials in 20 on a small corpus (SIMULATION.md
/// §1.2). Removing `meta.json` first stopped a *new* reader from starting but
/// could not recall a mapping that already existed.
///
/// Nothing here ever mutates a published file. The swap replaces a directory
/// *entry*, so a reader mid-query keeps the inodes it already opened and
/// finishes against a complete, consistent index — the old one. That also makes
/// the old `unpublish` unnecessary: there is no window in which a live entry is
/// half-written, so there is nothing to hide from a reader.
///
/// The caller may seed `staging` with extra files first (`cache` writes
/// `root.txt` and `params.txt` there, so an interrupted build is still
/// enumerable and reclaimable).
pub fn build_staged(
    staging: &Path,
    dir: &Path,
    root: &Path,
    opts: &BuildOptions,
    progress: impl FnMut(usize, usize),
) -> Result<BuildStats> {
    let result = build_unswapped(staging, root, opts, progress)
        .and_then(|stats| swap_into_place(staging, dir).map(|()| stats));
    if result.is_err() {
        // A failed build must not leave a directory behind that the budget
        // enforcer will count against the cap for ten minutes.
        let _ = std::fs::remove_dir_all(staging);
    }
    result
}

/// Replace `dir` with `staging`. Two renames, both atomic, neither of which
/// touches a byte of a file anyone may have mapped.
///
/// `rename(2)` will not replace a non-empty directory, so the old entry is
/// moved aside first rather than deleted — if the second rename fails, that is
/// what allows the old index to be put back instead of leaving the scope with
/// no index at all.
fn swap_into_place(staging: &Path, dir: &Path) -> Result<()> {
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let trash = sibling(dir, TRASH_SUFFIX);
    let had_old = dir.exists();
    if had_old {
        let _ = std::fs::remove_dir_all(&trash); // a previous crash's leftovers
        std::fs::rename(dir, &trash)
            .with_context(|| format!("move aside {}", dir.display()))?;
    }
    match std::fs::rename(staging, dir) {
        Ok(()) => {
            // Readers that hold the old inodes keep them; only the name is gone.
            let _ = std::fs::remove_dir_all(&trash);
            Ok(())
        }
        Err(e) => {
            if had_old {
                let _ = std::fs::rename(&trash, dir);
            }
            Err(anyhow::Error::new(e)
                .context(format!("publish index into {}", dir.display())))
        }
    }
}

/// The build itself, entirely inside `staging`.
fn build_unswapped(
    dir: &Path,
    root: &Path,
    opts: &BuildOptions,
    progress: impl FnMut(usize, usize),
) -> Result<BuildStats> {
    let t_total = Instant::now();
    let mut trace = Trace::new(SCHEDULE_BUILD);

    let files = trace.time(Stage::BuildWalk, || corpus::walk(root, &opts.params))?;
    std::fs::create_dir_all(dir)?;

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
/// Writing `meta.json` is what makes an index loadable: `cache::discover` keys
/// on it and nothing else. Now that a build assembles itself in a staging
/// directory nobody can discover, the ordering no longer guards a race — but it
/// stays, because it is what makes the *staging* directory self-describing:
/// a `meta.json` in a `.building-` dir means the build finished and only the
/// swap is outstanding, which is exactly what `cache_status` needs to tell an
/// interrupted build from one in flight.
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
