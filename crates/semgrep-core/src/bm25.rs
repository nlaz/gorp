//! BM25 index over chunks, usable serialized-from-disk or built in memory
//! during a cold (unindexed) search pass.

use crate::tokenize;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

const K1: f32 = 1.2;
const B: f32 = 0.75;

/// What BM25 scoring needs from a term store, so the formula is written once.
///
/// Two stores implement it: [`Bm25Index`] (in-memory, built during a cold pass
/// or a repair overlay) and [`FlatBm25`] (mmap'd, read from `bm25.flat`). They
/// used to carry a copy of the scoring loop each, and their agreement was
/// asserted by one fixture test with a 1e-5 tolerance — which was also hiding
/// the fact that they accumulated in different orders.
pub trait Postings {
    fn n_docs(&self) -> usize;
    fn total_len(&self) -> u64;
    /// Term id for a token, if this store knows it. Ids order the accumulation,
    /// so a store must return them consistently for a given build.
    fn term_id(&self, token: &str) -> Option<u32>;
    /// (chunk_id, term frequency) for a term, chunk_ids ascending.
    fn postings(&self, term: u32) -> impl Iterator<Item = (u32, u16)> + '_;
    fn doc_len(&self, chunk_id: u32) -> u32;

    /// Document frequency — how many chunks contain the term.
    fn df(&self, token: &str) -> usize {
        self.term_id(token).map_or(0, |t| self.postings(t).count())
    }
}

/// Okapi BM25 over any [`Postings`] store. Returns (chunk_id, score), best
/// first, ties broken by chunk id so the order is total.
pub fn top_k<P: Postings>(store: &P, query: &str, k: usize) -> Vec<(u32, f32)> {
    let n = store.n_docs() as f32;
    if n == 0.0 || k == 0 {
        return Vec::new();
    }
    let avgdl = (store.total_len() as f32 / n).max(1.0);

    // Dedup query terms, keeping multiplicity as a weight.
    let mut weights: HashMap<u32, f32> = HashMap::new();
    tokenize::for_each_token(query, |tok| {
        if let Some(id) = store.term_id(tok) {
            *weights.entry(id).or_insert(0.0) += 1.0;
        }
    });
    // Accumulate in term-id order, not hash order: f32 addition is not
    // associative, so iterating the map would score the same document
    // differently from run to run and let near-tied chunks swap rank.
    let mut terms: Vec<(u32, f32)> = weights.into_iter().collect();
    terms.sort_unstable_by_key(|&(id, _)| id);

    let mut scores: HashMap<u32, f32> = HashMap::new();
    for (term, weight) in terms {
        let df = store.postings(term).count() as f32;
        let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();
        for (chunk_id, tf) in store.postings(term) {
            let tf = tf as f32;
            let dl = store.doc_len(chunk_id) as f32;
            let denom = tf + K1 * (1.0 - B + B * dl / avgdl);
            *scores.entry(chunk_id).or_insert(0.0) += weight * idf * tf * (K1 + 1.0) / denom;
        }
    }

    let mut hits: Vec<(u32, f32)> = scores.into_iter().collect();
    hits.sort_unstable_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    hits.truncate(k);
    hits
}

#[derive(Default, Serialize, Deserialize)]
pub struct Bm25Index {
    /// term -> term_id
    terms: HashMap<String, u32>,
    /// term_id -> (chunk_id, term frequency), chunk_ids ascending
    postings: Vec<Vec<(u32, u16)>>,
    /// chunk_id -> token count
    doc_len: Vec<u32>,
    total_len: u64,
}

/// One document tokenized into (term, tf) pairs — the CPU-heavy half of
/// `add_doc`, split out so it can run on worker threads while the cheap
/// postings merge stays serial (chunk-id assignment must be deterministic).
pub struct TokenizedDoc {
    terms: Vec<(String, u16)>,
    len: u32,
}

/// Tokenize a document off-thread. Feed the result to
/// [`Bm25Index::add_tokenized`] in chunk order.
pub fn tokenize_doc(text: &str) -> TokenizedDoc {
    let mut tf: HashMap<String, u16> = HashMap::new();
    let mut len = 0u32;
    tokenize::for_each_token(text, |tok| {
        len += 1;
        // get_mut-then-insert avoids an owned-key allocation per occurrence.
        match tf.get_mut(tok) {
            Some(e) => *e = e.saturating_add(1),
            None => {
                tf.insert(tok.to_string(), 1);
            }
        }
    });
    // Sorted, not hash order: `add_tokenized` assigns term ids in the order it
    // first sees each term, so draining the map directly would give the same
    // corpus a different term table every run — and with it a different score
    // accumulation order and different serialized bytes.
    let mut terms: Vec<(String, u16)> = tf.into_iter().collect();
    terms.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    TokenizedDoc { terms, len }
}

