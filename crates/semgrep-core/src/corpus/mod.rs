//! Turning a directory into files, and files into chunks.
//!
//! The bottom layer of the engine: everything here is about the corpus itself,
//! and nothing here knows about queries, ranking, or storage. Split by job —
//! walking a tree (here), cutting files into chunks (`chunk`), driving the
//! parallel read pass (`pass`), and comparing a tree against an index
//! (`diff`). The submodules are re-exported, so callers write
//! `corpus::chunk_lines`, not `corpus::chunk::chunk_lines`.

mod chunk;
mod diff;
mod pass;

pub use chunk::{chunk_lines, lines};
pub use diff::{Diff, diff};
pub use pass::{FileWork, pass, pass_batches, process_file};

use crate::{ChunkParams, FileMeta};
use anyhow::Result;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// Walk `root` respecting .gitignore/.ignore, skipping hidden files, binaries,
/// and files over the size cap. Returns files in sorted (deterministic) order.
///
/// Traversal is parallel, and **the sort at the end is what makes that safe**.
/// Chunk ids are assigned in walk order and must stay in lockstep across the
/// chunk table, the BM25 add order and the `emb.bin` rows — but "walk order" is
/// defined by this sort, not by the order directories happen to be read in. So
/// visiting them concurrently changes nothing downstream, as long as the sort
/// stays. Anything that removes it silently breaks the index format.
///
/// This is worth parallelizing because it is not only a build cost. Read-repair
/// walks the query's scope on every warm query past the TTL, where it was the
/// larger half of a small query (6.7 ms of 8.7 ms on tokio) and 1.7 s on the
/// 84k-file kernel.
pub fn walk(root: &Path, params: &ChunkParams) -> Result<Vec<FileMeta>> {
    let files = std::sync::Mutex::new(Vec::new());
    let max_bytes = params.max_file_bytes;

    ignore::WalkBuilder::new(root)
        // Match the pool the rest of the pass uses rather than spawning a
        // second, differently-sized one alongside it.
        .threads(rayon::current_num_threads().max(1))
        // Depth 1 only, which is where a repo-local index lives — the same
        // top-level-only test the serial version applied to the relative path.
        // Doing it here rather than per entry also stops the walker descending
        // into an index directory at all.
        .filter_entry(|e| e.depth() != 1 || !is_index_dir(&e.file_name().to_string_lossy()))
        .build_parallel()
        .run(|| {
            Box::new(|entry| {
                let Ok(entry) = entry else { return ignore::WalkState::Continue };
                if !entry.file_type().is_some_and(|t| t.is_file()) {
                    return ignore::WalkState::Continue;
                }
                let Ok(meta) = entry.metadata() else { return ignore::WalkState::Continue };
                if meta.len() > max_bytes {
                    return ignore::WalkState::Continue;
                }
                // A root that IS a file strips to the empty string, and an
                // empty relative path breaks every downstream consumer —
                // ranked search over a single file silently returned nothing,
                // 100% of the time, for as long as this has existed
                // (RESEARCH.md §16.11; 47% of one campaign's semantic
                // searches). Exact mode uses the keyword path and never hit
                // it, which is why no test saw it. Fall back to the file's
                // own name so the path is always non-empty.
                let stripped = entry.path().strip_prefix(root).unwrap_or(entry.path());
                let rel = if stripped.as_os_str().is_empty() {
                    entry.file_name().to_string_lossy().into_owned()
                } else {
                    stripped.to_string_lossy().replace('\\', "/")
                };
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                // The lock covers a `Vec::push`; the `stat` above, which is the
                // actual cost, happens outside it.
                if let Ok(mut out) = files.lock() {
                    out.push(FileMeta { path: rel, size: meta.len(), mtime });
                }
                ignore::WalkState::Continue
            })
        });

    let mut files = files.into_inner().unwrap_or_else(|e| e.into_inner());
    // Unstable is fine and is the point: paths are unique, so the order is total
    // and does not depend on which thread found what.
    files.sort_unstable_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

/// A repo-local index directory, or one of the transient siblings a build swaps
/// through (`.semgrep.building-<pid>`, `.semgrep.trash-<pid>`). None of them are
/// corpus: indexing an index is circular, and a staging directory would also
/// churn the file table on every rebuild.
///
/// Spelled out here rather than asked of `store`, which owns the naming, because
/// `corpus` is the bottom layer and may not call upward. The trailing dot is
/// what keeps this from swallowing a user's `.semgreprc`.
fn is_index_dir(component: &str) -> bool {
    component == ".semgrep" || component.starts_with(".semgrep.")
}

pub fn abs_path(root: &Path, file: &FileMeta) -> PathBuf {
    resolve(root, &file.path)
}

/// Resolve a root-relative path against the scope root.
///
/// The scope may itself BE a file — agents do this constantly, scoping a
/// follow-up query to the file they just found. In that case `walk` records
/// the file's own name as its relative path and a plain `root.join(rel)`
/// looks for `<file>/<file>`, which does not exist. Every read then fails and
/// the search returns nothing while reporting success: ranked search over a
/// single file was empty 100% of the time, across four separate join sites
/// (RESEARCH.md §16.11). One helper so a fifth site cannot reintroduce it.
pub fn resolve(root: &Path, rel: &str) -> PathBuf {
    if root.is_file() {
        return root.to_path_buf();
    }
    root.join(rel)
}

/// The text a chunk contributes to BM25 and embeddings: the relative file
/// path prepended to the chunk body. Path segments carry strong signal
/// (`block/blk-cgroup-rwstat.h` says more than many code lines) that both
/// engines were previously blind to.
pub fn doc_text(rel_path: &str, slice: &str) -> String {
    let mut s = String::with_capacity(rel_path.len() + 1 + slice.len());
    s.push_str(rel_path);
    s.push('\n');
    s.push_str(slice);
    s
}

/// Read a corpus file as text. Returns `None` for binary (NUL-containing) or
/// unreadable files. Invalid UTF-8 is replaced lossily so we never bail on
/// mixed-encoding trees.
pub fn read_text(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let sniff = &bytes[..bytes.len().min(8192)];
    if sniff.contains(&0) {
        return None;
    }
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests;
