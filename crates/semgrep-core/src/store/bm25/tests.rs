//! Flat-layout tests: parity with the in-memory store, and reproducibility.

use super::*;
use crate::rank::bm25::Bm25Index;

/// Same documents in, same bytes out. Term ids used to be assigned in
/// `HashMap` drain order, so two indexes over one corpus disagreed on the
/// term table — invisible through the query API, but it made the serialized
/// index, and every score that depended on accumulation order, irreproducible.
#[test]
fn identical_corpora_serialize_identically() {
    let docs = [
        "src/queue.rs\nfn dequeue_urgent_first(lane: Priority) -> Option<Job>",
        "src/retry.rs\nfn compute_backoff_delay(attempt: u32) -> Duration",
        "docs/ops.md\ndrain the worker before restarting it",
    ];
    let build = || {
        let mut i = Bm25Index::new();
        for d in docs {
            i.add_doc(d);
        }
        i.finalize();
        to_flat_bytes(&i)
    };
    assert_eq!(build(), build(), "index serialization must be reproducible");
}

#[test]
fn flat_matches_in_memory() {
    let mut i = Bm25Index::new();
    for d in [
        "src/auth.rs\nfn validate_session_token(token: &str)",
        "src/retry.rs\nfn compute_backoff_delay(attempt: u32)",
        "docs/cooking.md\nknead the dough and bake sourdough bread",
    ] {
        i.add_doc(d);
    }
    i.finalize();
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("bm25.flat");
    std::fs::write(&p, to_flat_bytes(&i)).unwrap();
    let f = FlatBm25::open(&p).unwrap();
    assert_eq!(f.n_docs(), 3);
    for q in ["validate the session token", "backoff delay", "bake bread", "zzz missing"] {
        // Exactly equal, not within a tolerance. Both stores run the same
        // scorer over term-id order, and in the flat store term ids *are*
        // sorted-term positions, so the float accumulation is identical.
        // The old 1e-5 slack was hiding two different accumulation orders.
        assert_eq!(i.query(q, 10), f.query(q, 10), "query {q:?}");
    }
}

