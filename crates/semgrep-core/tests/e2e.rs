//! End-to-end tests: fixture corpus → all four modes, unindexed and indexed
//! (with and without HNSW), verifying parity between the two paths, plus the
//! cache behaviors: write-through, ancestor discovery, scope promotion, and
//! the read-repair overlay (cache-transparency invariant).

use semgrep_core::index::{self, BuildOptions};
use semgrep_core::search::{Mode, SearchOptions, search};
use semgrep_core::ChunkParams;
use std::fs;
use std::path::Path;

/// Point the cache at a per-process tempdir (never the user's real cache)
/// and make read-repair validation unthrottled. Called first in every test
/// that runs `search()`; `cache_base()` resolves env once per process, so
/// this must win the race — OnceLock synchronizes callers.
fn isolate_cache() {
    static INIT: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    INIT.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("SEMGREP_CACHE_DIR", dir.path());
            std::env::set_var("SEMGREP_CACHE_TTL_SECS", "0");
        }
        // Leak: the cache dir must outlive every test in the process.
        std::mem::forget(dir);
    });
}

/// Small corpus with clearly separated topics so retrieval is unambiguous.
fn fixture(dir: &Path) {
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::create_dir_all(dir.join("docs")).unwrap();
    fs::write(
        dir.join("src/retry.rs"),
        r#"//! Retry logic with exponential backoff.

pub fn compute_backoff_delay(attempt: u32, base_ms: u64) -> u64 {
    let exp = base_ms.saturating_mul(2u64.saturating_pow(attempt));
    exp.min(30_000)
}

pub fn should_retry(status: u16, attempt: u32) -> bool {
    attempt < 5 && (status == 429 || status >= 500)
}
"#,
    )
    .unwrap();
    fs::write(
        dir.join("src/auth.rs"),
        r#"//! Session token validation.

pub fn validate_session_token(token: &str) -> bool {
    !token.is_empty() && token.len() == 64 && token.chars().all(|c| c.is_ascii_hexdigit())
}

pub fn hash_password(password: &str, salt: &[u8]) -> Vec<u8> {
    // pretend this is argon2
    password.bytes().chain(salt.iter().copied()).collect()
}
"#,
    )
    .unwrap();
    fs::write(
        dir.join("docs/cooking.md"),
        "# Sourdough bread\n\nMix flour and water, let the starter ferment overnight.\nKnead the dough and bake at high temperature.\n",
    )
    .unwrap();
    fs::write(
        dir.join("docs/astronomy.md"),
        "# Telescopes\n\nA reflecting telescope uses mirrors to gather starlight.\nGalileo pioneered astronomical observation with refractors.\n",
    )
    .unwrap();
}

fn opts(mode: Mode) -> SearchOptions {
    SearchOptions {
        mode,
        k: 3,
        // small windows so each file yields at least one chunk quickly
        params: ChunkParams { window: 8, overlap: 2, ..Default::default() },
        ..Default::default()
    }
}

#[test]
fn keyword_mode_is_grep() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let r = search(dir.path(), r"fn \w+_token", &opts(Mode::Keyword)).unwrap();
    assert_eq!(r.hits.len(), 1);
    assert_eq!(r.hits[0].path, "src/auth.rs");
    assert!(r.hits[0].text.contains("validate_session_token"));
}

/// The pure streaming path (no index anywhere, none written).
fn stream_opts(mode: Mode) -> SearchOptions {
    SearchOptions { no_index: true, ..opts(mode) }
}

#[test]
fn bm25_unindexed_finds_identifier_from_nl_query() {
    isolate_cache();
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let r = search(dir.path(), "compute the backoff delay", &stream_opts(Mode::Bm25)).unwrap();
    assert!(!r.report.used_index);
    assert_eq!(r.hits[0].path, "src/retry.rs");
    assert!(r.hits[0].text.contains("compute_backoff_delay"));
}

#[test]
fn semantic_unindexed_beats_keywords() {
    isolate_cache();
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    // No lexical overlap with "sourdough"/"ferment": paraphrase only.
    let r = search(dir.path(), "baking bread with a fermented starter", &stream_opts(Mode::Semantic))
        .unwrap();
    assert_eq!(r.hits[0].path, "docs/cooking.md");
}

#[test]
fn hybrid_unindexed_ranks_target_first() {
    isolate_cache();
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let r = search(dir.path(), "check whether a session token is valid", &stream_opts(Mode::Hybrid))
        .unwrap();
    assert_eq!(r.hits[0].path, "src/auth.rs");
}

#[test]
fn indexed_matches_unindexed_results() {
    isolate_cache();
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let params = ChunkParams { window: 8, overlap: 2, ..Default::default() };

    let cold = search(dir.path(), "exponential backoff retries", &stream_opts(Mode::Hybrid)).unwrap();
    assert!(!cold.report.used_index);

    index::build(dir.path(), &BuildOptions { params, hnsw: false, sif: false }, |_, _| {}).unwrap();
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
        search(dir.path(), "mirrors gathering light from stars", &opts(Mode::Semantic)).unwrap();
    assert_eq!(cold_sem.hits[0].path, warm_sem.hits[0].path);
    assert_eq!(warm_sem.hits[0].path, "docs/astronomy.md");
}

