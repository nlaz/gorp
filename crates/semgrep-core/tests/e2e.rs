//! End-to-end tests: fixture corpus → all four modes, unindexed and indexed
//! (with and without HNSW), verifying parity between the two paths, plus the
//! cache behaviors: write-through, ancestor discovery, scope promotion, and
//! the read-repair overlay (cache-transparency invariant).

use semgrep_core::ChunkParams;
use semgrep_core::cache;
use semgrep_core::cache::repair::RepairOutcome;
use semgrep_core::search::{Mode, SearchOptions, search};
use semgrep_core::store::{self, BuildOptions};
use std::fs;
use std::path::Path;

/// Take the cache for the duration of a test.
///
/// Every test in this binary shares one cache directory, and it cannot be
/// otherwise today: `cache::cache_base()` resolves `SEMGREP_CACHE_DIR` through
/// a `OnceLock`, so the whole process gets one cache no matter what a test
/// sets. Cache state is therefore global mutable state, and these tests are all
/// mutators — a write-through search creates entries, while scope promotion and
/// budget enforcement delete entries belonging to whoever else is running.
///
/// So they are serialized. A finer read/write split was tried first and did not
/// hold: assertions about *which* entries exist, or about whether repair fired,
/// fail whenever a concurrent test's write-through prunes or promotes. Those
/// failures read as engine bugs, which is the expensive kind of flake.
/// Serializing costs nothing measurable — the whole binary runs in ~0.06 s.
///
/// The real fix is to make the cache root an explicit parameter instead of
/// process-global state, the same move `enforce_budget_with_cap` made for the
/// cap. That belongs with the `cache/` module split.
///
/// Poison is ignored deliberately: the lock orders tests, it guards no data.
/// Letting it poison turns one real failure into a dozen `PoisonError` panics
/// that bury the original.
fn isolate_cache() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("SEMGREP_CACHE_DIR", dir.path());
            std::env::set_var("SEMGREP_CACHE_TTL_SECS", "0");
        }
        // Leak: the cache dir must outlive every test in the process.
        std::mem::forget(dir);
        std::sync::Mutex::new(())
    })
    .lock()
    .unwrap_or_else(|e| e.into_inner())
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

// ---------------------------------------------------------------------------
// cache behaviors (RESEARCH.md §8/8.1)
// ---------------------------------------------------------------------------

