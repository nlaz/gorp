//! End-to-end tests: fixture corpus → all four modes, unindexed and indexed
//! (with and without HNSW), verifying parity between the two paths, plus the
//! cache behaviors: write-through, ancestor discovery, scope promotion, and
//! the read-repair overlay (cache-transparency invariant).

use semgrep_core::ChunkParams;
use semgrep_core::index::{self, BuildOptions};
use semgrep_core::search::{Mode, SearchOptions, search};
use std::fs;
use std::path::Path;

/// Point the cache at a per-process tempdir (never the user's real cache)
/// and make read-repair validation unthrottled. Called first in every test
/// that runs `search()`; `cache_base()` resolves env once per process, so
/// this must win the race — OnceLock synchronizes callers.
///
/// The returned guard is what keeps the suite honest. One cache directory is
/// shared by every test in this binary (it has to be: the base is resolved from
/// the environment exactly once per process), so a test that evicts entries
/// operates on *everyone's* state. `shared()` tests hold a read lock and run in
/// parallel; `exclusive()` tests — anything that prunes or clears — hold a write
/// lock and run alone. Without this the suite failed intermittently, and the
/// failure looked like a repair bug rather than a test-isolation bug.
fn isolate_cache() -> std::sync::RwLockReadGuard<'static, ()> {
    cache_lock().read().unwrap()
}

/// For tests that delete cache entries they do not own.
fn isolate_cache_exclusive() -> std::sync::RwLockWriteGuard<'static, ()> {
    cache_lock().write().unwrap()
}

