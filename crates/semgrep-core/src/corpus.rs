//! Corpus walking and chunking.

use crate::{Chunk, ChunkParams, FileMeta};
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
                Chunk { file_id, start_line: start as u32 + 1, end_line: end as u32 },
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

/// Re-read a chunk's lines from disk, by root-relative path.
///
/// Chunk text is never stored, so anything that needs the body — displaying a
/// hit, mining PRF terms, scoring MaxSim — comes back here. Returns `None` if
/// the file has become unreadable or vanished since it was indexed, which is
/// normal on a tree that moves under you.
pub fn lines(root: &Path, rel_path: &str, chunk: &Chunk) -> Option<String> {
    let text = read_text(&root.join(rel_path))?;
    let mut out = String::new();
    for (i, line) in text.lines().enumerate() {
        let line_no = i as u32 + 1;
        if line_no > chunk.end_line {
            break;
        }
        if line_no >= chunk.start_line {
            out.push_str(line);
            out.push('\n');
        }
    }
    Some(out)
}

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

    // -----------------------------------------------------------------------
    // tree diff
    // -----------------------------------------------------------------------

    fn fm(path: &str, size: u64, mtime: u64) -> FileMeta {
        FileMeta { path: path.into(), size, mtime }
    }

    #[test]
    fn diff_of_an_unchanged_tree_is_empty() {
        let files = vec![fm("a.rs", 10, 100), fm("b/c.rs", 20, 200)];
        let d = diff(&files, &files, "");
        assert!(d.is_empty(), "identical trees must not drift: {d:?}");
    }

    #[test]
    fn diff_classifies_each_kind_of_drift() {
        let indexed = vec![
            fm("kept.rs", 10, 100),
            fm("resized.rs", 20, 200),
            fm("touched.rs", 30, 300),
            fm("gone.rs", 40, 400),
        ];
        let live = vec![
            fm("kept.rs", 10, 100),
            fm("resized.rs", 21, 200), // size moved
            fm("touched.rs", 30, 301), // mtime moved
            fm("fresh.rs", 50, 500),   // never indexed
        ];
        let d = diff(&indexed, &live, "");
        assert_eq!(d.added, ["fresh.rs"]);
        assert_eq!(d.modified, [(1, "resized.rs".into()), (2, "touched.rs".into())]);
        assert_eq!(d.deleted, [3], "gone.rs is file_id 3");
        assert_eq!(d.len(), 4, "each drifted file counted exactly once");

        // Tombstones cover modified *and* deleted: serving either file's
        // indexed content would show text that is no longer there.
        let mut tombs: Vec<u32> = d.tombstones().collect();
        tombs.sort_unstable();
        assert_eq!(tombs, [1, 2, 3]);

        // Only added and modified need re-reading; a deletion has nothing to read.
        let stale: Vec<&String> = d.stale_paths().collect();
        assert_eq!(stale.len(), 3);
        assert!(!stale.iter().any(|p| p.as_str() == "gone.rs"));
    }

    /// The scoped case, and the one worth having a test for: a query against a
    /// subdirectory walks only that subtree, so every indexed file outside it
    /// is absent from `live`. Without the prefix filter the whole rest of the
    /// corpus reads as deleted — the entire index would be tombstoned on every
    /// subdirectory search.
    #[test]
    fn diff_scoped_to_a_prefix_ignores_the_rest_of_the_corpus() {
        let indexed =
            vec![fm("src/a.rs", 10, 100), fm("src/b.rs", 20, 200), fm("docs/c.md", 30, 300)];
        // Walked under src/, so live paths are relative to src/.
        let live = vec![fm("a.rs", 10, 100), fm("b.rs", 21, 200)];
        let d = diff(&indexed, &live, "src");
        assert!(d.added.is_empty());
        assert_eq!(d.modified, [(1, "src/b.rs".into())], "paths come back index-relative");
        assert!(d.deleted.is_empty(), "docs/ is out of scope, not deleted");
    }

    /// A prefix must match whole path segments. `src` must not claim `srcgen/`.
    #[test]
    fn diff_prefix_matches_whole_segments_only() {
        let indexed = vec![fm("src/a.rs", 10, 100), fm("srcgen/b.rs", 20, 200)];
        let d = diff(&indexed, &[fm("a.rs", 10, 100)], "src");
        assert!(d.is_empty(), "srcgen/ must not be treated as inside src/: {d:?}");
    }

    // -----------------------------------------------------------------------
    // the pass: batching and in-order folding
    // -----------------------------------------------------------------------

    #[test]
    fn batches_are_bounded_by_count_and_by_bytes() {
        let small: Vec<FileMeta> = (0..10).map(|i| fm(&format!("f{i}"), 1, 0)).collect();
        let batches = pass_batches(&small, 4, 1 << 20);
        assert_eq!(batches.iter().map(|(_, b)| b.len()).collect::<Vec<_>>(), [4, 4, 2]);
        // Base indices must let the caller reconstruct the global file index.
        assert_eq!(batches.iter().map(|(base, _)| *base).collect::<Vec<_>>(), [0, 4, 8]);

        // Byte cap splits earlier than the count cap would.
        let fat: Vec<FileMeta> = (0..6).map(|i| fm(&format!("f{i}"), 100, 0)).collect();
        let by_bytes = pass_batches(&fat, 100, 250);
        assert!(
            by_bytes.iter().all(|(_, b)| b.len() <= 3),
            "250-byte cap over 100-byte files means at most 3 per batch"
        );

        // Every file appears exactly once, in order.
        let flat: Vec<&str> =
            by_bytes.iter().flat_map(|(_, b)| b.iter().map(|f| f.path.as_str())).collect();
        assert_eq!(flat, ["f0", "f1", "f2", "f3", "f4", "f5"]);
    }

    /// A single file larger than the byte cap must still be processed, in a
    /// batch of its own, rather than being skipped or splitting into nothing.
    #[test]
    fn a_file_over_the_byte_cap_still_gets_a_batch() {
        let files = vec![fm("small", 10, 0), fm("huge", 10_000, 0), fm("also_small", 10, 0)];
        let batches = pass_batches(&files, 100, 100);
        let seen: Vec<&str> =
            batches.iter().flat_map(|(_, b)| b.iter().map(|f| f.path.as_str())).collect();
        assert_eq!(seen, ["small", "huge", "also_small"], "no file may be dropped");
    }

    #[test]
    fn pass_batches_of_nothing_is_nothing() {
        assert!(pass_batches(&[], 4, 1 << 20).is_empty());
    }

    /// The pass's contract, and the one CLAUDE.md calls load-bearing: work
    /// arrives in walk order with a dense global file index, so a serial fold
    /// can assign chunk ids that stay in lockstep with the BM25 add order and
    /// the embedding rows. The processing is parallel, so this is the only
    /// thing keeping those three in agreement.
    #[test]
    fn pass_folds_in_file_order_with_dense_indices() {
        let dir = tempfile::tempdir().unwrap();
        // Enough files to span several batches once the caps are small.
        for i in 0..40 {
            std::fs::write(
                dir.path().join(format!("f{i:03}.txt")),
                format!("line one of {i}\nline two of {i}\n"),
            )
            .unwrap();
        }
        let params = ChunkParams { window: 8, overlap: 2, ..Default::default() };
        let files = walk(dir.path(), &params).unwrap();
        assert_eq!(files.len(), 40);

        let mut order: Vec<usize> = Vec::new();
        let mut chunk_file_ids: Vec<u32> = Vec::new();
        pass(dir.path(), &files, &params, true, true, |i, work| {
            order.push(i);
            for (chunk, doc, tokens) in work.docs {
                chunk_file_ids.push(chunk.file_id);
                assert!(doc.is_some(), "want_text was requested");
                assert!(tokens.is_some(), "want_tokens was requested");
            }
        });

        assert_eq!(order, (0..40).collect::<Vec<_>>(), "fold must see files in walk order");
        // file_id is the global file index, so it must match the fold order and
        // never skip: a gap here is a chunk pointing at the wrong file.
        assert_eq!(chunk_file_ids, (0..40).map(|i| i as u32).collect::<Vec<_>>());
    }

    /// A binary file yields no work but must not disturb the indices of the
    /// files around it — a skipped file still occupies its file_id.
    #[test]
    fn pass_skips_unreadable_files_without_shifting_indices() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello world\n").unwrap();
        std::fs::write(dir.path().join("b.bin"), b"bad\0bytes").unwrap();
        std::fs::write(dir.path().join("c.txt"), "goodbye world\n").unwrap();

        let params = ChunkParams::default();
        let files = walk(dir.path(), &params).unwrap();
        let mut work_by_index: Vec<(usize, usize)> = Vec::new();
        pass(dir.path(), &files, &params, true, true, |i, work| {
            work_by_index.push((i, work.docs.len()));
        });
        assert_eq!(
            work_by_index,
            [(0, 1), (1, 0), (2, 1)],
            "the binary file yields nothing but keeps index 1"
        );
    }

    // -----------------------------------------------------------------------
    // chunk re-reads
    // -----------------------------------------------------------------------

    #[test]
    fn lines_returns_exactly_the_chunk_span() {
        let dir = tempfile::tempdir().unwrap();
        let body: String = (1..=10).map(|i| format!("line {i}\n")).collect();
        std::fs::write(dir.path().join("f.txt"), &body).unwrap();
        let chunk = Chunk { file_id: 0, start_line: 3, end_line: 5 };
        assert_eq!(lines(dir.path(), "f.txt", &chunk).unwrap(), "line 3\nline 4\nline 5\n");
    }

    #[test]
    fn lines_of_a_missing_file_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let chunk = Chunk { file_id: 0, start_line: 1, end_line: 2 };
        assert!(lines(dir.path(), "gone.txt", &chunk).is_none());
    }

    /// A span past the end of a file — the file shrank since it was indexed —
    /// returns what is there rather than failing or padding.
    #[test]
    fn lines_past_end_of_file_returns_what_exists() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "only\ntwo\n").unwrap();
        let chunk = Chunk { file_id: 0, start_line: 2, end_line: 99 };
        assert_eq!(lines(dir.path(), "f.txt", &chunk).unwrap(), "two\n");
    }

    #[test]
    fn diff_handles_empty_sides() {
        let files = vec![fm("a.rs", 10, 100)];
        assert_eq!(diff(&[], &files, "").added, ["a.rs"], "everything is new");
        assert_eq!(diff(&files, &[], "").deleted, [0], "everything is gone");
        assert!(diff(&[], &[], "").is_empty());
    }
}