#[test]
fn ancestor_index_serves_subdir_scope() {
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
        cache::cache_entries()
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

    // The overlay, specifically — so the drift bound is off. Two files of six is
    // 33% drift, which on a real corpus means "rebuild, do not patch"; this test
    // is about what patching does, and `repair_serves_a_small_drift_and_rebuilds_a_large_one`
    // covers which of the two is chosen.
    let opts = |m| SearchOptions { repair_max_drift: 0.0, ..opts(m) };

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
    let entries: Vec<_> = semgrep_core::cache::cache_entries()
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

    let gen_dir = semgrep_core::cache::cache_generation();
    let key = semgrep_core::cache::compat_key();
    assert!(gen_dir.ends_with(&key), "generation dir should be the compat key");
    assert!(
        semgrep_core::cache::cache_entries().iter().all(|(d, _)| d.starts_with(&gen_dir)),
        "every entry must live under the current generation"
    );

    // An entry from another generation is invisible, not an error.
    let alien = semgrep_core::cache::cache_base().join("v2-d999-deadbeefdeadbeef");
    std::fs::create_dir_all(alien.join("abc")).unwrap();
    std::fs::write(alien.join("abc/meta.json"), b"{}").unwrap();
    std::fs::write(alien.join("abc/root.txt"), dir.path().to_string_lossy().as_bytes())
        .unwrap();
    assert!(
        semgrep_core::cache::cache_entries().iter().all(|(d, _)| !d.starts_with(&alien)),
        "entries from another generation must not be discovered"
    );

    // ...and a write reclaims it, along with pre-generation flat entries
    // left by older builds (identified by holding meta.json directly).
    let legacy = semgrep_core::cache::cache_base().join("deadbeef00000000");
    std::fs::create_dir_all(&legacy).unwrap();
    std::fs::write(legacy.join("meta.json"), b"{}").unwrap();
    std::fs::write(legacy.join("root.txt"), b"/tmp").unwrap();
    let unrelated = semgrep_core::cache::cache_base().join("someones-other-data");
    std::fs::create_dir_all(&unrelated).unwrap();

    semgrep_core::cache::gc_old_generations();
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
    let (entry, _) = semgrep_core::cache::cache_entries()
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
#[test]
fn cache_prunes_dead_entries_and_enforces_a_budget() {
    let _cache = isolate_cache();
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
    assert!(semgrep_core::cache::cache_status().iter().any(|e| e.root == doomed_root));

    // The repo goes away; its entry can never be useful again.
    drop(doomed);
    let r = semgrep_core::cache::enforce_budget();
    assert!(r.removed >= 1, "expected the dead entry to be reclaimed");
    assert!(r.stuck.is_empty(), "nothing should have resisted deletion: {:?}", r.stuck);
    let after = semgrep_core::cache::cache_status();
    assert!(!after.iter().any(|e| e.root == doomed_root), "dead entry survived");
    assert!(after.iter().any(|e| e.root == keep_root), "live entry was evicted");

    // With a budget of zero, even a live entry must be evicted. Passing the cap
    // explicitly rather than through the environment: `cache_max_bytes` is read
    // per call, so mutating it here would leak into whatever else is running.
    semgrep_core::cache::enforce_budget_with_cap(0, 0);
    assert!(
        semgrep_core::cache::cache_status().is_empty(),
        "a zero budget should evict everything"
    );
}

/// An interrupted first search — Ctrl-C during the initial index of a large
/// repo, which is exactly when people interrupt — used to leave a directory
/// with no `root.txt`. Every enumerator skips those, so `cache --status`
/// under-reported, `cache --prune` freed nothing, and the bytes were
/// unreclaimable for the life of the machine. `root.txt` is now written before
/// the build, which makes the entry countable and prunable while still not
/// discoverable (that needs `meta.json`).
#[test]
fn interrupted_build_leaves_a_reclaimable_entry() {
    let _cache = isolate_cache();
    let orphan = cache::cache_generation().join("orphan-halfbuilt");
    fs::create_dir_all(&orphan).unwrap();
    fs::write(orphan.join("root.txt"), "/nonexistent-but-registered").unwrap();
    fs::write(orphan.join("emb.bin"), vec![0u8; 4096]).unwrap();

    let seen = cache::cache_status();
    let mine = seen.iter().find(|e| e.dir == orphan).expect("half-built entry must be visible");
    assert!(mine.incomplete, "no meta.json means unpublished");
    assert!(mine.bytes >= 4096, "its bytes must count against the budget");

    // Not discoverable, though: an unpublished entry must never serve a query.
    assert!(
        !cache::cache_entries().iter().any(|(d, _)| *d == orphan),
        "an entry without meta.json must not be discoverable"
    );

    // And a prune frees it. `0` for the abandonment threshold: a real prune
    // waits ABANDONED_AFTER_SECS so it cannot delete a build in flight.
    let r = cache::enforce_budget_with_cap(cache::cache_max_bytes(), 0);
    assert!(r.removed >= 1 && r.freed >= 4096, "prune should reclaim the orphan");
    assert!(!orphan.exists(), "orphan survived the prune");
}

/// A young incomplete entry is a build happening right now, not garbage.
/// Reclaiming it would delete the directory a concurrent process is filling.
#[test]
fn a_build_in_flight_is_not_reclaimed() {
    let _cache = isolate_cache();
    // A root that exists, so `incomplete` is the only thing under test — a
    // missing root is separately (and correctly) grounds for reclamation.
    let repo = tempfile::tempdir().unwrap();
    let live = cache::cache_generation().join("build-in-flight");
    fs::create_dir_all(&live).unwrap();
    fs::write(live.join("root.txt"), repo.path().to_string_lossy().as_bytes()).unwrap();

    cache::enforce_budget_with_cap(cache::cache_max_bytes(), 600);
    assert!(live.exists(), "an entry younger than the threshold must be left alone");
    let _ = fs::remove_dir_all(&live);
}

/// `discover` keys on `meta.json`, so writing it first published an index that
/// could not yet be loaded: a concurrent reader found the entry, failed on the
/// chunks.bin that did not exist yet, and — a cache load failure being a miss —
/// deleted the directory the builder was still writing. meta.json is now
/// written last, and removed before a rebuild begins.
#[test]
fn an_index_is_invisible_until_it_is_complete() {
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

    let idx_dir = store::index_dir(dir.path());
    assert!(store::exists(dir.path()));

    // Every other artifact must already be there when meta.json appears.
    for artifact in ["chunks.bin", "bm25.flat", "emb.bin"] {
        assert!(idx_dir.join(artifact).is_file(), "{artifact} missing from a published index");
    }

    // Remove meta.json the way an interrupted rebuild leaves things: the
    // artifacts are present but the index is unpublished, so it is not found.
    fs::remove_file(idx_dir.join("meta.json")).unwrap();
    assert!(!store::exists(dir.path()), "an index without meta.json is not an index");
    let r = search(dir.path(), "session token validation", &opts(Mode::Hybrid)).unwrap();
    assert!(!r.hits.is_empty(), "an unpublished index must degrade to an answer, not an error");
}

/// A repo-local `.semgrep/` is a committed artifact. Read-repair used to touch
/// `last_check` inside it on every validation, so merely *searching* dirtied a
/// tracked directory. The marker now lives under the cache for repo-local
/// indexes; for cache entries it stays put, where it doubles as the LRU
/// access time.
#[test]
fn searching_does_not_write_into_a_repo_local_index() {
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

    let idx_dir = store::index_dir(dir.path());
    let fingerprint = || -> Vec<(String, u64, std::time::SystemTime)> {
        let mut out: Vec<_> = fs::read_dir(&idx_dir)
            .unwrap()
            .flatten()
            .map(|e| {
                let m = e.metadata().unwrap();
                (e.file_name().to_string_lossy().into_owned(), m.len(), m.modified().unwrap())
            })
            .collect();
        out.sort();
        out
    };

    let before = fingerprint();
    for query in ["session token validation", "backoff delay", "sourdough starter"] {
        search(dir.path(), query, &opts(Mode::Hybrid)).unwrap();
    }
    assert_eq!(before, fingerprint(), "a search must not modify a repo-local .semgrep/");
}

/// A cache entry is identified by its chunk parameters as well as its root.
///
/// It was not, and the consequence was user-visible: one search with a
/// non-default `--window` wrote an entry keyed only by path, and every later
/// search of that scope — including plain default ones — was served from it,
/// silently returning spans of the wrong size. The eval harness sweeps window to
/// measure chunking, against the same cache ordinary use has, so a tuning run
/// contaminated whatever was measured next.
#[test]
fn chunk_params_are_part_of_a_cache_entry_identity() {
    let _cache = isolate_cache();
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let query = "compute the backoff delay";

    // Fine rerank off: this test reads the hit span as a proxy for chunk
    // geometry, and the fine window narrows every span to a few lines
    // regardless of the chunk behind it (§28.2).
    let narrow = SearchOptions {
        mode: Mode::Hybrid,
        k: 3,
        fine_rerank: false,
        params: ChunkParams { window: 8, overlap: 2, ..Default::default() },
        ..Default::default()
    };
    let wide = SearchOptions {
        mode: Mode::Hybrid,
        k: 3,
        fine_rerank: false,
        params: ChunkParams { window: 32, overlap: 8, ..Default::default() },
        ..Default::default()
    };

    let a = search(dir.path(), query, &narrow).unwrap();
    assert!(a.report.wrote_cache, "first search of this scope should cache it");
    let a_span = a.hits[0].end_line - a.hits[0].start_line;

    // The wide search must not be served the narrow entry.
    let b = search(dir.path(), query, &wide).unwrap();
    assert!(b.report.wrote_cache, "different params must miss, not reuse");
    let b_span = b.hits[0].end_line - b.hits[0].start_line;
    assert!(
        b_span > a_span,
        "window 32 should give wider spans than window 8 ({b_span} vs {a_span})"
    );

    // Both entries now coexist, and each keeps answering its own question.
    let a2 = search(dir.path(), query, &narrow).unwrap();
    assert!(!a2.report.wrote_cache, "the narrow entry should still be warm");
    assert_eq!(a2.hits[0].end_line - a2.hits[0].start_line, a_span, "narrow entry drifted");

    let b2 = search(dir.path(), query, &wide).unwrap();
    assert!(!b2.report.wrote_cache, "the wide entry should still be warm");
    assert_eq!(b2.hits[0].end_line - b2.hits[0].start_line, b_span, "wide entry drifted");
}

/// Scope promotion retires narrower entries, but only ones built the same way —
/// otherwise widening the scope at one window would silently delete the entry
/// another window's queries depend on.
#[test]
fn scope_promotion_spares_entries_built_with_other_params() {
    let _cache = isolate_cache();
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let canon = fs::canonicalize(dir.path()).unwrap();
    let opts_for = |window: u32| SearchOptions {
        mode: Mode::Hybrid,
        k: 3,
        params: ChunkParams { window, overlap: 2, ..Default::default() },
        ..Default::default()
    };

    // A narrow-window entry rooted at src/.
    search(&dir.path().join("src"), "hash a password", &opts_for(8)).unwrap();
    // Then a *different* window over the whole root, which promotes.
    search(dir.path(), "hash a password", &opts_for(16)).unwrap();

    let roots: Vec<std::path::PathBuf> = cache::cache_entries()
        .into_iter()
        .map(|(_, root)| root)
        .filter(|r| r.starts_with(&canon))
        .collect();
    assert!(
        roots.contains(&canon.join("src")),
        "the window-8 src/ entry must survive a window-16 promotion, got {roots:?}"
    );
    assert!(roots.contains(&canon), "the window-16 root entry should exist too");
}

/// Cache transparency, as an equality rather than a hope.
///
/// "The index is a cache" (RESEARCH.md §8) has to mean that whether a scope
/// happens to be cached is invisible in the answer. It was not: the cold path
/// scored full-precision cosine over f32 embeddings while the warm path scored
/// i8 dot products over the quantized matrix, so the two ranked near-ties
/// differently. Measured over the fixture corpus, 37 of 54 query/mode pairs
/// disagreed between cold and warm; the difference was usually a swap in the
/// tail, occasionally a different hit at the k boundary.
///
/// The cold path now quantizes exactly as `store::build` does, so both score the
/// same numbers. This test compares the *whole* top-k, not just the first hit —
/// the old parity test checked `hits[0]` and so could not see any of this.
#[test]
fn cold_and_warm_return_identical_results() {
    let _cache = isolate_cache();
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());

    let queries = [
        "compute the backoff delay",
        "check whether a session token is valid",
        "baking bread with a fermented starter",
        "mirrors gathering light from stars",
        "exponential backoff retries",
        "hashing a password with salt",
        // A miss, so the empty case is covered too.
        "quantum chromodynamics lattice gauge",
    ];

    for mode in [Mode::Bm25, Mode::Semantic, Mode::Hybrid] {
        for query in queries {
            let cold = search(dir.path(), query, &stream_opts(mode)).unwrap();
            assert!(!cold.report.used_index, "stream_opts must not use an index");

            // Warm the same scope and ask again.
            let warm = search(dir.path(), query, &opts(mode)).unwrap();
            assert!(warm.report.used_index, "the second search should be warm");

            let shape =
                |r: &semgrep_core::search::SearchResult| -> Vec<(String, u32, u32, u32)> {
                    r.hits
                        .iter()
                        .map(|h| (h.path.clone(), h.start_line, h.end_line, h.line))
                        .collect()
                };
            assert_eq!(
                shape(&cold),
                shape(&warm),
                "cold and warm disagree for {mode:?} / {query:?}"
            );
        }
    }
}