fn cache_lock() -> &'static std::sync::RwLock<()> {
    static LOCK: std::sync::OnceLock<std::sync::RwLock<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("SEMGREP_CACHE_DIR", dir.path());
            std::env::set_var("SEMGREP_CACHE_TTL_SECS", "0");
        }
        // Leak: the cache dir must outlive every test in the process.
        std::mem::forget(dir);
        std::sync::RwLock::new(())
    })
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

    index::build(
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
    index::build(
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
    index::build(
        dir.path(),
        &BuildOptions { params, hnsw: false, ..Default::default() },
        |_, _| {},
    )
    .unwrap();

    let idx = index::LoadedIndex::load(dir.path(), index::LoadNeeds::all()).unwrap();
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
    index::build(
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

// ---------------------------------------------------------------------------
// cache behaviors (RESEARCH.md §8/8.1)
// ---------------------------------------------------------------------------

#[test]
fn ancestor_index_serves_subdir_scope() {
    let _cache = isolate_cache();
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let params = ChunkParams { window: 8, overlap: 2, ..Default::default() };
    index::build(
        dir.path(),
        &BuildOptions { params, hnsw: false, ..Default::default() },
        |_, _| {},
    )
    .unwrap();

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
    let _cache = isolate_cache();
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
    let _cache = isolate_cache();
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
    let _cache = isolate_cache();
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
    let r =
        search(dir.path(), "superconducting qubits coherence", &opts(Mode::Hybrid)).unwrap();
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

/// A cache entry this binary cannot read (older format version, or a
/// different embedding table's dims) must degrade to a miss and re-fill —
/// never surface an error. The cache is memoization; it is not the caller's
/// problem. Regression test for the dim-256 rollout, which made every
/// pre-existing 512-dim cache entry error on every search.
#[test]
fn unreadable_cache_entry_degrades_to_a_miss() {
    let _cache = isolate_cache();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.py"), "def hello_world():\n    return 1\n").unwrap();

    // Warm the cache, then corrupt the entry's dims the way an upgrade would.
    let opts = semgrep_core::search::SearchOptions::default();
    semgrep_core::search::search(dir.path(), "greeting", &opts).unwrap();
    // Only this test's own entry — the cache dir is shared with other tests
    // running in parallel, so corrupting all of them clobbers their state.
    let mine = std::fs::canonicalize(dir.path()).unwrap();
    let entries: Vec<_> = semgrep_core::index::cache_entries()
        .into_iter()
        .filter(|(_, root)| *root == mine)
        .collect();
    assert_eq!(entries.len(), 1, "expected exactly one entry for this scope");
    for (d, _) in &entries {
        let p = d.join("meta.json");
        let mut m: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        m["dims"] = serde_json::json!(semgrep_core::EMBED_DIM + 1);
        std::fs::write(&p, serde_json::to_vec(&m).unwrap()).unwrap();
    }

    // Must still answer, not error.
    let res = semgrep_core::search::search(dir.path(), "greeting", &opts)
        .expect("stale cache entry must not surface as an error");
    assert!(
        res.hits.iter().any(|h| h.path.ends_with("a.py")),
        "expected the query to be answered after evicting the stale entry"
    );
}

/// Cache entries live under a generation directory keyed to this binary's
/// index format, dims, and embedding-table fingerprint. An entry written by
/// an incompatible binary sorts into a sibling directory and is therefore
/// never discovered — the failure mode is "not found", not "error".
#[test]
fn cache_entries_are_namespaced_by_compat_generation() {
    let _cache = isolate_cache();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.py"), "def parse_config():\n    return {}\n").unwrap();
    let opts = semgrep_core::search::SearchOptions::default();
    semgrep_core::search::search(dir.path(), "configuration parsing", &opts).unwrap();

    let gen_dir = semgrep_core::index::cache_generation();
    let key = semgrep_core::index::compat_key();
    assert!(gen_dir.ends_with(&key), "generation dir should be the compat key");
    assert!(
        semgrep_core::index::cache_entries().iter().all(|(d, _)| d.starts_with(&gen_dir)),
        "every entry must live under the current generation"
    );

    // An entry from another generation is invisible, not an error.
    let alien = semgrep_core::index::cache_base().join("v2-d999-deadbeefdeadbeef");
    std::fs::create_dir_all(alien.join("abc")).unwrap();
    std::fs::write(alien.join("abc/meta.json"), b"{}").unwrap();
    std::fs::write(alien.join("abc/root.txt"), dir.path().to_string_lossy().as_bytes())
        .unwrap();
    assert!(
        semgrep_core::index::cache_entries().iter().all(|(d, _)| !d.starts_with(&alien)),
        "entries from another generation must not be discovered"
    );

    // ...and a write reclaims it, along with pre-generation flat entries
    // left by older builds (identified by holding meta.json directly).
    let legacy = semgrep_core::index::cache_base().join("deadbeef00000000");
    std::fs::create_dir_all(&legacy).unwrap();
    std::fs::write(legacy.join("meta.json"), b"{}").unwrap();
    std::fs::write(legacy.join("root.txt"), b"/tmp").unwrap();
    let unrelated = semgrep_core::index::cache_base().join("someones-other-data");
    std::fs::create_dir_all(&unrelated).unwrap();

    semgrep_core::index::gc_old_generations();
    assert!(!alien.exists(), "stale generation should be garbage-collected");
    assert!(!legacy.exists(), "pre-generation flat entry should be reclaimed");
    assert!(unrelated.exists(), "unrelated directories must be left alone");
}

/// Corruption, not just a metadata mismatch: a half-written entry must also
/// degrade to a miss. `emb.bin` is removed after the entry is warm.
#[test]
fn corrupt_cache_entry_degrades_to_a_miss() {
    let _cache = isolate_cache();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.py"), "def retry_backoff():\n    return 2\n").unwrap();
    let opts = semgrep_core::search::SearchOptions::default();
    semgrep_core::search::search(dir.path(), "backoff", &opts).unwrap();

    let mine = std::fs::canonicalize(dir.path()).unwrap();
    let (entry, _) = semgrep_core::index::cache_entries()
        .into_iter()
        .find(|(_, root)| *root == mine)
        .expect("entry for this scope");
    std::fs::remove_file(entry.join("emb.bin")).unwrap();

    let res = semgrep_core::search::search(dir.path(), "backoff", &opts)
        .expect("a corrupt cache entry must not surface as an error");
    assert!(res.hits.iter().any(|h| h.path.ends_with("a.py")), "expected an answer");
}

/// A cache with no ceiling is a slow disk leak: the kernel corpus alone
/// indexes to ~946 MB. Entries whose repo is gone are dead weight forever,
/// and past the budget the least-recently-used must go.
///
/// Exclusive: eviction deletes entries belonging to every other test in this
/// binary, so it cannot run alongside them.
#[test]
fn cache_prunes_dead_entries_and_enforces_a_budget() {
    let _cache = isolate_cache_exclusive();
    let opts = semgrep_core::search::SearchOptions::default();

    // Two scopes; one of them we then delete off disk.
    let keep = tempfile::tempdir().unwrap();
    std::fs::write(keep.path().join("a.py"), "def keeper():\n    return 1\n").unwrap();
    let doomed = tempfile::tempdir().unwrap();
    std::fs::write(doomed.path().join("b.py"), "def doomed():\n    return 2\n").unwrap();
    semgrep_core::search::search(keep.path(), "keeper", &opts).unwrap();
    semgrep_core::search::search(doomed.path(), "doomed", &opts).unwrap();

    let doomed_root = std::fs::canonicalize(doomed.path()).unwrap();
    let keep_root = std::fs::canonicalize(keep.path()).unwrap();
    assert!(semgrep_core::index::cache_status().iter().any(|e| e.root == doomed_root));

    // The repo goes away; its entry can never be useful again.
    drop(doomed);
    let (n, _freed) = semgrep_core::index::enforce_budget();
    assert!(n >= 1, "expected the dead entry to be reclaimed");
    let after = semgrep_core::index::cache_status();
    assert!(!after.iter().any(|e| e.root == doomed_root), "dead entry survived");
    assert!(after.iter().any(|e| e.root == keep_root), "live entry was evicted");

    // With a budget of zero, even a live entry must be evicted. Passing the cap
    // explicitly rather than through the environment: `cache_max_bytes` is read
    // per call, so mutating it here would leak into whatever else is running.
    semgrep_core::index::enforce_budget_with_cap(0);
    assert!(
        semgrep_core::index::cache_status().is_empty(),
        "a zero budget should evict everything"
    );
}