impl Bm25Index {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn n_docs(&self) -> usize {
        self.doc_len.len()
    }

    /// Add the next chunk's text. Chunk ids are assigned sequentially in call
    /// order and must match the caller's chunk table.
    pub fn add_doc(&mut self, text: &str) -> u32 {
        self.add_tokenized(tokenize_doc(text))
    }

    /// Merge a pre-tokenized document (see [`tokenize_doc`]). Serial and
    /// cheap: postings appends and id bookkeeping only.
    pub fn add_tokenized(&mut self, doc: TokenizedDoc) -> u32 {
        let chunk_id = self.doc_len.len() as u32;
        for (term, freq) in doc.terms {
            let term_id = match self.terms.get(&term) {
                Some(&id) => id,
                None => {
                    let id = self.terms.len() as u32;
                    self.terms.insert(term, id);
                    self.postings.push(Vec::new());
                    id
                }
            };
            self.postings[term_id as usize].push((chunk_id, freq));
        }
        self.doc_len.push(doc.len);
        self.total_len += doc.len as u64;
        chunk_id
    }

    /// Canonicalize the index. Must be called before querying.
    ///
    /// Two jobs. Postings are appended per-doc so within a term they are
    /// ascending already; sorting is defensive after a deserialize or a merge.
    ///
    /// More importantly, term ids are renumbered into sorted-term order. Ids
    /// are handed out here in first-appearance order, but `FlatBm25` numbers
    /// terms by position in its sorted table — and [`top_k`] accumulates in
    /// term-id order, which f32 addition makes significant. Without this the
    /// cold path (this store) and the warm path (the flat one) computed scores
    /// differing in the last bits, so a pair of near-tied chunks could come
    /// back in one order cold and the other warm. The two stores now number
    /// terms identically, which makes their agreement structural rather than
    /// something a test has to allow slack for.
    pub fn finalize(&mut self) {
        let mut by_name: Vec<(String, u32)> = self.terms.drain().collect();
        by_name.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        let mut renumbered: Vec<Vec<(u32, u16)>> = Vec::with_capacity(by_name.len());
        for (new_id, (term, old_id)) in by_name.into_iter().enumerate() {
            renumbered.push(std::mem::take(&mut self.postings[old_id as usize]));
            self.terms.insert(term, new_id as u32);
        }
        self.postings = renumbered;
        for p in &mut self.postings {
            p.sort_unstable_by_key(|&(c, _)| c);
        }
    }

    /// Top-k chunks by BM25. Returns (chunk_id, score) descending.
    pub fn query(&self, query: &str, k: usize) -> Vec<(u32, f32)> {
        top_k(self, query, k)
    }
}

impl Postings for Bm25Index {
    fn n_docs(&self) -> usize {
        self.doc_len.len()
    }

    fn total_len(&self) -> u64 {
        self.total_len
    }

    fn term_id(&self, token: &str) -> Option<u32> {
        self.terms.get(token).copied()
    }

    fn postings(&self, term: u32) -> impl Iterator<Item = (u32, u16)> + '_ {
        self.postings[term as usize].iter().copied()
    }

    fn doc_len(&self, chunk_id: u32) -> u32 {
        self.doc_len[chunk_id as usize]
    }
}

// ---------------------------------------------------------------------------
// Flat on-disk format: mmap-able, zero deserialization.
//
// Provenance showed postcard-deserializing bm25.bin (319 MB on the kernel)
// costs ~840 ms per warm query — 21% of a hybrid query. The flat layout is
// binary-searched in place; a query touches only its own terms' postings.
//
// Layout (little-endian, sections 8-aligned):
//   [0..8)   magic "SGBM25F2"
//   u32 n_docs, u32 n_terms, u64 total_len
//   u64 off_term_offs   -> (n_terms+1) x u32 into term_bytes
//   u64 off_term_bytes  -> sorted term strings, concatenated
//   u64 off_post_offs   -> (n_terms+1) x u64 into postings
//   u64 off_postings    -> per term: (u32 chunk_id, u16 tf) records
//   u64 off_doc_len     -> n_docs x u32
// ---------------------------------------------------------------------------

const FLAT_MAGIC: &[u8; 8] = b"SGBM25F2";