/// cold == warm must hold with MaxSim on, which is now the CLI default for
/// `--mode semantic`.
///
/// `cold_and_warm_return_identical_results` builds `SearchOptions` directly and
/// leaves `rerank_maxsim` false, so it could not see this: when the rerank was
/// first defaulted on it lived only in `search::indexed`, and a cold semantic
/// search returned a different order from a warm one. Nothing in the suite
/// failed. This is the same invariant with the flag actually set.
#[test]
fn cold_and_warm_agree_with_maxsim_reranking() {
    let _cache = isolate_cache();
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());

    let queries = [
        "close connections that have gone idle",
        "compute the backoff delay",
        "check whether a session token is valid",
        "baking bread with a fermented starter",
        "quantum chromodynamics lattice gauge",
    ];

    for mode in [Mode::Semantic, Mode::Hybrid] {
        for query in queries {
            let mx = |o: SearchOptions| SearchOptions { rerank_maxsim: true, ..o };
            let cold = search(dir.path(), query, &mx(stream_opts(mode))).unwrap();
            assert!(!cold.report.used_index);
            let warm = search(dir.path(), query, &mx(opts(mode))).unwrap();
            assert!(warm.report.used_index);

            let c: Vec<_> = cold.hits.iter().map(|h| (&h.path, h.start_line)).collect();
            let w: Vec<_> = warm.hits.iter().map(|h| (&h.path, h.start_line)).collect();
            assert_eq!(c, w, "cold != warm for {mode:?} {query:?} with maxsim on");
        }
    }
}

/// The fine rerank (§28.2) must actually narrow spans — and switching it off
/// must restore the whole-chunk shape. `cold_and_warm_return_identical_results`
/// already proves cold/warm parity *with* fine on (it runs on defaults); this
/// is the non-vacuity half, so an accidentally inert rerank cannot pass the
/// parity battery by never doing anything.
#[test]
fn the_fine_rerank_narrows_spans_and_no_fine_restores_them() {
    let _cache = isolate_cache();
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let query = "compute the backoff delay";

    let fine = search(dir.path(), query, &opts(Mode::Semantic)).unwrap();
    assert!(!fine.hits.is_empty());
    for h in &fine.hits {
        let span = h.end_line - h.start_line + 1;
        assert!(
            span <= SearchOptions::default().fine_lines,
            "a fine span must fit the window: {} lines at {}:{}",
            span,
            h.path,
            h.start_line
        );
        let (cs, ce) = (
            h.chunk_start_line.expect("fine hits carry their chunk"),
            h.chunk_end_line.expect("fine hits carry their chunk"),
        );
        assert!(cs <= h.start_line && h.end_line <= ce, "the window sits inside its chunk");
        assert!(h.start_line <= h.line && h.line <= h.end_line, "best line inside the window");
        let shown = h.lines.as_ref().expect("the fine window is the passage");
        assert_eq!(shown.len() as u32, span, "the passage is exactly the window");
        assert_eq!(h.lines_from, Some(h.start_line));
    }

    let plain = search(
        dir.path(),
        query,
        &SearchOptions { fine_rerank: false, ..opts(Mode::Semantic) },
    )
    .unwrap();
    for h in &plain.hits {
        assert!(h.chunk_start_line.is_none(), "--no-fine must not carry chunk fields");
    }
    // A tail chunk can be shorter than the fine window on its own, so the
    // restored shape is asserted on the population, not per hit.
    assert!(
        plain
            .hits
            .iter()
            .any(|h| h.end_line - h.start_line + 1 > SearchOptions::default().fine_lines),
        "without fine at least one span should be chunk-sized"
    );
}

/// The score floor (§28.2) must refuse on both paths identically: same
/// zero-hit answer, same `floored` report, warm or cold. And it must be
/// set-level — a strong head with a weak tail passes untouched, because the
/// floor asks about the scope, not about each hit.
#[test]
fn cold_and_warm_agree_on_the_floor() {
    let _cache = isolate_cache();
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());

    let floor = |o: SearchOptions| SearchOptions { min_score: 0.99, ..o };
    // Nothing in the fixture is a 0.99 cosine for this.
    let query = "quantum chromodynamics lattice gauge";
    let cold = search(dir.path(), query, &floor(stream_opts(Mode::Semantic))).unwrap();
    assert!(!cold.report.used_index);
    let warm = search(dir.path(), query, &floor(opts(Mode::Semantic))).unwrap();
    assert!(warm.report.used_index);
    for (name, r) in [("cold", &cold), ("warm", &warm)] {
        assert!(r.hits.is_empty(), "{name}: a floored search returns nothing");
        assert!(r.report.floored, "{name}: and says why");
        let s = r.report.best_signal.expect("the refused score is reported");
        assert!(s < 0.99, "{name}: refused because {s} is under the floor");
    }
    assert_eq!(cold.report.best_signal, warm.report.best_signal, "same signal both paths");

    // A permissive floor changes nothing, and still reports the signal so a
    // calibration campaign can join score to outcome on successes too.
    let easy = search(
        dir.path(),
        "compute the backoff delay",
        &SearchOptions { min_score: 0.05, ..opts(Mode::Semantic) },
    )
    .unwrap();
    assert!(!easy.hits.is_empty(), "a strong match clears a low floor");
    assert!(!easy.report.floored);
    assert!(easy.report.best_signal.is_some(), "signal reported on success too");
}

/// cold == warm must hold with the declaration boost on (RESEARCH.md §24.1).
///
/// The same trap `cold_and_warm_agree_with_maxsim_reranking` was written for:
/// the boost re-reads chunk text and rescores post-fusion on *both* paths, and
/// a version living on only one of them would return a different order from a
/// cached scope than from an uncached one while every other test passed. The
/// weight is deliberately large so a one-sided implementation reorders
/// something rather than sneaking through on ties.
#[test]
fn cold_and_warm_agree_with_the_declaration_boost() {
    let _cache = isolate_cache();
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());

    let queries = [
        "compute_backoff_delay",
        "compute the backoff delay",
        "check whether a session token is valid",
        "verify_session_token",
        "quantum chromodynamics lattice gauge",
    ];

    let mut moved = false;
    for mode in [Mode::Bm25, Mode::Semantic, Mode::Hybrid] {
        for query in queries {
            // Fine rerank off: at the default blend 1.0 the fine window score
            // owns the final order, so the boost's within-pool reordering is
            // invisible and the vacuity assert below trips. This test guards
            // the coarse stage's cold/warm parity; the fine stage has its own
            // parity test (§28.2).
            let db = |o: SearchOptions| {
                SearchOptions { decl_boost: 4.0, fine_rerank: false, ..o }
            };
            let cold = search(dir.path(), query, &db(stream_opts(mode))).unwrap();
            assert!(!cold.report.used_index);
            let warm = search(dir.path(), query, &db(opts(mode))).unwrap();
            assert!(warm.report.used_index);

            let shape = |r: &semgrep_core::search::SearchResult| -> Vec<(String, u32)> {
                r.hits.iter().map(|h| (h.path.clone(), h.start_line)).collect()
            };
            assert_eq!(
                shape(&cold),
                shape(&warm),
                "cold != warm for {mode:?} {query:?} with decl_boost on"
            );
            // An inert boost would satisfy the equality above trivially, and
            // this test would then be guarding nothing at all. The baseline
            // must match the boosted arms in everything but the boost — with
            // fine left on here, every difference would be the fine rerank's
            // and `moved` would pass vacuously.
            let plain = search(
                dir.path(),
                query,
                &SearchOptions { fine_rerank: false, ..opts(mode) },
            )
            .unwrap();
            moved |= shape(&plain) != shape(&warm);
        }
    }
    assert!(moved, "decl_boost changed no result on this fixture — the test is vacuous");
}