#[test]
fn hnsw_index_agrees_with_exact_on_top_hit() {
    isolate_cache();
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let params = ChunkParams { window: 8, overlap: 2, ..Default::default() };
    index::build(dir.path(), &BuildOptions { params, hnsw: true, sif: false }, |_, _| {}).unwrap();

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
    index::build(dir.path(), &BuildOptions { params, hnsw: false, sif: false }, |_, _| {}).unwrap();

    let idx = index::LoadedIndex::load(dir.path(), index::LoadNeeds::all()).unwrap();
    assert_eq!(idx.stale_files().unwrap(), 0);

    fs::write(dir.path().join("src/new_file.rs"), "pub fn brand_new() {}\n").unwrap();
    assert_eq!(idx.stale_files().unwrap(), 1);
}

#[test]
fn no_index_flag_forces_streaming() {
    isolate_cache();
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let params = ChunkParams { window: 8, overlap: 2, ..Default::default() };
    index::build(dir.path(), &BuildOptions { params, hnsw: false, sif: false }, |_, _| {}).unwrap();
    let mut o = opts(Mode::Bm25);
    o.no_index = true;
    let r = search(dir.path(), "backoff", &o).unwrap();
    assert!(!r.report.used_index);
    assert_eq!(r.hits[0].path, "src/retry.rs");
}

// ---------------------------------------------------------------------------
// cache behaviors (RESEARCH.md §8/8.1)
// ---------------------------------------------------------------------------

#[test]
fn ancestor_index_serves_subdir_scope() {
    isolate_cache();
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let params = ChunkParams { window: 8, overlap: 2, ..Default::default() };
    index::build(dir.path(), &BuildOptions { params, hnsw: false, sif: false }, |_, _| {}).unwrap();

    // Query scoped to src/ — the index lives one level up.
    let r = search(&dir.path().join("src"), "validate a session token", &opts(Mode::Hybrid))
        .unwrap();
    assert!(r.report.used_index, "subdir scope should discover the root index");
    assert!(!r.report.wrote_cache, "must reuse, not rebuild");
    // Paths display relative to the queried scope (grep's contract).
    assert_eq!(r.hits[0].path, "auth.rs");
    // Out-of-scope files (docs/) must never appear.
    assert!(r.hits.iter().all(|h| !h.path.contains("docs/")));
}

#[test]
fn write_through_is_transparent() {
    isolate_cache();
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let q = "exponential backoff for retries";

    let cold = search(dir.path(), q, &stream_opts(Mode::Hybrid)).unwrap();
    let auto = search(dir.path(), q, &opts(Mode::Hybrid)).unwrap();
    assert!(auto.report.wrote_cache, "first ranked search should cache its scope");
    assert!(auto.report.used_index);
    let warm = search(dir.path(), q, &opts(Mode::Hybrid)).unwrap();
    assert!(!warm.report.wrote_cache, "second search reuses the entry");
    assert!(warm.report.used_index);

    // Cache-transparency invariant: identical results cold, freshly cached,
    // and warm.
    for other in [&auto, &warm] {
        assert_eq!(cold.hits[0].path, other.hits[0].path);
        assert_eq!(cold.hits[0].start_line, other.hits[0].start_line);
    }
}

#[test]
fn scope_promotion_evicts_child_entries() {
    isolate_cache();
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let canon = fs::canonicalize(dir.path()).unwrap();

    // Subdir first: creates an entry rooted at src/ (cost ∝ scope).
    let r = search(&dir.path().join("src"), "hash a password", &opts(Mode::Hybrid)).unwrap();
    assert!(r.report.wrote_cache);
    let roots = |base: &Path| -> Vec<std::path::PathBuf> {
        index::cache_entries()
            .into_iter()
            .map(|(_, root)| root)
            .filter(|r| r.starts_with(base))
            .collect()
    };
    assert_eq!(roots(&canon), vec![canon.join("src")]);

    // Widening to the repo root promotes: root entry built, child evicted.
    let r = search(dir.path(), "hash a password", &opts(Mode::Hybrid)).unwrap();
    assert!(r.report.wrote_cache);
    assert_eq!(roots(&canon), vec![canon.clone()]);

    // And the promoted entry serves the subdir scope warm.
    let r = search(&dir.path().join("src"), "hash a password", &opts(Mode::Hybrid)).unwrap();
    assert!(r.report.used_index && !r.report.wrote_cache);
    assert_eq!(r.hits[0].path, "auth.rs");
}

#[test]
fn read_repair_serves_current_tree() {
    isolate_cache();
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());

    // Warm the cache, then drift the tree: one new file, one rewritten.
    search(dir.path(), "retry backoff", &opts(Mode::Hybrid)).unwrap();
    fs::write(
        dir.path().join("docs/quantum.md"),
        "# Qubits\n\nSuperconducting qubits lose coherence to their environment.\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("src/retry.rs"),
        "//! Circuit breaking.\n\npub fn circuit_breaker_trip(failures: u32) -> bool {\n    failures > 3\n}\n",
    )
    .unwrap();

    // New file is found without any rebuild (lazy fill ≡ repair)…
    let r = search(dir.path(), "superconducting qubits coherence", &opts(Mode::Hybrid)).unwrap();
    assert!(r.report.used_index);
    assert!(r.report.stale_files > 0, "repair should report the drift");
    assert_eq!(r.hits[0].path, "docs/quantum.md");

    // …the rewritten file's new content ranks…
    let r = search(dir.path(), "circuit breaker tripping", &opts(Mode::Hybrid)).unwrap();
    assert_eq!(r.hits[0].path, "src/retry.rs");
    assert!(r.hits[0].text.contains("circuit_breaker_trip"));

    // …and its old content is tombstoned: no hit may show vanished text.
    let r = search(dir.path(), "compute the backoff delay", &opts(Mode::Hybrid)).unwrap();
    assert!(
        r.hits.iter().all(|h| !h.text.contains("compute_backoff_delay")),
        "stale chunk text must not be served"
    );
}