impl Bm25Index {
    /// Serialize into the flat mmap-able layout.
    pub fn to_flat_bytes(&self) -> Vec<u8> {
        let mut terms: Vec<(&str, u32)> =
            self.terms.iter().map(|(s, &id)| (s.as_str(), id)).collect();
        terms.sort_unstable_by_key(|&(s, _)| s);
        let n_terms = terms.len();

        let mut term_offs: Vec<u32> = Vec::with_capacity(n_terms + 1);
        let mut term_bytes: Vec<u8> = Vec::new();
        let mut post_offs: Vec<u64> = Vec::with_capacity(n_terms + 1);
        let mut postings: Vec<u8> = Vec::new();
        for &(s, id) in &terms {
            term_offs.push(term_bytes.len() as u32);
            term_bytes.extend_from_slice(s.as_bytes());
            post_offs.push(postings.len() as u64);
            for &(chunk, tf) in &self.postings[id as usize] {
                postings.extend_from_slice(&chunk.to_le_bytes());
                postings.extend_from_slice(&tf.to_le_bytes());
            }
        }
        term_offs.push(term_bytes.len() as u32);
        post_offs.push(postings.len() as u64);

        let align8 = |v: &mut Vec<u8>| v.resize(v.len().next_multiple_of(8), 0);
        let mut out = Vec::new();
        out.extend_from_slice(FLAT_MAGIC);
        out.extend_from_slice(&(self.doc_len.len() as u32).to_le_bytes());
        out.extend_from_slice(&(n_terms as u32).to_le_bytes());
        out.extend_from_slice(&self.total_len.to_le_bytes());
        // reserve section-offset slots
        let off_slots = out.len();
        out.extend_from_slice(&[0u8; 40]);

        let mut offsets = [0u64; 5];
        let sections: [&[u8]; 5] = [
            bytemuck_cast_u32(&term_offs),
            &term_bytes,
            bytemuck_cast_u64(&post_offs),
            &postings,
            bytemuck_cast_u32(&self.doc_len),
        ];
        // Two-pass not needed: write sequentially, recording aligned starts.
        let mut buf = out;
        for (i, s) in sections.iter().enumerate() {
            align8(&mut buf);
            offsets[i] = buf.len() as u64;
            buf.extend_from_slice(s);
        }
        for (i, off) in offsets.iter().enumerate() {
            buf[off_slots + i * 8..off_slots + (i + 1) * 8].copy_from_slice(&off.to_le_bytes());
        }
        buf
    }
}

fn bytemuck_cast_u32(v: &[u32]) -> &[u8] {
    unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, v.len() * 4) }
}
fn bytemuck_cast_u64(v: &[u64]) -> &[u8] {
    unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, v.len() * 8) }
}

/// Reader over the flat layout. Holds the mmap; all access is in place.
pub struct FlatBm25 {
    map: memmap2::Mmap,
    n_docs: u32,
    n_terms: u32,
    total_len: u64,
    off: [u64; 5],
}

impl FlatBm25 {
    pub fn open(path: &std::path::Path) -> anyhow::Result<Self> {
        let file = std::fs::File::open(path)?;
        let map = unsafe { memmap2::Mmap::map(&file)? };
        anyhow::ensure!(map.len() >= 64 && &map[..8] == FLAT_MAGIC, "bad bm25.flat header");
        let u32_at = |o: usize| u32::from_le_bytes(map[o..o + 4].try_into().unwrap());
        let u64_at = |o: usize| u64::from_le_bytes(map[o..o + 8].try_into().unwrap());
        let n_docs = u32_at(8);
        let n_terms = u32_at(12);
        let total_len = u64_at(16);
        let mut off = [0u64; 5];
        for (i, o) in off.iter_mut().enumerate() {
            *o = u64_at(24 + i * 8);
        }
        Ok(Self { map, n_docs, n_terms, total_len, off })
    }

    pub fn n_docs(&self) -> usize {
        self.n_docs as usize
    }

    #[inline]
    fn u32_le(&self, byte_off: usize) -> u32 {
        u32::from_le_bytes(self.map[byte_off..byte_off + 4].try_into().unwrap())
    }

    #[inline]
    fn term_at(&self, i: usize) -> &[u8] {
        let base = self.off[0] as usize;
        let lo = self.u32_le(base + i * 4) as usize;
        let hi = self.u32_le(base + (i + 1) * 4) as usize;
        let tb = self.off[1] as usize;
        &self.map[tb + lo..tb + hi]
    }

    fn find_term(&self, term: &str) -> Option<usize> {
        let (mut lo, mut hi) = (0usize, self.n_terms as usize);
        while lo < hi {
            let mid = (lo + hi) / 2;
            match self.term_at(mid).cmp(term.as_bytes()) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Some(mid),
            }
        }
        None
    }

    /// (chunk_id, tf) postings for the i-th sorted term.
    fn postings_at(&self, i: usize) -> impl Iterator<Item = (u32, u16)> + '_ {
        let base = self.off[2] as usize;
        let u64_at = |o: usize| u64::from_le_bytes(self.map[o..o + 8].try_into().unwrap());
        let lo = u64_at(base + i * 8) as usize;
        let hi = u64_at(base + (i + 1) * 8) as usize;
        let pb = self.off[3] as usize;
        self.map[pb + lo..pb + hi].chunks_exact(6).map(|rec| {
            (
                u32::from_le_bytes(rec[..4].try_into().unwrap()),
                u16::from_le_bytes(rec[4..6].try_into().unwrap()),
            )
        })
    }

    #[inline]
    fn doc_len_at(&self, chunk_id: u32) -> u32 {
        self.u32_le(self.off[4] as usize + chunk_id as usize * 4)
    }

    /// Top-k chunks by BM25 — the same scorer [`Bm25Index::query`] uses.
    pub fn query(&self, query: &str, k: usize) -> Vec<(u32, f32)> {
        top_k(self, query, k)
    }
}