/// cold == warm must survive prose rendering (RESEARCH.md §14.2): the cold
/// path renders inline, the warm path renders per the meta the write-through
/// build persisted, and the two must be the same function of the same option.
/// MaxSim is on, so the token-vector rendering sites are covered too.
#[test]
fn cold_and_warm_agree_under_embed_preproc() {
    use semgrep_core::text::EmbedPreproc;
    let _cache = isolate_cache();

    for pp in [
        EmbedPreproc::Split,
        EmbedPreproc::SplitWhole,
        EmbedPreproc::SplitNokw,
        EmbedPreproc::PruneKw,
        EmbedPreproc::PruneLex,
        EmbedPreproc::PruneDecl,
        EmbedPreproc::PruneSoft,
        EmbedPreproc::PruneUniq,
    ] {
        // A fresh scope per variant: cache entries are keyed by root and chunk
        // params, not by embed options, so reusing one scope would warm later
        // variants from the first one's index (the documented --sif tradeoff).
        let dir = tempfile::tempdir().unwrap();
        fixture(dir.path());
        for query in [
            "compute the backoff delay",
            "check whether a session token is valid",
            "quantum chromodynamics lattice gauge",
        ] {
            let with = |o: SearchOptions| SearchOptions {
                embed_preproc: pp,
                rerank_maxsim: true,
                ..o
            };
            let cold = search(dir.path(), query, &with(stream_opts(Mode::Semantic))).unwrap();
            assert!(!cold.report.used_index);
            let warm = search(dir.path(), query, &with(opts(Mode::Semantic))).unwrap();
            assert!(warm.report.used_index);

            let c: Vec<_> = cold.hits.iter().map(|h| (&h.path, h.start_line)).collect();
            let w: Vec<_> = warm.hits.iter().map(|h| (&h.path, h.start_line)).collect();
            assert_eq!(c, w, "cold != warm for {pp:?} {query:?}");
        }
    }
}

/// The warm path renders queries per the index's own meta, never per the flag:
/// stored vectors dictate the space. A `split` index queried with default
/// options must answer exactly as it does when the flag agrees with the meta.
#[test]
fn a_preproc_index_ignores_the_search_flag_and_follows_its_meta() {
    use semgrep_core::text::EmbedPreproc;
    let _cache = isolate_cache();
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());

    let build = BuildOptions {
        params: ChunkParams { window: 8, overlap: 2, ..Default::default() },
        embed_preproc: EmbedPreproc::Split,
        ..Default::default()
    };
    store::build(dir.path(), &build, |_, _| {}).unwrap();

    let query = "compute the backoff delay";
    let flag_agrees = SearchOptions {
        embed_preproc: EmbedPreproc::Split,
        ..opts(Mode::Semantic)
    };
    let flag_default = opts(Mode::Semantic);
    let a = search(dir.path(), query, &flag_agrees).unwrap();
    let b = search(dir.path(), query, &flag_default).unwrap();
    assert!(a.report.used_index && b.report.used_index);
    let shape = |r: &semgrep_core::search::SearchResult| -> Vec<(String, u32)> {
        r.hits.iter().map(|h| (h.path.clone(), h.start_line)).collect()
    };
    assert_eq!(shape(&a), shape(&b), "the flag leaked into a warm query");
}

/// The rerank head must not change what the *pool* is allowed to contain: a
/// deeper head may only reorder rows the shallower one already had, plus more.
#[test]
fn a_deeper_maxsim_head_is_a_superset_of_a_shallower_one() {
    let _cache = isolate_cache();
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());

    let head = |pool: usize| {
        let o = SearchOptions {
            rerank_maxsim: true,
            maxsim_pool: pool,
            k: 5,
            ..opts(Mode::Semantic)
        };
        search(dir.path(), "close connections that have gone idle", &o)
            .unwrap()
            .hits
            .iter()
            .map(|h| (h.path.clone(), h.start_line))
            .collect::<Vec<_>>()
    };
    // Both must return k hits; the deeper head may reorder but must not
    // return fewer, which would mean the pool truncation dropped candidates.
    assert_eq!(head(8).len(), head(96).len());
}

/// Post-fusion reranking at blend 0.0 must reproduce the fused order exactly.
///
/// This is the sign-convention guard. `fuse` emits higher-is-better scores and
/// `blend_head` speaks lower-is-better pseudo-distances; wiring them together
/// without converting inverts the ranking. That bug measured as hybrid R@5
/// 0.770 -> 0.058, and it was only obvious because blend 0.3 — which should
/// mostly preserve the original order — scored worse than blend 1.0. At alpha
/// 0 the rerank is the identity function, so this test fails loudly if either
/// end of the conversion is dropped.
#[test]
fn post_fusion_rerank_at_zero_blend_is_the_identity() {
    let _cache = isolate_cache();
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());

    for query in ["close connections that have gone idle", "compute the backoff delay"] {
        let base = search(dir.path(), query, &opts(Mode::Hybrid)).unwrap();
        let post = search(
            dir.path(),
            query,
            &SearchOptions {
                rerank_maxsim: true,
                maxsim_post: true,
                maxsim_blend: 0.0,
                ..opts(Mode::Hybrid)
            },
        )
        .unwrap();
        let b: Vec<_> = base.hits.iter().map(|h| (&h.path, h.start_line)).collect();
        let p: Vec<_> = post.hits.iter().map(|h| (&h.path, h.start_line)).collect();
        assert_eq!(b, p, "blend 0.0 changed the order for {query:?}");
    }
}

/// Cold and warm must agree under post-fusion reranking too — the rerank has
/// to sit at the same point in both pipelines.
#[test]
fn cold_and_warm_agree_under_post_fusion_reranking() {
    let _cache = isolate_cache();
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());

    let cfg = |o: SearchOptions| SearchOptions {
        rerank_maxsim: true,
        maxsim_post: true,
        maxsim_blend: 0.5,
        ..o
    };
    for query in ["close connections that have gone idle", "validate a session token"] {
        let cold = search(dir.path(), query, &cfg(stream_opts(Mode::Hybrid))).unwrap();
        let warm = search(dir.path(), query, &cfg(opts(Mode::Hybrid))).unwrap();
        let c: Vec<_> = cold.hits.iter().map(|h| (&h.path, h.start_line)).collect();
        let w: Vec<_> = warm.hits.iter().map(|h| (&h.path, h.start_line)).collect();
        assert_eq!(c, w, "cold != warm under post-fusion for {query:?}");
    }
}

