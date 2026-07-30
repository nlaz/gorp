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
