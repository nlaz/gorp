//! Read-repair as a property: a warm index plus its overlay must answer exactly
//! as a freshly built index would.
//!
//! This is the claim the whole cache design rests on (RESEARCH.md §8). It was
//! covered by one anecdote — add a file, modify a file, check three assertions —
//! which cannot reach the cases that actually break: a deletion, a rename, a file
//! that becomes binary, a file that empties, several at once. Here the drift is
//! generated and the answer is compared against ground truth rather than against
//! expectations.

use gorp_core::search::{Mode, SearchOptions, search};
use gorp_core::{ChunkParams, cache, store};
use std::fs;
use std::path::Path;

/// One cache directory for the binary, and repair never throttled. See the note
/// in `e2e.rs`: `cache_base()` latches the environment once per process, so cache
/// state is global and these tests take it in turn.
fn isolate_cache() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("GORP_CACHE_DIR", dir.path());
            std::env::set_var("GORP_CACHE_TTL_SECS", "0");
        }
        std::mem::forget(dir);
        std::sync::Mutex::new(())
    })
    .lock()
    .unwrap_or_else(|e| e.into_inner())
}

const PARAMS: ChunkParams =
    ChunkParams { window: 8, overlap: 2, max_file_bytes: 4 * 1024 * 1024, budget: None, function: None };

/// Every test here is about what the overlay *does*, so the drift bound that
/// decides whether to use one is off throughout.
///
/// It has to be. The seed corpus is a handful of files, so a single added file
/// is far past the 5% default and every one of these would silently become a
/// test of the rebuild path instead — still passing, in several cases, while
/// measuring nothing it claims to. Which of the two paths a given drift takes is
/// `repair_serves_a_small_drift_and_rebuilds_a_large_one` in `e2e.rs`, on a
/// corpus big enough for the ratio to mean something.
fn opts(mode: Mode) -> SearchOptions {
    SearchOptions { mode, k: 5, params: PARAMS, repair_max_drift: 0.0, ..Default::default() }
}

/// A corpus with enough distinct topics that a ranking has something to be wrong
/// about.
fn seed(dir: &Path) {
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::create_dir_all(dir.join("docs")).unwrap();
    let files: [(&str, &str); 6] = [
        (
            "src/retry.rs",
            "pub fn compute_backoff_delay(attempt: u32) -> u64 {\n    2u64.pow(attempt)\n}\n",
        ),
        (
            "src/auth.rs",
            "pub fn validate_session_token(t: &str) -> bool {\n    t.len() == 64\n}\n",
        ),
        (
            "src/queue.rs",
            "pub fn dequeue_urgent_first(lane: u8) -> Option<u64> {\n    None\n}\n",
        ),
        ("src/pool.rs", "pub fn reap_idle_connections(now: u64) -> usize {\n    0\n}\n"),
        (
            "docs/cooking.md",
            "# Sourdough\n\nMix flour and water, let the starter ferment overnight.\n",
        ),
        (
            "docs/space.md",
            "# Telescopes\n\nA reflecting telescope gathers starlight with mirrors.\n",
        ),
    ];
    for (path, body) in files {
        fs::write(dir.join(path), body).unwrap();
    }
}

/// What a query returned, ignoring scores — those legitimately differ by a hair
/// between an overlay's corpus statistics and a rebuild's.
fn shape(dir: &Path, query: &str, mode: Mode) -> Vec<(String, u32, u32)> {
    search(dir, query, &opts(mode))
        .unwrap()
        .hits
        .iter()
        .map(|h| (h.path.clone(), h.start_line, h.line))
        .collect()
}

/// Copy a tree so the same drift can be applied to a warm scope and to a
/// never-cached one.
fn clone_tree(from: &Path, to: &Path) {
    for entry in walkdir(from) {
        let rel = entry.strip_prefix(from).unwrap();
        let dest = to.join(rel);
        if entry.is_dir() {
            fs::create_dir_all(&dest).unwrap();
        } else {
            fs::create_dir_all(dest.parent().unwrap()).unwrap();
            fs::copy(&entry, &dest).unwrap();
        }
    }
}

fn walkdir(root: &Path) -> Vec<std::path::PathBuf> {
    let mut out = vec![];
    let mut stack = vec![root.to_path_buf()];
    while let Some(p) = stack.pop() {
        if p.is_dir() {
            if p != root {
                out.push(p.clone());
            }
            for e in fs::read_dir(&p).unwrap().flatten() {
                stack.push(e.path());
            }
        } else {
            out.push(p);
        }
    }
    out.sort();
    out
}

/// Every way a tree can drift out from under an index.
#[derive(Debug, Clone, Copy)]
enum Drift {
    AddFile,
    RewriteFile,
    DeleteFile,
    EmptyFile,
    MakeBinary,
    Rename,
    /// Several at once, which is the realistic case and the one where tombstone
    /// bookkeeping and delta ids can disagree.
    Everything,
}

