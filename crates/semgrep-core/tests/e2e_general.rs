//! End-to-end: the fixture corpus through all four modes, unindexed and
//! indexed (with and without HNSW), and the parity between the two paths.

mod common;
use semgrep_core::ChunkParams;
use semgrep_core::search::{Mode, search};
use semgrep_core::store::{self, BuildOptions};
use std::fs;

use common::*;

#[test]
fn keyword_mode_is_grep() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let r = search(dir.path(), r"fn \w+_token", &opts(Mode::Keyword)).unwrap();
    assert_eq!(r.hits.len(), 1);
    assert_eq!(r.hits[0].path, "src/auth.rs");
    assert!(r.hits[0].text.contains("validate_session_token"));
}


#[test]
fn bm25_unindexed_finds_identifier_from_nl_query() {
    let _cache = isolate_cache();
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let r = search(dir.path(), "compute the backoff delay", &stream_opts(Mode::Bm25)).unwrap();
    assert!(!r.report.used_index);
    assert_eq!(r.hits[0].path, "src/retry.rs");
    assert!(r.hits[0].text.contains("compute_backoff_delay"));
}

#[test]
fn semantic_unindexed_beats_keywords() {
    let _cache = isolate_cache();
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    // No lexical overlap with "sourdough"/"ferment": paraphrase only.
    let r = search(
        dir.path(),
        "baking bread with a fermented starter",
        &stream_opts(Mode::Semantic),
    )
    .unwrap();
    assert_eq!(r.hits[0].path, "docs/cooking.md");
}

#[test]
fn hybrid_unindexed_ranks_target_first() {
    let _cache = isolate_cache();
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let r = search(
        dir.path(),
        "check whether a session token is valid",
        &stream_opts(Mode::Hybrid),
    )
    .unwrap();
    assert_eq!(r.hits[0].path, "src/auth.rs");
}

#[test]
fn indexed_matches_unindexed_results() {
    let _cache = isolate_cache();
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let params = ChunkParams { window: 8, overlap: 2, ..Default::default() };

    let cold =
        search(dir.path(), "exponential backoff retries", &stream_opts(Mode::Hybrid)).unwrap();
    assert!(!cold.report.used_index);

    store::build(
        dir.path(),
        &BuildOptions { params, hnsw: false, ..Default::default() },
        |_, _| {},
    )
    .unwrap();
    let warm = search(dir.path(), "exponential backoff retries", &opts(Mode::Hybrid)).unwrap();
    assert!(warm.report.used_index);
    assert!(!warm.report.used_hnsw);

    assert_eq!(cold.hits[0].path, warm.hits[0].path);
    assert_eq!(cold.hits[0].start_line, warm.hits[0].start_line);

    // exact (brute-force) indexed semantic must agree with streaming semantic
    let cold_sem =
        search(dir.path(), "mirrors gathering light from stars", &stream_opts(Mode::Semantic))
            .unwrap();
    let warm_sem =
        search(dir.path(), "mirrors gathering light from stars", &opts(Mode::Semantic))
            .unwrap();
    assert_eq!(cold_sem.hits[0].path, warm_sem.hits[0].path);
    assert_eq!(warm_sem.hits[0].path, "docs/astronomy.md");
}

#[test]
fn hnsw_index_agrees_with_exact_on_top_hit() {
    let _cache = isolate_cache();
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let params = ChunkParams { window: 8, overlap: 2, ..Default::default() };
    store::build(
        dir.path(),
        &BuildOptions { params, hnsw: true, ..Default::default() },
        |_, _| {},
    )
    .unwrap();

    let mut o = opts(Mode::Semantic);
    let hnsw = search(dir.path(), "hashing a password with salt", &o).unwrap();
    assert!(hnsw.report.used_hnsw);
    o.use_hnsw = false;
    let exact = search(dir.path(), "hashing a password with salt", &o).unwrap();
    assert!(!exact.report.used_hnsw);
    assert_eq!(hnsw.hits[0].path, exact.hits[0].path);
    assert_eq!(exact.hits[0].path, "src/auth.rs");
}

#[test]
fn staleness_detected_after_edit() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let params = ChunkParams { window: 8, overlap: 2, ..Default::default() };
    store::build(
        dir.path(),
        &BuildOptions { params, hnsw: false, ..Default::default() },
        |_, _| {},
    )
    .unwrap();

    let idx = store::LoadedIndex::load(dir.path(), store::LoadNeeds::all()).unwrap();
    assert_eq!(idx.stale_files().unwrap(), 0);

    fs::write(dir.path().join("src/new_file.rs"), "pub fn brand_new() {}\n").unwrap();
    assert_eq!(idx.stale_files().unwrap(), 1);
}

#[test]
fn no_index_flag_forces_streaming() {
    let _cache = isolate_cache();
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let params = ChunkParams { window: 8, overlap: 2, ..Default::default() };
    store::build(
        dir.path(),
        &BuildOptions { params, hnsw: false, ..Default::default() },
        |_, _| {},
    )
    .unwrap();
    let mut o = opts(Mode::Bm25);
    o.no_index = true;
    let r = search(dir.path(), "backoff", &o).unwrap();
    assert!(!r.report.used_index);
    assert_eq!(r.hits[0].path, "src/retry.rs");
}