impl Postings for FlatBm25 {
    fn n_docs(&self) -> usize {
        self.n_docs as usize
    }

    fn total_len(&self) -> u64 {
        self.total_len
    }

    /// Term ids here are positions in the sorted term table, so they order the
    /// accumulation by term string — the same relation the in-memory store now
    /// has, which is why the two agree bit for bit.
    fn term_id(&self, token: &str) -> Option<u32> {
        self.find_term(token).map(|i| i as u32)
    }

    fn postings(&self, term: u32) -> impl Iterator<Item = (u32, u16)> + '_ {
        self.postings_at(term as usize)
    }

    fn doc_len(&self, chunk_id: u32) -> u32 {
        self.doc_len_at(chunk_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idx(docs: &[&str]) -> Bm25Index {
        let mut i = Bm25Index::new();
        for d in docs {
            i.add_doc(d);
        }
        i.finalize();
        i
    }

    #[test]
    fn ranks_matching_doc_first() {
        let i = idx(&[
            "the cat sat on the mat",
            "dogs chase cats in the yard",
            "quantum entanglement of photons",
        ]);
        let hits = i.query("cat mat", 10);
        assert_eq!(hits[0].0, 0);
        assert!(hits[0].1 > hits.get(1).map(|h| h.1).unwrap_or(0.0));
    }

    #[test]
    fn idf_prefers_rare_terms() {
        let i = idx(&["alpha beta beta beta", "alpha gamma", "alpha delta"]);
        // "gamma" is rarer than "alpha"; doc 1 should win a mixed query.
        let hits = i.query("alpha gamma", 10);
        assert_eq!(hits[0].0, 1);
    }

    #[test]
    fn code_identifiers_match_nl_query() {
        let i = idx(&[
            "fn parse_config_file(path: &Path) -> Config",
            "fn render_frame(buf: &mut Buffer)",
        ]);
        let hits = i.query("parse the config file", 10);
        assert_eq!(hits[0].0, 0);
    }

    #[test]
    fn roundtrips_through_postcard() {
        let i = idx(&["hello world", "goodbye world"]);
        let bytes = postcard::to_allocvec(&i).unwrap();
        let j: Bm25Index = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(j.query("hello", 5)[0].0, 0);
        assert_eq!(j.n_docs(), 2);
    }

    /// Scores must be bit-identical across queries built in a different token
    /// order. They were not: accumulation followed `HashMap` iteration order,
    /// which is seeded per process, so near-tied chunks swapped rank at random
    /// between runs and no output could be snapshot-compared.
    #[test]
    fn scores_do_not_depend_on_term_order() {
        let i = idx(&[
            "alpha beta gamma delta epsilon",
            "alpha alpha beta zeta",
            "gamma delta delta eta theta",
            "epsilon zeta eta alpha beta gamma",
        ]);
        let baseline = i.query("alpha beta gamma delta epsilon zeta eta", 10);
        for permuted in [
            "eta zeta epsilon delta gamma beta alpha",
            "gamma alpha eta beta delta zeta epsilon",
            "delta epsilon alpha eta beta gamma zeta",
        ] {
            let got = i.query(permuted, 10);
            assert_eq!(got, baseline, "query {permuted:?} scored differently");
        }
    }

    #[test]
    fn empty_query_and_empty_index() {
        let i = idx(&["something"]);
        assert!(i.query("зздщ", 5).is_empty());
        let e = Bm25Index::new();
        assert!(e.query("anything", 5).is_empty());
    }
}

#[cfg(test)]
mod flat_tests {
    use super::*;

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
            i.to_flat_bytes()
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
        std::fs::write(&p, i.to_flat_bytes()).unwrap();
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
            std::fs::write(&p, mem.to_flat_bytes()).unwrap();
            let flat = FlatBm25::open(&p).unwrap();

            assert_eq!(mem.n_docs(), flat.n_docs(), "trial {trial}");
            for _ in 0..12 {
                let n_terms = 1 + next() % 4;
                let mut q: Vec<&str> =
                    (0..n_terms).map(|_| vocab[next() % vocab.len()]).collect();
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
}
