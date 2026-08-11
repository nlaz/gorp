//! Tests for the corpus layer, kept in one place: they share the `FileMeta`
//! helper and most exercise walk, chunk, and pass in combination.

use super::*;
use crate::{Chunk, ChunkParams, FileMeta};

#[test]
fn chunking_covers_all_lines_with_overlap() {
    let text = (1..=100).map(|i| format!("line {i}\n")).collect::<String>();
    let params = ChunkParams { window: 32, overlap: 8, ..Default::default() };
    let chunks = chunk_lines(0, "x.rs", &text, &params);
    assert_eq!(chunks[0].0.start_line, 1);
    assert_eq!(chunks[0].0.end_line, 32);
    assert_eq!(chunks[1].0.start_line, 25);
    // stride 24: starts at 1, 25, 49, 73; the last window absorbs the tail
    assert_eq!(chunks.len(), 4);
    assert_eq!(chunks.last().unwrap().0.start_line, 73);
    assert_eq!(chunks.last().unwrap().0.end_line, 100);
}

#[test]
fn with_window_holds_the_overlap_share_not_the_line_count() {
    // Overlap stops a match being split across a boundary, which is a property
    // of the window it sits in. Carrying the default 8 lines onto a 12-line
    // window would overlap by two thirds — over `hit::finalize`'s 0.5
    // near-duplicate threshold, so every neighbour would collapse and §24's
    // finer file-scope pass would return fewer results rather than more.
    let p = ChunkParams::default().with_window(12);
    assert_eq!(p.window, 12);
    assert_eq!(p.overlap, 3, "8/32 of 12 rounds to 3, a quarter as before");
    assert!((p.overlap as f32 / p.window as f32) < 0.5, "must stay under the dedupe threshold");
    // A budget measures the same thing in a different unit and would silently
    // win over the window we just set.
    assert_eq!(ChunkParams { budget: Some(900), ..Default::default() }.with_window(12).budget, None);
}

#[test]
fn short_file_is_one_chunk() {
    let chunks = chunk_lines(0, "x.rs", "a\nb\n", &ChunkParams::default());
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].1, "a\nb\n");
}

#[test]
fn empty_and_blank_files_yield_nothing() {
    assert!(chunk_lines(0, "x.rs", "", &ChunkParams::default()).is_empty());
    assert!(chunk_lines(0, "x.rs", "\n\n  \n", &ChunkParams::default()).is_empty());
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

// ---------------------------------------------------------------------------
// The walk is parallel; the sort is what keeps it usable as an id assignment.
// ---------------------------------------------------------------------------

/// A tree wide and deep enough that several walker threads are genuinely
/// interleaving, with gitignored and hidden entries mixed in.
fn walk_fixture(root: &std::path::Path) {
    // `.ignore`, not `.gitignore`: the `ignore` crate only honours gitignore
    // rules inside an actual git repository, so a `.gitignore` in a bare
    // tempdir does nothing. `.ignore` is always respected.
    std::fs::write(root.join(".ignore"), "ignored/\n*.log\n").unwrap();
    std::fs::create_dir_all(root.join("ignored")).unwrap();
    std::fs::write(root.join("ignored/nope.rs"), "fn ignored() {}\n").unwrap();
    std::fs::write(root.join("build.log"), "noise\n").unwrap();
    std::fs::create_dir_all(root.join(".hidden")).unwrap();
    std::fs::write(root.join(".hidden/secret.rs"), "fn hidden() {}\n").unwrap();
    // A repo-local index and the two transient directories a build swaps
    // through: none of them are corpus.
    for d in [".semgrep", ".semgrep.building-1234", ".semgrep.trash-1234"] {
        std::fs::create_dir_all(root.join(d)).unwrap();
        std::fs::write(root.join(d).join("meta.json"), "{}").unwrap();
    }
    for a in 0..8 {
        for b in 0..8 {
            let dir = root.join(format!("m{a}/s{b}"));
            std::fs::create_dir_all(&dir).unwrap();
            for c in 0..4 {
                std::fs::write(
                    dir.join(format!("f{c}.rs")),
                    format!("pub fn symbol_{a}_{b}_{c}() -> u32 {{ {a}{b}{c} }}\n"),
                )
                .unwrap();
            }
        }
    }
}

/// Walk order *is* chunk-id order, and chunk ids must agree across the chunk
/// table, the BM25 add order and the `emb.bin` rows. Traversal is concurrent, so
/// nothing about the order in which directories are read is stable — the sort at
/// the end is the entire guarantee. Twenty walks, byte-identical.
#[test]
fn the_walk_is_deterministic_however_the_threads_interleave() {
    let dir = tempfile::tempdir().unwrap();
    walk_fixture(dir.path());
    let params = ChunkParams::default();

    let first = walk(dir.path(), &params).unwrap();
    assert_eq!(first.len(), 8 * 8 * 4, "the fixture's real files, and only those");
    for i in 1..20 {
        assert_eq!(walk(dir.path(), &params).unwrap(), first, "walk {i} disagreed with walk 0");
    }
}

#[test]
fn the_walk_returns_a_totally_ordered_unique_list() {
    let dir = tempfile::tempdir().unwrap();
    walk_fixture(dir.path());
    let files = walk(dir.path(), &ChunkParams::default()).unwrap();

    let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
    let mut sorted = paths.clone();
    sorted.sort_unstable();
    assert_eq!(paths, sorted, "walk order must be sorted by path");
    let unique: std::collections::HashSet<&&str> = paths.iter().collect();
    assert_eq!(unique.len(), paths.len(), "a file must be visited exactly once");
}

/// What the walk must not pick up: gitignored paths, hidden directories, the
/// repo-local index, and the staging/trash siblings a publish swaps through.
#[test]
fn the_walk_skips_ignored_hidden_and_index_directories() {
    let dir = tempfile::tempdir().unwrap();
    walk_fixture(dir.path());
    let files = walk(dir.path(), &ChunkParams::default()).unwrap();

    for unwanted in ["ignored/", "build.log", ".hidden", ".semgrep"] {
        assert!(
            !files.iter().any(|f| f.path.starts_with(unwanted)),
            "{unwanted:?} should not be corpus: {:?}",
            files.iter().map(|f| &f.path).take(5).collect::<Vec<_>>()
        );
    }
}

/// The size cap still applies, and applies per file rather than to the tree.
#[test]
fn the_walk_honours_the_size_cap() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("small.rs"), "fn a() {}\n").unwrap();
    std::fs::write(dir.path().join("big.rs"), "x".repeat(4096)).unwrap();
    let params = ChunkParams { max_file_bytes: 1024, ..Default::default() };
    let files = walk(dir.path(), &params).unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].path, "small.rs");
}
