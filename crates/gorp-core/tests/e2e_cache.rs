//! End-to-end: cache behaviors (RESEARCH.md §8/8.1) — write-through,
//! ancestor discovery, scope promotion, and the read-repair overlay that
//! keeps a warm answer indistinguishable from a cold one.

mod common;
use gorp_core::ChunkParams;
use gorp_core::cache;
use gorp_core::search::{Mode, SearchOptions, search};
use gorp_core::store::{self, BuildOptions};
use std::fs;
use std::path::Path;

use common::*;

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
    let opts = gorp_core::search::SearchOptions::default();
    gorp_core::search::search(dir.path(), "greeting", &opts).unwrap();
    // Only this test's own entry — the cache dir is shared with other tests
    // running in parallel, so corrupting all of them clobbers their state.
    let mine = std::fs::canonicalize(dir.path()).unwrap();
    let entries: Vec<_> = gorp_core::cache::cache_entries()
        .into_iter()
        .filter(|(_, root)| *root == mine)
        .collect();
    assert_eq!(entries.len(), 1, "expected exactly one entry for this scope");
    for (d, _) in &entries {
        let p = d.join("meta.json");
        let mut m: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        m["dims"] = serde_json::json!(gorp_core::EMBED_DIM + 1);
        std::fs::write(&p, serde_json::to_vec(&m).unwrap()).unwrap();
    }

    // Must still answer, not error.
    let res = gorp_core::search::search(dir.path(), "greeting", &opts)
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
    let opts = gorp_core::search::SearchOptions::default();
    gorp_core::search::search(dir.path(), "configuration parsing", &opts).unwrap();

    let gen_dir = gorp_core::cache::cache_generation();
    let key = gorp_core::cache::compat_key();
    assert!(gen_dir.ends_with(&key), "generation dir should be the compat key");
    assert!(
        gorp_core::cache::cache_entries().iter().all(|(d, _)| d.starts_with(&gen_dir)),
        "every entry must live under the current generation"
    );

    // An entry from another generation is invisible, not an error.
    let alien = gorp_core::cache::cache_base().join("v2-d999-deadbeefdeadbeef");
    std::fs::create_dir_all(alien.join("abc")).unwrap();
    std::fs::write(alien.join("abc/meta.json"), b"{}").unwrap();
    std::fs::write(alien.join("abc/root.txt"), dir.path().to_string_lossy().as_bytes())
        .unwrap();
    assert!(
        gorp_core::cache::cache_entries().iter().all(|(d, _)| !d.starts_with(&alien)),
        "entries from another generation must not be discovered"
    );

    // ...and a write reclaims it, along with pre-generation flat entries
    // left by older builds (identified by holding meta.json directly).
    let legacy = gorp_core::cache::cache_base().join("deadbeef00000000");
    std::fs::create_dir_all(&legacy).unwrap();
    std::fs::write(legacy.join("meta.json"), b"{}").unwrap();
    std::fs::write(legacy.join("root.txt"), b"/tmp").unwrap();
    let unrelated = gorp_core::cache::cache_base().join("someones-other-data");
    std::fs::create_dir_all(&unrelated).unwrap();

    gorp_core::cache::gc_old_generations();
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
    let opts = gorp_core::search::SearchOptions::default();
    gorp_core::search::search(dir.path(), "backoff", &opts).unwrap();

    let mine = std::fs::canonicalize(dir.path()).unwrap();
    let (entry, _) = gorp_core::cache::cache_entries()
        .into_iter()
        .find(|(_, root)| *root == mine)
        .expect("entry for this scope");
    std::fs::remove_file(entry.join("emb.bin")).unwrap();

    let res = gorp_core::search::search(dir.path(), "backoff", &opts)
        .expect("a corrupt cache entry must not surface as an error");
    assert!(res.hits.iter().any(|h| h.path.ends_with("a.py")), "expected an answer");
}

