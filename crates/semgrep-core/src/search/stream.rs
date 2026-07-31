//! The cold path: one streaming pass over the corpus, no index involved.
//!
//! Mirrors `indexed` — rank lexically, rank semantically, fuse, materialize —
//! but computes the representations instead of loading them, and keeps only what
//! a single query needs. The semantic side holds a top-k heap rather than a
//! matrix, so its memory is bounded by k; BM25 postings are held for the
//! duration of the query, which is the larger cost and is why a big corpus wants
//! an index.

use super::{Prelude, SearchOptions, SearchReport, SearchResult, hit};
use crate::rank::bm25::Bm25Index;
use crate::rank::{self, Mode, TopK};
use crate::trace::{SCHEDULE_COLD, Stage, Trace, elapsed_ms};
use crate::{Chunk, FileMeta, corpus, text};
use anyhow::Result;
use std::path::Path;
use std::time::Instant;

pub fn run(
    root: &Path,
    query: &str,
    opts: &SearchOptions,
    prelude: &Prelude,
) -> Result<SearchResult> {
    let pool = super::FUSION_POOL.max(opts.k);
    let mut trace = Trace::new(SCHEDULE_COLD);
    trace.replay(prelude);

    let files = trace.time(Stage::Walk, || corpus::walk(root, &opts.params))?;

    let pass = corpus_pass(root, &files, query, opts, pool, &mut trace);
    let lexical = trace.time(Stage::RankBm25, || match pass.bm25 {
        Some(mut b) => {
            b.finalize();
            b.query(query, pool)
        }
        None => Vec::new(),
    });
    let semantic = pass.nearest.map(TopK::into_sorted).unwrap_or_default();
    // The MaxSim rerank has to happen on this path too, and before fusion, or
    // cold and warm answer the same question differently — the invariant
    // `cold_and_warm_return_identical_results` exists for. `indexed` reranks
    // here; this mirrors it against the same chunk texts.
    let semantic = if opts.maxsim_post {
        semantic
    } else {
        rerank_maxsim(root, &files, &pass.chunks, query, semantic, opts, &mut trace)
    };
    let ranked = trace.time(Stage::RankFuse, || {
        rank::fuse(opts.mode, lexical, semantic, super::fused_width(pool), opts.sem_weight)
    });
    // Post-fusion placement, mirroring `indexed`. Both paths must rerank at the
    // same point or a cached scope answers differently from an uncached one.
    // Same sign conversion as `indexed::rerank_fused`: fuse emits
    // higher-is-better, blend_head speaks lower-is-better.
    let ranked = if opts.maxsim_post && opts.rerank_maxsim {
        let as_dist: Vec<(u32, f32)> = ranked.iter().map(|&(id, s)| (id, -s)).collect();
        let o = SearchOptions { maxsim_post: false, ..opts.clone() };
        rerank_maxsim(root, &files, &pass.chunks, query, as_dist, &o, &mut trace)
            .into_iter().map(|(id, pd)| (id, -pd)).collect()
    } else {
        ranked
    };

    let cands = trace.time(Stage::Candidates, || {
        candidates(ranked, &pass.chunks, &files, super::candidate_width(opts.k))
    });
    // No embedding matrix here, so candidate vectors are recomputed on demand.
    // At most a few thousand texts, which is nothing beside the pass just done —
    // but it lands in `finalize:vectors`, which is therefore a much larger
    // number cold than warm under the same name.
    let hits = hit::finalize(root, query, cands, opts, "", &mut trace, |c| {
        let fm = &files[c.chunk.file_id as usize];
        let text = corpus::lines(root, &fm.path, &c.chunk)?;
        let embedded = text::embed_query(&corpus::doc_text(&fm.path, &text));
        // Through the index's quantization, so diversity reranking sees the
        // same vectors it would warm. Without this the two paths diversify
        // differently and a cached scope answers a query differently from an
        // uncached one.
        Some(rank::as_stored(&embedded))
    });

    Ok(SearchResult {
        report: SearchReport {
            used_index: false,
            n_chunks_considered: pass.chunks.len(),
            stages: trace.finish(),
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
    // Timed, and under the same name the warm path uses. It sat outside every
    // span before, which is how a whole `ese::encode` call went unattributed on
    // a path whose entire job is embedding.
    let mut embedder = trace.time(Stage::RankEmbedQuery, || {
        want_sem.then(|| Embedder::new(text::embed_query(query), pool))
    });

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
    let total = elapsed_ms(t_pass);
    trace.record(Stage::PassEmbed, embed_ms);
    trace.record(Stage::PassRead, (total - embed_ms).max(0.0));

    Pass { chunks, bm25, nearest }
}

/// Batches chunk texts, embeds them, and keeps the k nearest to the query.
///
/// Scoring goes through the same quantization the index uses, rather than
/// comparing f32 vectors directly. That is what makes a cold answer equal a warm
/// one: the warm path scores `dot_distance_i8` over the i8 rows in `emb.bin`, so
/// a cold path scoring full-precision cosine would rank near-ties differently and
/// "the index is a cache" would be false for semantic search. Quantizing costs a
/// multiply and a round per dimension, against an embedding per chunk.
struct Embedder {
    query: Vec<i8>,
    pending: Vec<(u32, String)>,
    nearest: TopK,
    embed_ms: f64,
}

impl Embedder {
    /// Texts per batch. `ese` parallelizes internally above 16, and batching is
    /// what bounds resident text regardless of corpus size.
    const BATCH: usize = 1024;

    fn new(query: [f32; crate::EMBED_DIM], k: usize) -> Self {
        let mut q = query;
        rank::normalize(&mut q);
        Self {
            query: rank::quantize_i8(&q),
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
        let mut vecs = ese::encode(self.pending.iter().map(|(_, text)| text));
        for ((id, _), v) in self.pending.iter().zip(vecs.iter_mut()) {
            // Exactly what store::build writes into emb.bin, so the two paths
            // score the same numbers.
            rank::normalize(v);
            self.nearest.push(*id, rank::dot_distance_i8(&self.query, &rank::quantize_i8(v)));
        }
        self.embed_ms += elapsed_ms(start);
        self.pending.clear();
    }
}

/// Fused ids into candidates. No scope filter: the cold path walks exactly the
/// queried subtree, so everything it ranked is in scope by construction.
fn candidates(
    ranked: Vec<(u32, f32)>,
    chunks: &[Chunk],
    files: &[FileMeta],
    limit: usize,
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
        .take(limit)
        .collect()
}

/// MaxSim rerank for the cold path, mirroring `indexed::rerank_maxsim`.
///
/// Both paths must apply it, and apply it at the same point (before fusion),
/// or the same query answers differently depending on whether a scope happens
/// to be indexed — which is exactly what `cold_and_warm_return_identical_results`
/// forbids. The chunk text is re-read here rather than held from the pass: the
/// streaming pass deliberately keeps only the answer, not the corpus.
fn rerank_maxsim(
    root: &Path,
    files: &[FileMeta],
    chunks: &[Chunk],
    query: &str,
    ranked: Vec<(u32, f32)>,
    opts: &SearchOptions,
    trace: &mut Trace,
) -> Vec<(u32, f32)> {
    if !opts.rerank_maxsim || ranked.is_empty() {
        return ranked;
    }
    // No SIF stats on the cold path, matching a default-built index.
    let query_tokens = text::token_vectors(query, None);
    if query_tokens.is_empty() {
        return ranked;
    }
    trace.time(Stage::RankMaxsim, || {
        use rayon::prelude::*;
        let head = rank::maxsim_head_size(ranked.len(), opts.k, opts.maxsim_pool);
        let scored: Vec<(u32, f32, f32)> = ranked[..head]
            .par_iter()
            .map(|&(id, dist)| {
                let chunk = &chunks[id as usize];
                let fm = &files[chunk.file_id as usize];
                let sim = corpus::lines(root, &fm.path, chunk)
                    .map(|text| {
                        let doc = text::token_vectors(&corpus::doc_text(&fm.path, &text), None);
                        rank::maxsim(&query_tokens, &doc)
                    })
                    .unwrap_or(f32::NEG_INFINITY);
                (id, sim, dist)
            })
            .collect();
        let mut out = rank::maxsim_blend_head(&scored, opts.maxsim_blend);
        out.extend_from_slice(&ranked[head..]);
        out
    })
}