/// What changed between an index's file table and the tree as it is now.
///
/// Paths are index-root-relative (the form the file table stores), so both
/// callers can look files up and read them without further translation.
/// Split out of the two places that used to compute it inline — whole-index
/// staleness counting and scoped read-repair — which had drifted into two
/// implementations of one predicate, neither testable without a filesystem.
#[derive(Debug, Default, PartialEq)]
pub struct Diff {
    /// Live files the index has never seen.
    pub added: Vec<String>,
    /// (file_id, path) for files the index has, whose size or mtime moved.
    pub modified: Vec<(u32, String)>,
    /// file_ids the index has that are no longer on disk.
    pub deleted: Vec<u32>,
}

impl Diff {
    /// Files that need reading to bring an index up to date.
    pub fn stale_paths(&self) -> impl Iterator<Item = &String> {
        self.added.iter().chain(self.modified.iter().map(|(_, p)| p))
    }

    /// Base file_ids whose indexed content must not be served.
    pub fn tombstones(&self) -> impl Iterator<Item = u32> + '_ {
        self.modified.iter().map(|&(id, _)| id).chain(self.deleted.iter().copied())
    }

    /// Number of drifted files, each counted once.
    pub fn len(&self) -> usize {
        self.added.len() + self.modified.len() + self.deleted.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Diff `live` (walked under `prefix`, so its paths are prefix-relative)
/// against `indexed` (the whole file table, index-root-relative).
///
/// `prefix` scopes the comparison: only indexed files under it participate, so
/// a query against a subdirectory does not see the rest of the corpus as
/// deleted. `""` compares the whole tree.
///
/// A file counts as modified when size or mtime differs. That is cheap and
/// wrong only in the direction that matters least: a rewrite preserving both is
/// missed, which no content-free check can catch.
pub fn diff(indexed: &[FileMeta], live: &[FileMeta], prefix: &str) -> Diff {
    let under_prefix = |path: &str| {
        prefix.is_empty() || path.strip_prefix(prefix).is_some_and(|r| r.starts_with('/'))
    };
    let mut known: std::collections::HashMap<&str, (u32, u64, u64)> = indexed
        .iter()
        .enumerate()
        .filter(|(_, f)| under_prefix(&f.path))
        .map(|(id, f)| (f.path.as_str(), (id as u32, f.size, f.mtime)))
        .collect();

    let mut out = Diff::default();
    for f in live {
        let path =
            if prefix.is_empty() { f.path.clone() } else { format!("{}/{}", prefix, f.path) };
        match known.remove(path.as_str()) {
            Some((_, size, mtime)) if size == f.size && mtime == f.mtime => {}
            Some((id, _, _)) => out.modified.push((id, path)),
            None => out.added.push(path),
        }
    }
    // Whatever the index still claims under this scope is gone.
    out.deleted.extend(known.into_values().map(|(id, _, _)| id));

    // Deterministic order: `known` is a HashMap, and callers embed these ids in
    // chunk ids and rank order.
    out.added.sort_unstable();
    out.modified.sort_unstable();
    out.deleted.sort_unstable();
    out
}