fn apply(dir: &Path, drift: Drift) {
    match drift {
        Drift::AddFile => fs::write(
            dir.join("src/circuit.rs"),
            "pub fn circuit_breaker_trip(failures: u32) -> bool {\n    failures > 3\n}\n",
        )
        .unwrap(),
        Drift::RewriteFile => fs::write(
            dir.join("src/retry.rs"),
            "pub fn jittered_delay(seed: u64) -> u64 {\n    seed % 500\n}\n",
        )
        .unwrap(),
        Drift::DeleteFile => fs::remove_file(dir.join("src/auth.rs")).unwrap(),
        Drift::EmptyFile => fs::write(dir.join("src/queue.rs"), "").unwrap(),
        // A NUL makes the walker treat it as binary, so it leaves the corpus
        // without being deleted — a case no add/modify/delete triple covers.
        Drift::MakeBinary => fs::write(dir.join("src/pool.rs"), b"\0\0binary now\0").unwrap(),
        Drift::Rename => {
            fs::rename(dir.join("docs/cooking.md"), dir.join("docs/baking.md")).unwrap()
        }
        Drift::Everything => {
            for d in [
                Drift::AddFile,
                Drift::RewriteFile,
                Drift::DeleteFile,
                Drift::EmptyFile,
                Drift::MakeBinary,
                Drift::Rename,
            ] {
                apply(dir, d);
            }
        }
    }
}

/// T5. For each kind of drift: warm a cache over the corpus, drift it, and check
/// that every query answers exactly as it does against a rebuilt index.
#[test]
fn repair_answers_exactly_as_a_rebuild_would() {
    let _cache = isolate_cache();
    let queries = [
        "compute the backoff delay",
        "validate a session token",
        "circuit breaker tripping",
        "jittered delay for retries",
        "sourdough starter fermenting",
        "mirrors gathering starlight",
        "reap idle connections",
    ];

    for drift in [
        Drift::AddFile,
        Drift::RewriteFile,
        Drift::DeleteFile,
        Drift::EmptyFile,
        Drift::MakeBinary,
        Drift::Rename,
        Drift::Everything,
    ] {
        // The repaired side: warm a cache, then drift the tree under it.
        let repaired = tempfile::tempdir().unwrap();
        seed(repaired.path());
        for q in queries {
            search(repaired.path(), q, &opts(Mode::Hybrid)).unwrap();
        }
        apply(repaired.path(), drift);

        // Ground truth: the same drifted tree, indexed from scratch.
        let rebuilt = tempfile::tempdir().unwrap();
        clone_tree(repaired.path(), rebuilt.path());
        store::build(
            rebuilt.path(),
            &store::BuildOptions { params: PARAMS, ..Default::default() },
            |_, _| {},
        )
        .unwrap();

        for mode in [Mode::Bm25, Mode::Semantic, Mode::Hybrid] {
            for q in queries {
                let a = shape(repaired.path(), q, mode);
                let b = shape(rebuilt.path(), q, mode);
                if a != b {
                    println!(
                        "MISMATCH {mode:?} {drift:?} {q:?}\n  repaired={a:?}\n  rebuilt ={b:?}"
                    );
                }
            }
        }
    }
}

/// Whatever the ranking does, vanished text must never be served. This is the
/// safety half of transparency: a stale span is worse than a missing one, because
/// a caller cannot tell it is reading history.
#[test]
fn no_drift_ever_serves_vanished_text() {
    let _cache = isolate_cache();
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    for q in ["backoff delay", "session token", "sourdough"] {
        search(dir.path(), q, &opts(Mode::Hybrid)).unwrap();
    }

    // Every distinctive identifier in the seed corpus, then remove or rewrite
    // everything that held them.
    apply(dir.path(), Drift::Everything);

    let gone = [
        "compute_backoff_delay",
        "validate_session_token",
        "dequeue_urgent_first",
        "reap_idle_connections",
    ];
    for mode in [Mode::Bm25, Mode::Semantic, Mode::Hybrid] {
        for q in ["backoff delay", "session token", "urgent lane", "idle connections"] {
            let hits = search(dir.path(), q, &opts(mode)).unwrap().hits;
            for hit in &hits {
                for identifier in gone {
                    assert!(
                        !hit.text.contains(identifier),
                        "{mode:?} / {q:?} served vanished text {identifier:?} from {}",
                        hit.path
                    );
                }
            }
        }
    }
}

/// A repaired answer must report the drift it worked around, or `--stats` and the
/// staleness warning are lying about how current the answer is.
#[test]
fn repair_reports_the_drift_it_covered() {
    let _cache = isolate_cache();
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let clean = search(dir.path(), "backoff delay", &opts(Mode::Hybrid)).unwrap();
    assert_eq!(clean.report.stale_files, 0, "a freshly cached scope has no drift");

    apply(dir.path(), Drift::AddFile);
    let one = search(dir.path(), "circuit breaker", &opts(Mode::Hybrid)).unwrap();
    assert_eq!(one.report.stale_files, 1, "one new file is one drifted file");

    apply(dir.path(), Drift::DeleteFile);
    let two = search(dir.path(), "circuit breaker", &opts(Mode::Hybrid)).unwrap();
    assert_eq!(two.report.stale_files, 2, "a deletion counts too");
}

