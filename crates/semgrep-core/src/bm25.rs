//! BM25 index over chunks, usable serialized-from-disk or built in memory
//! during a cold (unindexed) search pass.

use crate::tokenize;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

const K1: f32 = 1.2;
const B: f32 = 0.75;

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
    TokenizedDoc { terms: tf.into_iter().collect(), len }
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

    /// Postings are appended per-doc so within a term they are ascending
    /// already; sort defensively after deserialize or parallel merges.
    pub fn finalize(&mut self) {
        for p in &mut self.postings {
            p.sort_unstable_by_key(|&(c, _)| c);
        }
    }

    /// Top-k chunks by BM25. Returns (chunk_id, score) descending.
    pub fn query(&self, query: &str, k: usize) -> Vec<(u32, f32)> {
        let n = self.doc_len.len() as f32;
        if n == 0.0 {
            return Vec::new();
        }
        let avgdl = (self.total_len as f32 / n).max(1.0);
        // Dedup query terms, keeping multiplicity as a weight.
        let mut qterms: HashMap<u32, f32> = HashMap::new();
        tokenize::for_each_token(query, |tok| {
            if let Some(&id) = self.terms.get(tok) {
                *qterms.entry(id).or_insert(0.0) += 1.0;
            }
        });
        let mut scores: HashMap<u32, f32> = HashMap::new();
        for (&term_id, &qweight) in &qterms {
            let plist = &self.postings[term_id as usize];
            let df = plist.len() as f32;
            let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();
            for &(chunk_id, tf) in plist {
                let tf = tf as f32;
                let dl = self.doc_len[chunk_id as usize] as f32;
                let denom = tf + K1 * (1.0 - B + B * dl / avgdl);
                *scores.entry(chunk_id).or_insert(0.0) +=
                    qweight * idf * tf * (K1 + 1.0) / denom;
            }
        }
        let mut hits: Vec<(u32, f32)> = scores.into_iter().collect();
        hits.sort_unstable_by(|a, b| {
            b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(&b.0))
        });
        hits.truncate(k);
        hits
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

        let align8 = |v: &mut Vec<u8>| while v.len() % 8 != 0 { v.push(0) };
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
    fn doc_len(&self, chunk_id: u32) -> u32 {
        self.u32_le(self.off[4] as usize + chunk_id as usize * 4)
    }

    /// Document frequency of a term (0 if absent) — for idf-style scoring
    /// outside the BM25 query itself (e.g. PRF expansion-term selection).
    pub fn df(&self, term: &str) -> usize {
        self.find_term(term).map_or(0, |i| self.postings_at(i).count())
    }

    /// Top-k chunks by BM25 — same scoring as [`Bm25Index::query`].
    pub fn query(&self, query: &str, k: usize) -> Vec<(u32, f32)> {
        let n = self.n_docs as f32;
        if n == 0.0 {
            return Vec::new();
        }
        let avgdl = (self.total_len as f32 / n).max(1.0);
        let mut qterms: HashMap<usize, f32> = HashMap::new();
        tokenize::for_each_token(query, |tok| {
            if let Some(i) = self.find_term(tok) {
                *qterms.entry(i).or_insert(0.0) += 1.0;
            }
        });
        let mut scores: HashMap<u32, f32> = HashMap::new();
        for (&term_i, &qweight) in &qterms {
            let df = self.postings_at(term_i).count() as f32;
            let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();
            for (chunk_id, tf) in self.postings_at(term_i) {
                let tf = tf as f32;
                let dl = self.doc_len(chunk_id) as f32;
                let denom = tf + K1 * (1.0 - B + B * dl / avgdl);
                *scores.entry(chunk_id).or_insert(0.0) +=
                    qweight * idf * tf * (K1 + 1.0) / denom;
            }
        }
        let mut hits: Vec<(u32, f32)> = scores.into_iter().collect();
        hits.sort_unstable_by(|a, b| {
            b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(&b.0))
        });
        hits.truncate(k);
        hits
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
        let i = idx(&[
            "alpha beta beta beta",
            "alpha gamma",
            "alpha delta",
        ]);
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
            let a = i.query(q, 10);
            let b = f.query(q, 10);
            assert_eq!(a.len(), b.len(), "query {q:?}");
            for ((ca, sa), (cb, sb)) in a.iter().zip(&b) {
                assert_eq!(ca, cb, "query {q:?}");
                assert!((sa - sb).abs() < 1e-5, "query {q:?}: {sa} vs {sb}");
            }
        }
    }
}