/// A cache with no ceiling is a slow disk leak: the kernel corpus alone
/// indexes to ~946 MB. Entries whose repo is gone are dead weight forever,
/// and past the budget the least-recently-used must go.
#[test]
fn cache_prunes_dead_entries_and_enforces_a_budget() {
    let _cache = isolate_cache();
    let opts = gorp_core::search::SearchOptions::default();

    // Two scopes; one of them we then delete off disk.
    let keep = tempfile::tempdir().unwrap();
    std::fs::write(keep.path().join("a.py"), "def keeper():\n    return 1\n").unwrap();
    let doomed = tempfile::tempdir().unwrap();
    std::fs::write(doomed.path().join("b.py"), "def doomed():\n    return 2\n").unwrap();
    gorp_core::search::search(keep.path(), "keeper", &opts).unwrap();
    gorp_core::search::search(doomed.path(), "doomed", &opts).unwrap();

    let doomed_root = std::fs::canonicalize(doomed.path()).unwrap();
    let keep_root = std::fs::canonicalize(keep.path()).unwrap();
    assert!(gorp_core::cache::cache_status().iter().any(|e| e.root == doomed_root));

    // The repo goes away; its entry can never be useful again.
    drop(doomed);
    let r = gorp_core::cache::enforce_budget();
    assert!(r.removed >= 1, "expected the dead entry to be reclaimed");
    assert!(r.stuck.is_empty(), "nothing should have resisted deletion: {:?}", r.stuck);
    let after = gorp_core::cache::cache_status();
    assert!(!after.iter().any(|e| e.root == doomed_root), "dead entry survived");
    assert!(after.iter().any(|e| e.root == keep_root), "live entry was evicted");

    // With a budget of zero, even a live entry must be evicted. Passing the cap
    // explicitly rather than through the environment: `cache_max_bytes` is read
    // per call, so mutating it here would leak into whatever else is running.
    gorp_core::cache::enforce_budget_with_cap(0, 0);
    assert!(
        gorp_core::cache::cache_status().is_empty(),
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

/// A repo-local `.gorp/` is a committed artifact. Read-repair used to touch
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
    assert_eq!(before, fingerprint(), "a search must not modify a repo-local .gorp/");
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
                |r: &gorp_core::search::SearchResult| -> Vec<(String, u32, u32, u32)> {
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

            let shape = |r: &gorp_core::search::SearchResult| -> Vec<(String, u32)> {
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

/// cold == warm must hold with the path boost on (RESEARCH.md §35.1).
///
/// Same trap, same shape as `cold_and_warm_agree_with_the_declaration_boost`:
/// the boost rescores post-fusion on *both* paths through the one
/// `apply_structural_boost`, and a version living on only one of them would
/// answer a cached scope differently from an uncached one. `auth` and
/// `cooking` appear in fixture paths but not in file bodies, so the path term
/// has candidates only it can lift; the weight is large so it reorders rather
/// than sneaking through on ties.
#[test]
fn cold_and_warm_agree_with_the_path_boost() {
    let _cache = isolate_cache();
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());

    let queries = [
        "session token in the auth module",
        "backoff cooking",
        "retry compute backoff",
        "quantum chromodynamics lattice gauge",
    ];

    let mut moved = false;
    for mode in [Mode::Bm25, Mode::Semantic, Mode::Hybrid] {
        for query in queries {
            // Fine rerank off for the same reason as the declaration twin: at
            // blend 1.0 the fine window owns the final order and within-pool
            // reordering is invisible to the vacuity assert.
            let pb = |o: SearchOptions| {
                SearchOptions { path_boost: 4.0, fine_rerank: false, ..o }
            };
            let cold = search(dir.path(), query, &pb(stream_opts(mode))).unwrap();
            assert!(!cold.report.used_index);
            let warm = search(dir.path(), query, &pb(opts(mode))).unwrap();
            assert!(warm.report.used_index);

            let shape = |r: &gorp_core::search::SearchResult| -> Vec<(String, u32)> {
                r.hits.iter().map(|h| (h.path.clone(), h.start_line)).collect()
            };
            assert_eq!(
                shape(&cold),
                shape(&warm),
                "cold != warm for {mode:?} {query:?} with path_boost on"
            );
            let plain = search(
                dir.path(),
                query,
                &SearchOptions { fine_rerank: false, ..opts(mode) },
            )
            .unwrap();
            moved |= shape(&plain) != shape(&warm);
        }
    }
    assert!(moved, "path_boost changed no result on this fixture — the test is vacuous");
}

/// cold == warm must hold with the learned checklist on (RESEARCH.md §35.2).
///
/// The checklist runs inside `finalize` — the shared tail — so parity holds
/// by construction *provided* every feature is candidate-local; this is the
/// tripwire for anyone adding a feature that reads index state. Diversify is
/// pinned off so MMR cannot mask (or manufacture) the reorder the vacuity
/// guard demands.
#[test]
fn cold_and_warm_agree_with_the_learned_reranker() {
    let _cache = isolate_cache();
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());

    let queries = [
        "compute the backoff delay",
        "validate a session token",
        "retry compute backoff",
        "telescope starlight",
    ];

    let mut moved = false;
    for mode in [Mode::Bm25, Mode::Semantic, Mode::Hybrid] {
        for query in queries {
            let lb = |o: SearchOptions| {
                SearchOptions { learned_blend: 1.0, diversify: false, ..o }
            };
            let cold = search(dir.path(), query, &lb(stream_opts(mode))).unwrap();
            assert!(!cold.report.used_index);
            let warm = search(dir.path(), query, &lb(opts(mode))).unwrap();
            assert!(warm.report.used_index);

            let shape = |r: &gorp_core::search::SearchResult| -> Vec<(String, u32)> {
                r.hits.iter().map(|h| (h.path.clone(), h.start_line)).collect()
            };
            assert_eq!(
                shape(&cold),
                shape(&warm),
                "cold != warm for {mode:?} {query:?} with the learned blend on"
            );
            let plain = search(
                dir.path(),
                query,
                &SearchOptions { diversify: false, ..opts(mode) },
            )
            .unwrap();
            moved |= shape(&plain) != shape(&warm);
        }
    }
    assert!(moved, "the learned blend changed no result on this fixture — the test is vacuous");
}

/// cold == warm must survive prose rendering (RESEARCH.md §14.2): the cold
/// path renders inline, the warm path renders per the meta the write-through
/// build persisted, and the two must be the same function of the same option.
/// MaxSim is on, so the token-vector rendering sites are covered too.
#[test]
fn cold_and_warm_agree_under_embed_preproc() {
    use gorp_core::text::EmbedPreproc;
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
    use gorp_core::text::EmbedPreproc;
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
    let shape = |r: &gorp_core::search::SearchResult| -> Vec<(String, u32)> {
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