// ---------------------------------------------------------------------------
// Publication is a swap, not a rewrite (SIMULATION.md §1.2, FIXES.md #6)
// ---------------------------------------------------------------------------

/// The SIGBUS, reduced to its mechanism.
///
/// A rebuild used to write `emb.bin` straight into the live entry, truncating a
/// file another process had already mapped — and a mapping whose backing file is
/// truncated faults on access. Measured at 5-8 bad trials in 20 on a small
/// corpus, and it is a signal, not an error, so nothing could catch it.
///
/// A mapping is an inode, and the swap only ever replaces a *name*. So a reader
/// that mapped the old `emb.bin` must still be able to read every byte of it
/// after a full rebuild has published a different one.
#[test]
fn a_rebuild_does_not_disturb_an_already_mapped_index() {
    let _cache = isolate_cache();
    let repo = tempfile::tempdir().unwrap();
    fixture(repo.path());
    let dir = semgrep_core::store::index_dir(repo.path());

    let build = |opts: &semgrep_core::store::BuildOptions| {
        semgrep_core::store::build(repo.path(), opts, |_, _| {}).expect("build")
    };
    let opts = semgrep_core::store::BuildOptions::default();
    build(&opts);

    let emb = dir.join("emb.bin");
    let file = fs::File::open(&emb).expect("emb.bin exists");
    let before: u64 = file.metadata().unwrap().len();
    assert!(before > 0, "a built index has embeddings");
    let map = unsafe { memmap2::Mmap::map(&file) }.expect("map emb.bin");
    let first_bytes = map[..map.len().min(64)].to_vec();

    // Grow the corpus so the rebuild genuinely produces a different, larger
    // emb.bin — an identical rewrite would not prove anything.
    for i in 0..12 {
        fs::write(
            repo.path().join(format!("src/extra{i}.rs")),
            format!("fn generated_symbol_{i}() {{ /* padding to move the row count */ }}\n"),
        )
        .unwrap();
    }
    build(&opts);

    assert!(
        fs::metadata(&emb).unwrap().len() > before,
        "the rebuild should have produced a larger emb.bin"
    );
    // The load-bearing assertion: touching every page of the old mapping. If
    // publication truncated the file this reader had open, this is where the
    // process would die of SIGBUS rather than fail an assertion.
    let checksum: u64 = map.iter().map(|&b| b as u64).sum();
    assert_eq!(map.len() as u64, before, "the old mapping changed size underneath us");
    assert_eq!(&map[..map.len().min(64)], &first_bytes[..], "the old mapping's bytes changed");
    let _ = checksum;
}

/// The staging directory must not be visible as an index while it is being
/// filled — including at the moment it is complete but not yet swapped, when it
/// holds a perfectly valid `meta.json`.
#[test]
fn a_staging_directory_is_never_discoverable_but_is_always_reclaimable() {
    let _cache = isolate_cache();
    let repo = tempfile::tempdir().unwrap();
    fixture(repo.path());
    let root = fs::canonicalize(repo.path()).unwrap();

    let entry = cache::cache_generation().join("pretend-entry");
    let staging = semgrep_core::store::staging_path(&entry);
    fs::create_dir_all(&staging).unwrap();
    fs::write(staging.join("root.txt"), root.to_string_lossy().as_bytes()).unwrap();
    fs::write(staging.join("params.txt"), "w40o10").unwrap();
    // A *complete* build, awaiting only its rename.
    fs::write(staging.join("meta.json"), "{}").unwrap();
    fs::write(staging.join("emb.bin"), vec![0u8; 8192]).unwrap();

    assert!(
        !cache::cache_entries().iter().any(|(d, _)| *d == staging),
        "a directory mid-swap must never be discoverable, even when complete"
    );

    let seen = cache::cache_status();
    let mine = seen.iter().find(|e| e.dir == staging).expect("still counted against the budget");
    assert!(mine.incomplete, "a meta.json inside a staging dir does not make it an entry");
    assert!(mine.bytes >= 8192, "its bytes must count");

    let r = cache::enforce_budget_with_cap(cache::cache_max_bytes(), 0);
    assert!(r.removed >= 1, "an abandoned staging dir must be reclaimable");
    assert!(!staging.exists(), "staging dir survived the prune");
}

/// Reclamation runs after registration so the enforcer can see what triggered
/// it (FIXES.md #5). Seeing it, it evicted it: the query had just paid for a
/// complete index build, missed on re-discovery, and streamed the corpus as
/// well — paying twice and keeping nothing, on 5 of 8 queries under budget
/// pressure (SIMULATION.md §1.4).
#[test]
fn a_write_does_not_evict_the_entry_it_just_wrote() {
    let _cache = isolate_cache();
    let repo = tempfile::tempdir().unwrap();
    fixture(repo.path());
    let root = fs::canonicalize(repo.path()).unwrap();

    let opts = semgrep_core::store::BuildOptions::default();
    let (dir, stats) = cache::write_cache_entry(&root, &opts, |_, _| {}).expect("write entry");
    assert!(dir.exists(), "the entry was written");

    // A cap below one entry: every candidate is over budget, so an unprotected
    // enforcer evicts the only thing there is — the entry just built.
    let cap = (stats.index_bytes / 2).max(1);
    let r = cache::enforce_budget_protecting(&dir);
    assert!(dir.exists(), "the freshly written entry must survive its own enforcement");
    assert!(r.stuck.is_empty(), "nothing resisted deletion: {:?}", r.stuck);

    // And it is still discoverable, which is the property that actually matters:
    // the query that paid for it can now be answered warm.
    let params = semgrep_core::ChunkParams::default();
    assert!(
        cache::discover(&root, &params).is_some(),
        "the entry it built must serve the query that built it"
    );

    // Unprotected, the same cap does evict it — the protection is doing the work,
    // not a budget that happened to be large enough.
    cache::enforce_budget_with_cap(cap, 600);
    assert!(!dir.exists(), "without protection an over-cap entry is evicted as before");
}

/// One undeletable directory used to take the whole cache with it: the victim
/// was popped whether or not it went and the running total only fell on success,
/// so the loop chewed through every healthy entry behind it and stopped with the
/// undeletable one as the sole survivor, at exit 0 with no warning.
#[cfg(unix)]
#[test]
fn an_undeletable_entry_does_not_take_the_healthy_ones_with_it() {
    use std::os::unix::fs::PermissionsExt;
    let _cache = isolate_cache();

    // Four entries, all live, oldest last in eviction order.
    let mut repos = Vec::new();
    let mut dirs = Vec::new();
    for i in 0..4 {
        let repo = tempfile::tempdir().unwrap();
        fs::write(repo.path().join("a.rs"), format!("fn symbol_{i}() {{}}\n")).unwrap();
        let d = cache::cache_generation().join(format!("entry-{i}"));
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("root.txt"), repo.path().to_string_lossy().as_bytes()).unwrap();
        fs::write(d.join("meta.json"), "{}").unwrap();
        fs::write(d.join("emb.bin"), vec![0u8; 4096]).unwrap();
        dirs.push(d);
        repos.push(repo);
    }

    // Make the least-recently-used one refuse to be removed. `remove_dir_all`
    // needs write+execute on the directory to unlink what is inside it.
    let stuck = &dirs[0];
    let original = fs::metadata(stuck).unwrap().permissions();
    fs::set_permissions(stuck, fs::Permissions::from_mode(0o500)).unwrap();

    // Count only the four this test made. Every test in this binary shares one
    // cache directory, so a global count is whatever else has run.
    let mine = |v: &[cache::CacheEntryInfo]| {
        v.iter().filter(|e| dirs.contains(&e.dir)).count()
    };
    let before = mine(&cache::cache_status());
    let r = cache::enforce_budget_with_cap(0, 600);
    let after = mine(&cache::cache_status());

    // Restore before asserting, or a failure leaves an undeletable tempdir.
    fs::set_permissions(stuck, original).unwrap();

    assert_eq!(before, 4, "four entries to begin with");
    assert_eq!(r.stuck.len(), 1, "the undeletable entry must be reported, not silently skipped");
    assert!(
        after >= 2,
        "one stuck entry must not cascade into the healthy ones: {after} of {before} survived"
    );
    assert!(stuck.exists(), "the undeletable entry is still there — that is the point");
}