/// Store parity as a property, over corpora big enough to have deep
/// postings lists, terms shared across many documents, and queries that
/// miss. One hand-written fixture cannot cover the interesting cases:
/// binary search near term-table boundaries, multi-term accumulation,
/// documents of very different lengths driving the length normalization.
#[test]
fn stores_agree_on_generated_corpora() {
    let mut state = 0x5EEDu64;
    let mut next = move || {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        (state >> 33) as usize
    };
    let vocab = [
        "retry",
        "backoff",
        "jitter",
        "queue",
        "urgent",
        "token",
        "session",
        "digest",
        "pool",
        "connection",
        "idle",
        "reap",
        "histogram",
        "quantile",
        "bucket",
        "cron",
        "schedule",
        "dispatch",
        "handler",
        "drain",
        "ring",
        "buffer",
        "mask",
        "atomic",
    ];

    for trial in 0..8 {
        let n_docs = 1 + next() % 60;
        let docs: Vec<String> = (0..n_docs)
            .map(|d| {
                let n_words = 1 + next() % 40;
                let body: Vec<&str> =
                    (0..n_words).map(|_| vocab[next() % vocab.len()]).collect();
                // A path prefix, as real chunk docs have.
                format!("src/mod{}/file{}.rs\n{}", d % 4, d, body.join(" "))
            })
            .collect();

        let mut mem = Bm25Index::new();
        for d in &docs {
            mem.add_doc(d);
        }
        mem.finalize();
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("bm25.flat");
        std::fs::write(&p, to_flat_bytes(&mem)).unwrap();
        let flat = FlatBm25::open(&p).unwrap();

        assert_eq!(mem.n_docs(), flat.n_docs(), "trial {trial}");
        for _ in 0..12 {
            let n_terms = 1 + next() % 4;
            let mut q: Vec<&str> = (0..n_terms).map(|_| vocab[next() % vocab.len()]).collect();
            if next() % 4 == 0 {
                q.push("nonexistentterm"); // must be ignored identically
            }
            let query = q.join(" ");
            assert_eq!(
                mem.query(&query, 10),
                flat.query(&query, 10),
                "trial {trial}, query {query:?}"
            );
            // And df must agree, since PRF term selection reads it.
            for term in &q {
                assert_eq!(mem.df(term), flat.df(term), "df({term:?}) trial {trial}");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Corruption. `FlatBm25` is the one artifact in the index format that is not
// size-checked by its own container: `emb.bin` is checked in `store::load`,
// `chunks.bin` by postcard, `meta.json` by serde. It validated only its magic
// and `len >= 64` — which is *exactly* its own header length, so a file
// truncated to precisely that passed and then indexed the map with five
// section offsets pointing past the end of it.
//
// The failure this must return `Err` for is not "a wrong answer". `search()`
// deletes an unreadable cache entry and answers from the streaming path, but
// only on `Err`; a panic bypasses that, leaves the entry in place, and takes
// out the next invocation too. Simulation testing measured it as three
// consecutive runs at exit 101 with zero hits (SIMULATION.md §1.1).
// ---------------------------------------------------------------------------

fn sample_bytes() -> Vec<u8> {
    let mut i = Bm25Index::new();
    for d in [
        "src/queue.rs\nfn dequeue_urgent_first(lane: Priority) -> Option<Job>",
        "src/retry.rs\nfn compute_backoff_delay(attempt: u32) -> Duration",
        "src/pool.rs\nfn size_connection_pool(target: usize) -> PoolConfig",
        "docs/ops.md\ndrain the worker before restarting it",
    ] {
        i.add_doc(d);
    }
    i.finalize();
    to_flat_bytes(&i)
}

fn open_bytes(bytes: &[u8]) -> anyhow::Result<FlatBm25> {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("bm25.flat");
    std::fs::write(&p, bytes).unwrap();
    let r = FlatBm25::open(&p);
    // Keep the mapping valid for the caller's lifetime.
    std::mem::forget(dir);
    r
}

#[test]
fn a_file_truncated_to_exactly_its_header_is_an_error() {
    // The regression. 8 magic + 4 + 4 + 8 + 40 = 64, so this is the largest
    // truncation the old check accepted.
    let bytes = sample_bytes();
    assert!(bytes.len() > HEADER_BYTES);
    let err = open_bytes(&bytes[..HEADER_BYTES]).err().expect("must reject");
    assert!(
        format!("{err:#}").contains("corrupt"),
        "expected a corruption error, got: {err:#}"
    );
}

#[test]
fn truncation_at_every_byte_is_an_error_or_a_usable_index_never_a_panic() {
    // Every prefix, not a sampled few: the bug lived at one specific length,
    // and a spot check would have missed it exactly as the original test did.
    let bytes = sample_bytes();
    for cut in 0..bytes.len() {
        match open_bytes(&bytes[..cut]) {
            Err(_) => {}
            Ok(idx) => {
                // If it opens, it must answer without panicking. A truncated
                // file may legitimately still be structurally complete when the
                // cut lands in trailing alignment padding.
                let _ = idx.query("dequeue urgent backoff", 5);
                let _ = idx.n_docs();
            }
        }
    }
}

#[test]
fn garbage_section_offsets_are_an_error() {
    let mut bytes = sample_bytes();
    for i in 0..5 {
        bytes[24 + i * 8..32 + i * 8].copy_from_slice(&u64::MAX.to_le_bytes());
    }
    let err = open_bytes(&bytes).err().expect("must reject");
    assert!(format!("{err:#}").contains("corrupt"), "got: {err:#}");
}

#[test]
fn one_poisoned_offset_at_a_time_is_an_error() {
    // Each section is checked, not just the first one that happens to be read.
    for i in 0..5 {
        let mut bytes = sample_bytes();
        let far = bytes.len() as u64 * 4;
        bytes[24 + i * 8..32 + i * 8].copy_from_slice(&far.to_le_bytes());
        assert!(
            open_bytes(&bytes).is_err(),
            "section {i} offset past the end of the file must be rejected"
        );
    }
}

#[test]
fn an_inflated_term_count_is_an_error() {
    // n_terms drives the size of two tables; a bad one makes both run off the
    // end without any offset being wrong.
    let mut bytes = sample_bytes();
    bytes[12..16].copy_from_slice(&1_000_000u32.to_le_bytes());
    assert!(open_bytes(&bytes).is_err());
}

#[test]
fn an_inflated_doc_count_is_an_error() {
    let mut bytes = sample_bytes();
    bytes[8..12].copy_from_slice(&1_000_000u32.to_le_bytes());
    assert!(open_bytes(&bytes).is_err());
}

#[test]
fn a_valid_index_still_opens_and_answers() {
    // The bounds checks must not reject anything real.
    let idx = open_bytes(&sample_bytes()).ok().expect("a valid index must open");
    assert_eq!(idx.n_docs(), 4);
    assert!(!idx.query("dequeue urgent", 5).is_empty());
}
