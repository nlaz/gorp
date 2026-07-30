//! The cold path: one streaming pass over the corpus, no index involved.
//!
//! Mirrors `indexed` — rank lexically, rank semantically, fuse, materialize —
//! but computes the representations instead of loading them, and keeps only what
//! a single query needs. The semantic side holds a top-k heap rather than a
//! matrix, so its memory is bounded by k; BM25 postings are held for the
//! duration of the query, which is the larger cost and is why a big corpus wants
//! an index.

use super::trace::Trace;
use super::{SearchOptions, SearchReport, SearchResult, hit};
use crate::rank::bm25::Bm25Index;
use crate::rank::{self, Mode, TopK};
use crate::{Chunk, FileMeta, corpus, text};
use anyhow::Result;
use std::path::Path;
use std::time::Instant;

pub fn run(root: &Path, query: &str, opts: &SearchOptions) -> Result<SearchResult> {
    let pool = super::FUSION_POOL.max(opts.k);
    let mut trace = Trace::new();

    let t_walk = Instant::now();
    let files = corpus::walk(root, &opts.params)?;
    let walk_ms = t_walk.elapsed().as_millis();
    trace.record("walk", walk_ms as f64);

    let t_rank = Instant::now();
    let pass = corpus_pass(root, &files, query, opts, pool, &mut trace);
    let lexical = trace.time("rank:bm25", || match pass.bm25 {
        Some(mut b) => {
            b.finalize();
            b.query(query, pool)
        }
        None => Vec::new(),
    });
    let semantic = pass.nearest.map(TopK::into_sorted).unwrap_or_default();
    let ranked = trace.time("rank:fuse", || {
        rank::fuse(opts.mode, lexical, semantic, opts.k * 3, opts.sem_weight)
    });
    let rank_ms = t_rank.elapsed().as_millis();

    let cands = candidates(ranked, &pass.chunks, &files);
    // No embedding matrix here, so candidate vectors are recomputed on demand.
    // At most a few thousand texts, which is nothing beside the pass just done.
    let hits = trace.time("finalize", || {
        hit::finalize(root, query, cands, opts, "", |c| {
            let fm = &files[c.chunk.file_id as usize];
            let text = corpus::lines(root, &fm.path, &c.chunk)?;
            Some(text::embed_query(&corpus::doc_text(&fm.path, &text)).to_vec())
        })
    });

    Ok(SearchResult {
        report: SearchReport {
            used_index: false,
            n_chunks_considered: pass.chunks.len(),
            walk_ms,
            rank_ms,
            stages: trace.into_stages(),
            ..Default::default()
        },
        hits,
    })
}

/// What one streaming pass retained: the chunk table, and whichever engines this
/// mode asked for.
struct Pass {
    chunks: Vec<Chunk>,
    bm25: Option<Bm25Index>,
    /// The k nearest chunks seen so far. A heap, not a matrix — this is what
    /// keeps a cold semantic search's memory independent of corpus size.
    nearest: Option<TopK>,
}

/// Read, chunk, tokenize, and embed the corpus, keeping only the query's answer.
fn corpus_pass(
    root: &Path,
    files: &[FileMeta],
    query: &str,
    opts: &SearchOptions,
    pool: usize,
    trace: &mut Trace,
) -> Pass {
    let want_bm25 = matches!(opts.mode, Mode::Bm25 | Mode::Hybrid);
    let want_sem = matches!(opts.mode, Mode::Semantic | Mode::Hybrid);

    let mut chunks: Vec<Chunk> = Vec::new();
    let mut bm25 = want_bm25.then(Bm25Index::new);
    let mut embedder = want_sem.then(|| Embedder::new(text::embed_query(query), pool));

    let t_pass = Instant::now();
    corpus::pass(root, files, &opts.params, want_sem, want_bm25, |_, work| {
        for (chunk, doc, tokens) in work.docs {
            // Lockstep: the chunk id is this chunk's position in the table, and
            // the BM25 document and the embedding are queued under the same id.
            let chunk_id = chunks.len() as u32;
            chunks.push(chunk);
            if let (Some(b), Some(t)) = (&mut bm25, tokens) {
                b.add_tokenized(t);
            }
            if let (Some(e), Some(doc)) = (&mut embedder, doc) {
                e.push(chunk_id, doc);
            }
        }
    });
    let (nearest, embed_ms) = match embedder {
        Some(e) => {
            let (top, ms) = e.finish();
            (Some(top), ms)
        }
        None => (None, 0.0),
    };
    // Embedding is separated from reading and tokenizing because they scale
    // differently: one is CPU-bound in ese, the other IO-bound in the walk.
    let total = t_pass.elapsed().as_secs_f64() * 1e3;
    trace.record("pass:embed", embed_ms);
    trace.record("pass:read+tokenize", total - embed_ms);

    Pass { chunks, bm25, nearest }
}

/// Batches chunk texts, embeds them, and keeps the k nearest to the query.
struct Embedder {
    query: [f32; crate::EMBED_DIM],
    pending: Vec<(u32, String)>,
    nearest: TopK,
    embed_ms: f64,
}

impl Embedder {
    /// Texts per batch. `ese` parallelizes internally above 16, and batching is
    /// what bounds resident text regardless of corpus size.
    const BATCH: usize = 1024;

    fn new(query: [f32; crate::EMBED_DIM], k: usize) -> Self {
        Self {
            query,
            pending: Vec::with_capacity(Self::BATCH),
            nearest: TopK::new(k),
            embed_ms: 0.0,
        }
    }

    fn push(&mut self, chunk_id: u32, doc: String) {
        self.pending.push((chunk_id, doc));
        if self.pending.len() >= Self::BATCH {
            self.flush();
        }
    }

    /// The heap, and the milliseconds spent embedding.
    fn finish(mut self) -> (TopK, f64) {
        self.flush();
        (self.nearest, self.embed_ms)
    }

    fn flush(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let start = Instant::now();
        let vecs = ese::encode(self.pending.iter().map(|(_, text)| text));
        for ((id, _), v) in self.pending.iter().zip(&vecs) {
            self.nearest.push(*id, rank::distance(&self.query, v));
        }
        self.embed_ms += start.elapsed().as_secs_f64() * 1e3;
        self.pending.clear();
    }
}

/// Fused ids into candidates. No scope filter: the cold path walks exactly the
/// queried subtree, so everything it ranked is in scope by construction.
fn candidates(
    ranked: Vec<(u32, f32)>,
    chunks: &[Chunk],
    files: &[FileMeta],
) -> Vec<hit::Candidate> {
    ranked
        .into_iter()
        .map(|(id, score)| {
            let chunk = chunks[id as usize];
            hit::Candidate {
                id,
                chunk,
                path: files[chunk.file_id as usize].path.clone(),
                score,
            }
        })
        .collect()
}