/// The drift bound (FIXES.md #7, RESEARCH.md §8 mechanism 2).
///
/// A small drift is patched in memory — cheap, and it keeps the entry. A large
/// one replaces the entry instead, because repairing charges the same price on
/// every query past the TTL and never amortizes: on tokio a 50%-drifted scope
/// cost 131 ms a query against a 127 ms cold pass, forever (SIMULATION.md §1.3).
/// Both branches must answer with the *current* tree; only the cost differs.
#[test]
fn repair_serves_a_small_drift_and_rebuilds_a_large_one() {
    let _cache = isolate_cache();
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    // 40 files, so one file is 2.5% — under the 5% default — and twenty is 50%.
    for i in 0..40 {
        fs::write(
            dir.path().join(format!("src/mod{i}.rs")),
            format!("//! Module {i}.\npub fn stable_symbol_{i}() -> u32 {{ {i} }}\n"),
        )
        .unwrap();
    }
    let base = SearchOptions { k: 5, params: ChunkParams { window: 8, overlap: 2, ..Default::default() }, ..Default::default() };
    search(dir.path(), "stable symbol", &base).unwrap();

    // One file of forty: under the bound, so the overlay handles it and the
    // entry is kept rather than rewritten.
    fs::write(
        dir.path().join("src/mod0.rs"),
        "//! Rewritten.\npub fn exponential_backoff_with_jitter(n: u32) -> u32 { 1 << n }\n",
    )
    .unwrap();
    let r = search(dir.path(), "exponential backoff jitter", &base).unwrap();
    assert!(r.report.used_index, "a small drift stays on the warm path");
    assert!(!r.report.wrote_cache, "a small drift must not trigger a rebuild");
    assert_eq!(r.hits[0].path, "src/mod0.rs", "the overlay serves the new text");
    assert!(
        matches!(r.report.repair, RepairOutcome::Repaired { .. }),
        "expected an overlay, got {:?}",
        r.report.repair
    );

    // Twenty of forty: over the bound, so the entry is replaced. The answer must
    // still be the current tree — that is not negotiable, only the route is.
    for i in 10..30 {
        fs::write(
            dir.path().join(format!("src/mod{i}.rs")),
            format!("//! Rewritten {i}.\npub fn circuit_breaker_trips_{i}() -> bool {{ true }}\n"),
        )
        .unwrap();
    }
    let r = search(dir.path(), "circuit breaker trips", &base).unwrap();
    assert!(r.report.wrote_cache, "a large drift must rebuild the entry");
    assert!(r.report.used_index, "and must answer warm from the rebuilt entry");
    assert!(
        r.hits[0].path.starts_with("src/mod1") || r.hits[0].path.starts_with("src/mod2"),
        "the rebuild must serve the rewritten files, got {}",
        r.hits[0].path
    );

    // The rebuilt entry is clean, so the next identical query is an ordinary
    // warm hit. This is the point of rebuilding rather than streaming: repairing
    // would have charged the same price again here.
    let r = search(dir.path(), "circuit breaker trips", &base).unwrap();
    assert!(r.report.used_index && !r.report.wrote_cache, "the rebuilt entry is reused");
    assert_eq!(r.report.repair, RepairOutcome::NoDrift, "a fresh entry has nothing to repair");
}

/// The bound is off for a repo-local `.semgrep/`. It is the user's artifact, and
/// silently replacing it — or serving around it — is not the engine's call, so
/// it repairs however far it has drifted and reports the staleness.
#[test]
fn a_repo_local_index_is_repaired_however_far_it_has_drifted() {
    let _cache = isolate_cache();
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    semgrep_core::store::build(
        dir.path(),
        &semgrep_core::store::BuildOptions { params: ChunkParams { window: 8, overlap: 2, ..Default::default() }, ..Default::default() },
        |_, _| {},
    )
    .unwrap();

    // Rewrite most of the corpus — far past any threshold.
    for name in ["src/auth.rs", "src/retry.rs", "docs/ops.md"] {
        if dir.path().join(name).exists() {
            fs::write(
                dir.path().join(name),
                "//! Rewritten.\npub fn circuit_breaker_trip(n: u32) -> bool { n > 3 }\n",
            )
            .unwrap();
        }
    }

    let o = SearchOptions { k: 5, params: ChunkParams { window: 8, overlap: 2, ..Default::default() }, ..Default::default() };
    let r = search(dir.path(), "circuit breaker tripping", &o).unwrap();
    assert!(r.report.used_index, "a repo-local index still answers");
    assert!(!r.report.wrote_cache, "and is never rebuilt behind the user's back");
    assert!(
        matches!(r.report.repair, RepairOutcome::Repaired { .. }),
        "a repo-local index repairs at any drift, got {:?}",
        r.report.repair
    );
    assert!(r.report.stale_files > 0, "the staleness is reported rather than hidden");
}

/// A narrow scope must not starve (FIXES.md #13, SIMULATION.md §1.7).
///
/// The fused list is `FUSION_POOL * 2` = 256 rows wide. The scope filter used to
/// run *after* that truncation, so a subtree holding none of the corpus-wide top
/// 256 got nothing — on tokio, `docs/` returned zero hits from a fully indexed
/// 8,042-chunk corpus. The condition is built here rather than hoped for: 400
/// noise files that all answer the query better than the one file in the scope.
#[test]
fn a_narrow_scope_returns_hits_even_when_the_corpus_head_excludes_it() {
    let _cache = isolate_cache();
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("noise")).unwrap();
    fs::create_dir_all(dir.path().join("target")).unwrap();

    // Enough noise to fill the fused window several times over, all of it a
    // stronger lexical match than the target.
    for i in 0..400 {
        fs::write(
            dir.path().join(format!("noise/n{i}.rs")),
            "//! exponential backoff retry policy\n\
             pub fn exponential_backoff_retry_policy_{i}() {{ /* backoff retry policy */ }}\n"
                .replace("{i}", &i.to_string()),
        )
        .unwrap();
    }
    // One weak match, in its own subtree.
    fs::write(
        dir.path().join("target/only.rs"),
        "//! Scheduling.\npub fn retry_after(delay: u64) -> u64 { delay }\n",
    )
    .unwrap();

    let o = SearchOptions {
        k: 5,
        params: ChunkParams { window: 8, overlap: 2, ..Default::default() },
        ..Default::default()
    };

    // Warm an index covering the whole tree, then query only the subtree.
    let all = search(dir.path(), "exponential backoff retry policy", &o).unwrap();
    assert!(all.hits.iter().all(|h| h.path.starts_with("noise/")),
            "the corpus head should be all noise, or this proves nothing");

    let scoped =
        search(&dir.path().join("target"), "exponential backoff retry policy", &o).unwrap();
    assert!(scoped.report.used_index, "the subtree query is served warm");
    assert!(
        !scoped.hits.is_empty(),
        "a scope outside the corpus-wide head must still return its own rows"
    );
    assert!(
        scoped.hits.iter().all(|h| h.path.starts_with("target/") || !h.path.contains('/')),
        "out-of-scope rows leaked in: {:?}",
        scoped.hits.iter().map(|h| &h.path).collect::<Vec<_>>()
    );

    // And the whole-corpus answer is untouched by the mask existing.
    let again = search(dir.path(), "exponential backoff retry policy", &o).unwrap();
    assert_eq!(
        again.hits.iter().map(|h| h.path.clone()).collect::<Vec<_>>(),
        all.hits.iter().map(|h| h.path.clone()).collect::<Vec<_>>(),
        "an unscoped query must rank exactly as it did before"
    );
}

