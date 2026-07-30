//! The corpus pass: read, chunk, and tokenize every file in parallel while
//! keeping chunk ids in lockstep with the order the caller folds them.

use super::chunk::chunk_lines;
use super::{abs_path, doc_text, read_text};
use crate::{Chunk, ChunkParams, FileMeta};
use std::path::Path;

/// Run the corpus pass: read, chunk, and tokenize every file, handing each
/// file's work to `fold` in walk order.
///
/// The parallelism and the ordering are both load-bearing, which is why this
/// exists once instead of once per caller (index build and cold search had a
/// copy each). Files are processed in bounded batches on rayon workers — the
/// serial version left most cores idle for two thirds of the pass — and folded
/// back serially in file order, so chunk ids stay in lockstep across the chunk
/// table, the BM25 add order, and the embedding rows. Batches bound resident
/// text to a few MB regardless of corpus size.
///
/// `want_text` keeps chunk text for embedding; `want_tokens` tokenizes for
/// BM25. `fold` sees `(file_index, FileWork)`.
pub fn pass(
    root: &Path,
    files: &[FileMeta],
    params: &ChunkParams,
    want_text: bool,
    want_tokens: bool,
    mut fold: impl FnMut(usize, FileWork),
) {
    use rayon::prelude::*;
    for (base, batch) in pass_batches(files, PASS_MAX_FILES, PASS_MAX_BYTES) {
        let works: Vec<FileWork> = batch
            .par_iter()
            .enumerate()
            .map(|(i, fm)| {
                process_file(root, (base + i) as u32, fm, params, want_text, want_tokens)
            })
            .collect();
        for (i, work) in works.into_iter().enumerate() {
            fold(base + i, work);
        }
    }
}

/// Batch bounds for [`pass`]: at most this many files, or this many bytes of
/// source, resident per batch.
const PASS_MAX_FILES: usize = 256;
const PASS_MAX_BYTES: u64 = 16 << 20;

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
    pub docs: Vec<(Chunk, Option<String>, Option<crate::rank::bm25::TokenizedDoc>)>,
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
            let tokens = want_tokens.then(|| crate::rank::bm25::tokenize_doc(&doc));
            (chunk, want_text.then_some(doc), tokens)
        })
        .collect();
    FileWork { bytes: text.len() as u64, docs }
}
