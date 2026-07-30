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
pub fn walk(root: &Path, params: &ChunkParams) -> Result<Vec<FileMeta>> {
    let mut files = Vec::new();
    for entry in ignore::WalkBuilder::new(root).build() {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.len() > params.max_file_bytes {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(root)
            .unwrap_or(entry.path())
            .to_string_lossy()
            .replace('\\', "/");
        if rel.starts_with(".semgrep/") || rel == ".semgrep" {
            continue;
        }
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        files.push(FileMeta { path: rel, size: meta.len(), mtime });
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

pub fn abs_path(root: &Path, file: &FileMeta) -> PathBuf {
    root.join(&file.path)
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