/// A hidden subtree is absent from its parent's index, and that is a different
/// finding from the starvation above with a different cause and a different fix.
///
/// SIMULATION.md §1.7 reported `.github` and `docs` on tokio together, as one
/// finding: two scopes returning zero hits from a fully indexed corpus. Only
/// `docs` was that finding. `corpus::walk` runs `ignore::WalkBuilder` at its
/// default `hidden(true)`, so tokio's index holds **no** dot-prefixed path at
/// all — there was never anything under `.github` to starve. The scope mask
/// cannot fix that and should not pretend to.
///
/// What happens instead is worth pinning, because it is not obvious and it is
/// the reason the drift bound needs its retry escape (see `search::search`):
/// asking for the hidden scope by name builds it an index of its own, because a
/// walk rooted *at* `.github` does not consider its contents hidden.
#[test]
fn a_hidden_subtree_is_absent_from_its_parents_index_but_searchable_on_its_own() {
    let _cache = isolate_cache();
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    fs::create_dir_all(dir.path().join(".github")).unwrap();
    fs::write(
        dir.path().join(".github/workflow.yml"),
        "name: exponential backoff retry policy\njobs:\n  retry:\n    backoff: exponential\n",
    )
    .unwrap();

    let params = ChunkParams { window: 8, overlap: 2, ..Default::default() };
    let from_root = semgrep_core::corpus::walk(dir.path(), &params).unwrap();
    assert!(
        !from_root.iter().any(|f| f.path.starts_with(".github")),
        "a walk from the parent skips hidden directories: {:?}",
        from_root.iter().map(|f| &f.path).collect::<Vec<_>>()
    );
    let from_itself = semgrep_core::corpus::walk(&dir.path().join(".github"), &params).unwrap();
    assert_eq!(
        from_itself.len(),
        1,
        "but a walk rooted at it does not: {:?}",
        from_itself.iter().map(|f| &f.path).collect::<Vec<_>>()
    );

    let o = SearchOptions { k: 5, params, ..Default::default() };
    search(dir.path(), "retry backoff", &o).unwrap();

    // First ask: the parent entry covers the path but holds nothing under it, so
    // the scope gets an index of its own.
    let first = search(&dir.path().join(".github"), "exponential backoff", &o).unwrap();
    assert!(first.report.wrote_cache, "the hidden scope is indexed on demand");
    assert!(!first.hits.is_empty(), "and then answers");

    // Second ask: warm, and — the part that matters — *not* another rebuild.
    // Re-raising the drift bound here would charge a build and a stream on every
    // query for as long as the scope stayed hidden from its parent's walk.
    let second = search(&dir.path().join(".github"), "exponential backoff", &o).unwrap();
    assert!(second.report.used_index, "served from the entry just built");
    assert!(!second.report.wrote_cache, "a hidden scope must not rebuild on every query");
}

/// A search scope that IS a file must work in every mode.
///
/// RESEARCH.md §16.11: it did not. `walk` stripped the root off itself,
/// yielding an empty relative path, and four separate `root.join(rel)` sites
/// then looked for `<file>/<file>`. Ranked search over a single file returned
/// nothing — 100% of the time, for 47% of one campaign's agent searches —
/// while reporting success. Exact mode "worked" but printed `:9:text` with no
/// filename. Nothing in the suite covered a file-as-root, which is exactly
/// the scope an agent uses for a follow-up query.
#[test]
fn a_single_file_scope_returns_hits_in_every_mode() {
    let _cache = isolate_cache();
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let file = dir.path().join("src/retry.rs");

    for mode in [Mode::Bm25, Mode::Semantic, Mode::Hybrid] {
        for no_index in [true, false] {
            let o = SearchOptions { mode, no_index, k: 3, ..opts(mode) };
            let r = search(&file, "compute the backoff delay", &o).unwrap();
            assert!(!r.hits.is_empty(),
                    "{mode:?} (no_index={no_index}) found nothing in a file scope");
            assert!(!r.hits[0].path.is_empty(),
                    "{mode:?} (no_index={no_index}) produced an empty path");
        }
    }

    let o = SearchOptions { mode: Mode::Keyword, k: 5, ..opts(Mode::Keyword) };
    let r = search(&file, "compute_backoff_delay", &o).unwrap();
    assert!(!r.hits.is_empty(), "keyword found nothing in a file scope");
    assert!(!r.hits[0].path.is_empty(), "keyword produced an empty path");
}

/// A file scope must not build a cache entry it can never read back.
///
/// `cache::discover` refuses a non-directory root, so an entry keyed at a file
/// has no possible reader. Before this was guarded, every file-scoped search
/// built a complete index, wrote it, failed to re-discover it, streamed anyway,
/// and then had the entry deleted by the budget sweep — which judges a root
/// dead by `is_dir` and so classified a live file as a dead repo. `--stats`
/// named the round trip `built_but_missed`. Agents scope to a file constantly
/// (47% of searches in the §16.10 campaign), and the tier-1 trace caught it on
/// the first smoke run.
#[test]
fn a_file_scope_does_not_write_a_cache_entry() {
    let _guard = isolate_cache();
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let file = dir.path().join("src/retry.rs");

    for _ in 0..3 {
        let r = search(&file, "compute the backoff delay", &opts(Mode::Semantic)).unwrap();
        assert!(!r.hits.is_empty(), "a file scope must still answer");
        assert!(!r.report.wrote_cache, "a file scope must not write a cache entry");
        assert!(!r.report.used_index, "and cannot be served from one");
    }
    let entries = semgrep_core::cache::cache_status();
    assert!(
        !entries.iter().any(|e| e.root == std::fs::canonicalize(&file).unwrap()),
        "a cache entry was written for a file root: {:?}",
        entries.iter().map(|e| &e.root).collect::<Vec<_>>()
    );

    // The control: a directory scope still caches, so the guard above is not
    // simply switching write-through off.
    let r = search(dir.path(), "compute the backoff delay", &opts(Mode::Semantic)).unwrap();
    assert!(r.report.wrote_cache, "a directory scope must still write through");
}


/// cold == warm must survive path rendering too (RESEARCH.md §20.1). Separate
/// from the tier loop because `PathRender` is the orthogonal axis: a bug that
/// only shows up when the path is rewritten would hide inside a tier sweep.
#[test]
fn cold_and_warm_agree_under_path_render() {
    use semgrep_core::text::{EmbedPreproc, PathRender};
    let _cache = isolate_cache();

    for pr in [PathRender::Dedupe, PathRender::Tail, PathRender::Scaled] {
        let dir = tempfile::tempdir().unwrap();
        fixture(dir.path());
        for query in ["compute the backoff delay", "check whether a session token is valid"] {
            let with = |o: SearchOptions| SearchOptions {
                embed_preproc: EmbedPreproc::PruneLex,
                path_render: pr,
                rerank_maxsim: true,
                ..o
            };
            let cold = search(dir.path(), query, &with(stream_opts(Mode::Semantic))).unwrap();
            assert!(!cold.report.used_index);
            let warm = search(dir.path(), query, &with(opts(Mode::Semantic))).unwrap();
            assert!(warm.report.used_index);

            let c: Vec<_> = cold.hits.iter().map(|h| (&h.path, h.start_line)).collect();
            let w: Vec<_> = warm.hits.iter().map(|h| (&h.path, h.start_line)).collect();
            assert_eq!(c, w, "cold != warm for {pr:?} {query:?}");
        }
    }
}

