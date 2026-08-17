//! BM25: the scorer, and the in-memory store it runs over.
//!
//! The scorer is written once, over the `Postings` trait, because there are two
//! stores: this one (built during a cold pass or a read-repair overlay) and the
//! mmap'd `store::bm25::FlatBm25`. Both must number terms the same way — see
//! `finalize`.

use crate::text::token as tokenize;
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

/// The corpus a store is part of but does not contain.
///
/// BM25's idf and length normalization are corpus-wide quantities. A store that
/// holds only some of the corpus — a read-repair overlay, which holds just the
/// files that drifted — would otherwise compute them over its own handful of
/// documents and produce scores on a different scale from the base index they get
/// merged with. Passing the rest of the corpus in makes the two comparable.
pub struct Rest<'a> {
    pub n_docs: usize,
    pub total_len: u64,
    /// Document frequency of a term in the rest of the corpus.
    pub df: &'a dyn Fn(&str) -> usize,
}

/// Okapi BM25 over any [`Postings`] store. Returns (chunk_id, score), best
/// first, ties broken by chunk id so the order is total.
pub fn top_k<P: Postings>(store: &P, query: &str, k: usize) -> Vec<(u32, f32)> {
    top_k_scoped(store, query, k, None, None)
}

/// [`top_k`] where `store` holds only part of a corpus. See [`Rest`].
pub fn top_k_within<P: Postings>(
    store: &P,
    query: &str,
    k: usize,
    rest: Option<&Rest>,
) -> Vec<(u32, f32)> {
    top_k_scoped(store, query, k, rest, None)
}

/// [`top_k_within`] restricted to the documents `allow` accepts.
///
/// The filter is applied while scores accumulate, not to the finished list: a
/// list truncated to `k` corpus-wide and *then* filtered to a subtree is empty
/// whenever the subtree is not already winning, which is how two directories of
/// tokio returned nothing at all from a fully indexed corpus. Corpus
/// statistics stay global — idf is a property of the corpus, and
/// rescoping it would make a hit in a small directory mean something different
/// from the same hit at the root.
pub fn top_k_scoped<P: Postings>(
    store: &P,
    query: &str,
    k: usize,
    rest: Option<&Rest>,
    allow: Option<&dyn Fn(u32) -> bool>,
) -> Vec<(u32, f32)> {
    // Dedup query terms, keeping multiplicity as a weight. Keyed by token rather
    // than by term id because the corpus-wide df lookup needs the string — and
    // term ids are canonically ordered by token anyway, so the accumulation order
    // this fixes is the same one.
    let mut weights: HashMap<String, f32> = HashMap::new();
    tokenize::for_each_token(query, |tok| {
        if store.term_id(tok).is_some() {
            *weights.entry(tok.to_string()).or_insert(0.0) += 1.0;
        }
    });
    let mut terms: Vec<(String, f32)> = weights.into_iter().collect();
    terms.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    top_k_weighted(store, &terms, k, rest, allow)
}

/// [`top_k_scoped`] with the term weights supplied by the caller instead of
/// derived from a query string — the entry point bridge expansion uses to
/// score its mined terms at reduced weight (§33) rather than PRF's
/// full-weight concatenation. `terms` must be sorted by token: f32 addition
/// is not associative, so the accumulation order is part of the contract
/// (the same determinism rule the string path enforces internally).
pub fn top_k_weighted<P: Postings>(
    store: &P,
    terms: &[(String, f32)],
    k: usize,
    rest: Option<&Rest>,
    allow: Option<&dyn Fn(u32) -> bool>,
) -> Vec<(u32, f32)> {
    let (rest_docs, rest_len) = rest.map_or((0, 0), |r| (r.n_docs, r.total_len));
    let n = (store.n_docs() + rest_docs) as f32;
    if n == 0.0 || k == 0 {
        return Vec::new();
    }
    let avgdl = ((store.total_len() + rest_len) as f32 / n).max(1.0);

    debug_assert!(terms.windows(2).all(|w| w[0].0 <= w[1].0), "terms must be token-sorted");
    let mut scores: HashMap<u32, f32> = HashMap::new();
    for (token, weight) in terms {
        let Some(term) = store.term_id(token) else { continue };
        let df = (store.postings(term).count() + rest.map_or(0, |r| (r.df)(token))) as f32;
        let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();
        for (chunk_id, tf) in store.postings(term) {
            if allow.is_some_and(|f| !f(chunk_id)) {
                continue;
            }
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

    // Serialization view. The flat on-disk layout is a `store` concern, so the
    // writer lives there; it needs to enumerate the whole term table, which
    // `Postings` (a query-time interface) deliberately does not offer. These
    // three keep the fields private rather than opening them to the crate.

    /// (term, term_id) for every term. Sorted by term after `finalize`.
    pub fn term_table(&self) -> impl Iterator<Item = (&str, u32)> {
        self.terms.iter().map(|(t, &id)| (t.as_str(), id))
    }

    pub fn postings_of(&self, term: u32) -> &[(u32, u16)] {
        &self.postings[term as usize]
    }

    pub fn doc_lens(&self) -> &[u32] {
        &self.doc_len
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

#[cfg(test)]
mod tests;