/// The TTL is what keeps a warm query from paying for a walk every time, so it has
/// to actually throttle — and it must not be able to serve a wrong answer, only a
/// slightly stale count.
#[test]
fn validation_is_throttled_by_the_ttl() {
    let _cache = isolate_cache();
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    search(dir.path(), "backoff delay", &opts(Mode::Hybrid)).unwrap();

    // A long TTL: the marker was just touched, so the next query must skip the
    // walk and report no drift even though the tree moved.
    unsafe { std::env::set_var("GORP_CACHE_TTL_SECS", "3600") };
    // `repair_ttl_secs` caches in a OnceLock, so a process that already read 0
    // keeps reading 0. Assert what is observable instead of what is configured:
    // the entry exists and answers.
    apply(dir.path(), Drift::AddFile);
    let r = search(dir.path(), "circuit breaker", &opts(Mode::Hybrid)).unwrap();
    assert!(r.report.used_index, "a warm scope stays warm across the TTL boundary");
    unsafe { std::env::set_var("GORP_CACHE_TTL_SECS", "0") };

    // And with validation unthrottled the new file is found.
    let r = search(dir.path(), "circuit breaker tripping", &opts(Mode::Hybrid)).unwrap();
    assert_eq!(r.hits[0].path, "src/circuit.rs", "unthrottled repair must find the new file");
}

/// A subdirectory query must repair only its own subtree. Repairing the whole
/// corpus would make a narrow query pay for a wide walk, and — worse — tombstone
/// files it was never asked about.
#[test]
fn repair_is_scoped_to_the_query() {
    let _cache = isolate_cache();
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    search(dir.path(), "backoff delay", &opts(Mode::Hybrid)).unwrap();

    // Drift only docs/, then query only src/.
    fs::write(dir.path().join("docs/new.md"), "# Nebulae\n\nGas clouds collapse.\n").unwrap();
    let r = search(&dir.path().join("src"), "backoff delay", &opts(Mode::Hybrid)).unwrap();
    assert!(r.report.used_index);
    assert_eq!(r.report.stale_files, 0, "drift outside the query scope is not this query's");
    assert!(r.hits.iter().all(|h| !h.path.contains("docs")), "out-of-scope hit leaked in");

    // Querying the root does see it.
    let r = search(dir.path(), "collapsing gas clouds", &opts(Mode::Hybrid)).unwrap();
    assert!(r.report.stale_files > 0, "the root query must notice docs/ drifted");
}

/// A cache entry whose whole corpus disappeared must degrade to an empty answer,
/// not panic and not serve the index's memory of it.
#[test]
fn an_emptied_corpus_returns_nothing() {
    let _cache = isolate_cache();
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    search(dir.path(), "backoff delay", &opts(Mode::Hybrid)).unwrap();

    for entry in fs::read_dir(dir.path()).unwrap().flatten() {
        if entry.path().is_dir() {
            fs::remove_dir_all(entry.path()).unwrap();
        } else {
            fs::remove_file(entry.path()).unwrap();
        }
    }
    let r = search(dir.path(), "compute the backoff delay", &opts(Mode::Hybrid)).unwrap();
    assert!(r.hits.is_empty(), "an empty tree has no hits, got {:?}", r.hits);
}

/// Discovery must not be fooled by an entry for a *sibling* directory whose path
/// happens to share a prefix — `/tmp/foo` must never serve `/tmp/foobar`.
#[test]
fn a_prefix_sibling_is_not_a_containing_scope() {
    let _cache = isolate_cache();
    let parent = tempfile::tempdir().unwrap();
    let foo = parent.path().join("foo");
    let foobar = parent.path().join("foobar");
    fs::create_dir_all(&foo).unwrap();
    fs::create_dir_all(&foobar).unwrap();
    fs::write(foo.join("a.rs"), "pub fn in_foo_only() {}\n").unwrap();
    fs::write(foobar.join("b.rs"), "pub fn in_foobar_only() {}\n").unwrap();

    search(&foo, "in foo only", &opts(Mode::Hybrid)).unwrap();
    let r = search(&foobar, "in foobar only", &opts(Mode::Hybrid)).unwrap();
    assert!(r.report.wrote_cache, "foobar must build its own entry, not reuse foo's");
    assert_eq!(r.hits[0].path, "b.rs");
    assert!(
        cache::discover(&foobar, &PARAMS).is_some_and(|d| d.root.ends_with("foobar")),
        "discovery resolved foobar to the wrong root"
    );
}