/// A character budget and a line window cut the same tree differently, so they
/// must not share a cache entry — that is FIXES.md #10 one parameter later.
#[test]
fn a_budgeted_entry_never_answers_a_line_windowed_query() {
    let _cache = isolate_cache();
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());

    let lines = SearchOptions {
        params: ChunkParams { window: 8, overlap: 2, ..Default::default() },
        ..opts(Mode::Semantic)
    };
    let budgeted = SearchOptions {
        params: ChunkParams {
            window: 8,
            overlap: 2,
            budget: Some(200),
            ..Default::default()
        },
        ..opts(Mode::Semantic)
    };
    // Warm both, in that order. If they shared an entry the second would be
    // served from the first's chunking and report a hit against the wrong ids.
    search(dir.path(), "compute the backoff delay", &lines).unwrap();
    let b = search(dir.path(), "compute the backoff delay", &budgeted).unwrap();
    assert!(b.report.used_index);

    // Chunk boundaries differ, so at least one hit must start on a different
    // line — otherwise the two entries are indistinguishable and the guard is
    // untested rather than passing.
    let a = search(dir.path(), "compute the backoff delay", &lines).unwrap();
    let starts = |r: &semgrep_core::search::SearchResult| -> Vec<(String, u32)> {
        r.hits.iter().map(|h| (h.path.clone(), h.start_line)).collect()
    };
    assert_ne!(starts(&a), starts(&b), "budgeted and line-windowed chunking coincided");
}

/// A budgeted build must be reproducible cold, like every other chunking.
#[test]
fn cold_and_warm_agree_under_a_character_budget() {
    let _cache = isolate_cache();
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let params = ChunkParams { window: 8, overlap: 2, budget: Some(200), ..Default::default() };
    for query in ["compute the backoff delay", "check whether a session token is valid"] {
        let cold =
            search(dir.path(), query, &SearchOptions { params, ..stream_opts(Mode::Semantic) })
                .unwrap();
        let warm =
            search(dir.path(), query, &SearchOptions { params, ..opts(Mode::Semantic) }).unwrap();
        assert!(!cold.report.used_index && warm.report.used_index);
        let c: Vec<_> = cold.hits.iter().map(|h| (&h.path, h.start_line)).collect();
        let w: Vec<_> = warm.hits.iter().map(|h| (&h.path, h.start_line)).collect();
        assert_eq!(c, w, "cold != warm under a budget for {query:?}");
    }
}

/// Function-mode entries live under their own `f` tag (§29.3): the template
/// is `a_budgeted_entry_never_answers_a_line_windowed_query`, one mode later.
/// Fine rerank off — these tests read hit spans as chunk geometry, and the
/// fine window would narrow every span to a few lines regardless of cutter.
#[cfg(feature = "func-chunk")]
#[test]
fn a_function_entry_never_answers_a_window_query() {
    let _cache = isolate_cache();
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());

    let lines = SearchOptions {
        fine_rerank: false,
        params: ChunkParams { window: 8, overlap: 2, ..Default::default() },
        ..opts(Mode::Semantic)
    };
    let function = SearchOptions {
        fine_rerank: false,
        params: ChunkParams {
            window: 8,
            overlap: 2,
            function: Some(semgrep_core::FUNC_CAP_DEFAULT),
            ..Default::default()
        },
        ..opts(Mode::Semantic)
    };
    search(dir.path(), "compute the backoff delay", &lines).unwrap();
    let f = search(dir.path(), "compute the backoff delay", &function).unwrap();
    assert!(f.report.used_index);

    let a = search(dir.path(), "compute the backoff delay", &lines).unwrap();
    // Full spans, not just starts: a function chunk and an 8-line window can
    // begin at the same line (a def at the top of a file does), but a cutter
    // that ends at the function's brace and one that ends 8 lines in cannot
    // agree everywhere unless the entries were shared.
    let spans = |r: &semgrep_core::search::SearchResult| -> Vec<(String, u32, u32)> {
        r.hits.iter().map(|h| (h.path.clone(), h.start_line, h.end_line)).collect()
    };
    assert_ne!(spans(&a), spans(&f), "function and window chunking coincided");
}

/// Function chunking must be reproducible cold, like every other chunking —
/// the parse is a pure function of the file bytes, and both paths cut through
/// the same `corpus::chunk_lines`.
#[cfg(feature = "func-chunk")]
#[test]
fn cold_and_warm_agree_under_function_chunking() {
    let _cache = isolate_cache();
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let params = ChunkParams {
        window: 8,
        overlap: 2,
        function: Some(semgrep_core::FUNC_CAP_DEFAULT),
        ..Default::default()
    };
    for query in ["compute the backoff delay", "check whether a session token is valid"] {
        let cold = search(
            dir.path(),
            query,
            &SearchOptions { params, ..stream_opts(Mode::Semantic) },
        )
        .unwrap();
        assert!(!cold.report.used_index);
        let warm =
            search(dir.path(), query, &SearchOptions { params, ..opts(Mode::Semantic) }).unwrap();
        assert!(warm.report.used_index);
        let shape = |r: &semgrep_core::search::SearchResult| -> Vec<(String, u32, u32)> {
            r.hits.iter().map(|h| (h.path.clone(), h.start_line, h.end_line)).collect()
        };
        assert_eq!(shape(&cold), shape(&warm), "cold != warm under function chunking: {query}");
    }
}

/// Read-repair re-cuts a drifted file from `meta.params` alone (§29.3): under
/// function mode the repaired chunks must equal what a fresh build would cut,
/// or a warm entry answers with different spans than a rebuild — the exact
/// drift the params tag exists to prevent, one layer down.
#[cfg(feature = "func-chunk")]
#[test]
fn repair_recuts_a_drifted_file_the_way_a_build_would() {
    let _cache = isolate_cache();
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let params = ChunkParams {
        window: 8,
        overlap: 2,
        function: Some(semgrep_core::FUNC_CAP_DEFAULT),
        ..Default::default()
    };
    let o = SearchOptions { fine_rerank: false, params, ..opts(Mode::Semantic) };
    let query = "compute the backoff delay";
    search(dir.path(), query, &o).unwrap();

    // Drift the gold file: a new function above the old one moves every span.
    let target = dir.path().join("src/retry.rs");
    let old = std::fs::read_to_string(&target).unwrap();
    std::fs::write(
        &target,
        format!("/// Added later.\nfn added_later(x: u32) -> u32 {{\n    x + 1\n}}\n\n{old}"),
    )
    .unwrap();

    // TTL 0 in tests, so the next warm query repairs around the drift.
    let repaired = search(dir.path(), query, &o).unwrap();
    assert!(repaired.report.used_index, "the entry should be patched, not discarded");
    let fresh = search(
        dir.path(),
        query,
        &SearchOptions { no_index: true, ..o.clone() },
    )
    .unwrap();
    let shape = |r: &semgrep_core::search::SearchResult| -> Vec<(String, u32, u32)> {
        r.hits.iter().map(|h| (h.path.clone(), h.start_line, h.end_line)).collect()
    };
    assert_eq!(shape(&repaired), shape(&fresh), "repair cut differently than a build");
}
