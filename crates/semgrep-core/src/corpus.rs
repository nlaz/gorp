//! Corpus walking and chunking.

use crate::{Chunk, ChunkParams, FileMeta};
use anyhow::{Context, Result};
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

/// Split file text into overlapping line-window chunks, yielding the chunk
/// record and its text slice bounds so callers can avoid copying.
pub fn chunk_lines<'a>(
    file_id: u32,
    text: &'a str,
    params: &ChunkParams,
) -> Vec<(Chunk, &'a str)> {
    let mut line_starts: Vec<usize> = vec![0];
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            line_starts.push(i + 1);
        }
    }
    // Trailing entry marks EOF so slicing line i..j is uniform.
    if *line_starts.last().unwrap() != text.len() {
        line_starts.push(text.len());
    }
    let n_lines = line_starts.len() - 1;
    if n_lines == 0 {
        return Vec::new();
    }

    let window = params.window.max(1) as usize;
    let stride = window.saturating_sub(params.overlap as usize).max(1);
    let mut chunks = Vec::new();
    let mut start = 0usize;
    loop {
        let end = (start + window).min(n_lines);
        let slice = &text[line_starts[start]..line_starts[end]];
        if !slice.trim().is_empty() {
            chunks.push((
                Chunk {
                    file_id,
                    start_line: start as u32 + 1,
                    end_line: end as u32,
                },
                slice,
            ));
        }
        if end == n_lines {
            break;
        }
        start += stride;
    }
    chunks
}

/// Split files into batches bounded by file count *and* cumulative bytes,
/// yielding (base_index, slice). Bounds resident text (and token maps)
/// during the parallel pass regardless of file-size distribution.
pub fn pass_batches(
    files: &[FileMeta],
    max_files: usize,
    max_bytes: u64,
) -> Vec<(usize, &[FileMeta])> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut bytes = 0u64;
    for (i, f) in files.iter().enumerate() {
        let full = i - start >= max_files || (i > start && bytes + f.size > max_bytes);
        if full {
            out.push((start, &files[start..i]));
            start = i;
            bytes = 0;
        }
        bytes += f.size;
    }
    if start < files.len() {
        out.push((start, &files[start..]));
    }
    out
}

/// Per-file output of the parallel pass phase: everything CPU-heavy about a
/// file (read, chunk, doc-text assembly, tokenization) computed off-thread.
/// The serial consumer assigns chunk ids in file order, so the chunk table /
/// BM25 add order / embedding row lockstep is preserved by construction.
pub struct FileWork {
    pub bytes: u64,
    /// (chunk, doc text for embedding, tokenized doc for BM25) in file order.
    pub docs: Vec<(Chunk, Option<String>, Option<crate::bm25::TokenizedDoc>)>,
}

/// Process one file for the pass: `want_text` keeps the doc text (embedding),
/// `want_tokens` tokenizes for BM25. Runs on rayon workers.
pub fn process_file(
    root: &Path,
    file_id: u32,
    fm: &FileMeta,
    params: &ChunkParams,
    want_text: bool,
    want_tokens: bool,
) -> FileWork {
    let Some(text) = read_text(&abs_path(root, fm)) else {
        return FileWork { bytes: 0, docs: Vec::new() };
    };
    let docs = chunk_lines(file_id, &text, params)
        .into_iter()
        .map(|(chunk, slice)| {
            let doc = doc_text(&fm.path, slice);
            let tokens = want_tokens.then(|| crate::bm25::tokenize_doc(&doc));
            (chunk, want_text.then_some(doc), tokens)
        })
        .collect();
    FileWork { bytes: text.len() as u64, docs }
}

/// Extract the lines of `chunk` from a file given by root-relative path
/// (for query-time re-reads where no FileMeta is at hand: PRF term
/// extraction, MaxSim reranking).
pub fn chunk_text_rel(root: &Path, rel_path: &str, chunk: &Chunk) -> Option<String> {
    let text = read_text(&root.join(rel_path))?;
    let mut out = String::new();
    for (i, line) in text.lines().enumerate() {
        let line_no = i as u32 + 1;
        if line_no >= chunk.start_line && line_no <= chunk.end_line {
            out.push_str(line);
            out.push('\n');
        }
        if line_no > chunk.end_line {
            break;
        }
    }
    Some(out)
}

/// Extract the lines of `chunk` from its file on disk (for result display).
pub fn chunk_text(root: &Path, file: &FileMeta, chunk: &Chunk) -> Result<String> {
    let path = abs_path(root, file);
    let text = read_text(&path)
        .with_context(|| format!("cannot re-read {}", path.display()))?;
    let mut out = String::new();
    for (i, line) in text.lines().enumerate() {
        let line_no = i as u32 + 1;
        if line_no >= chunk.start_line && line_no <= chunk.end_line {
            out.push_str(line);
            out.push('\n');
        }
        if line_no > chunk.end_line {
            break;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunking_covers_all_lines_with_overlap() {
        let text = (1..=100).map(|i| format!("line {i}\n")).collect::<String>();
        let params = ChunkParams { window: 32, overlap: 8, ..Default::default() };
        let chunks = chunk_lines(0, &text, &params);
        assert_eq!(chunks[0].0.start_line, 1);
        assert_eq!(chunks[0].0.end_line, 32);
        assert_eq!(chunks[1].0.start_line, 25);
        // stride 24: starts at 1, 25, 49, 73; the last window absorbs the tail
        assert_eq!(chunks.len(), 4);
        assert_eq!(chunks.last().unwrap().0.start_line, 73);
        assert_eq!(chunks.last().unwrap().0.end_line, 100);
    }

    #[test]
    fn short_file_is_one_chunk() {
        let chunks = chunk_lines(0, "a\nb\n", &ChunkParams::default());
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].1, "a\nb\n");
    }

    #[test]
    fn empty_and_blank_files_yield_nothing() {
        assert!(chunk_lines(0, "", &ChunkParams::default()).is_empty());
        assert!(chunk_lines(0, "\n\n  \n", &ChunkParams::default()).is_empty());
    }
}
