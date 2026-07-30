//! The cold path: one streaming pass over the corpus, no index involved.
//!
//! Bounded memory for the semantic side (a top-k heap, not a matrix); BM25
//! postings are held for the duration of the query.

use super::hit;
use super::{SearchOptions, SearchReport, SearchResult};
use crate::rank::bm25::Bm25Index;
use crate::rank::{self, Mode, TopK};
use crate::{Chunk, corpus, text};
use anyhow::Result;
use std::path::Path;
use std::time::Instant;

/// Chunk texts are embedded in batches this large on the streaming path.
const EMBED_BATCH: usize = 1024;

pub fn run(root: &Path, query: &str, opts: &SearchOptions) -> Result<SearchResult> {
    let want_bm25 = matches!(opts.mode, Mode::Bm25 | Mode::Hybrid);
    let want_sem = matches!(opts.mode, Mode::Semantic | Mode::Hybrid);
    let pool = super::FUSION_POOL.max(opts.k);

    let t_walk = Instant::now();
    let files = corpus::walk(root, &opts.params)?;
    let walk_ms = t_walk.elapsed().as_millis();

    let t_rank = Instant::now();
    let qvec = want_sem.then(|| text::embed_query(query));
    let mut bm25 = want_bm25.then(Bm25Index::new);
    let mut top = TopK::new(pool);
    let mut chunks: Vec<Chunk> = Vec::new();
    let mut pending: Vec<(u32, String)> = Vec::new();

    let ms = |t: Instant| t.elapsed().as_secs_f64() * 1e3;
    let embed_ms = std::cell::Cell::new(0.0f64);
    let flush = |pending: &mut Vec<(u32, String)>, top: &mut TopK| {
        if let Some(q) = &qvec
            && !pending.is_empty()
        {
            let t0 = Instant::now();
            let vecs = ese::encode(pending.iter().map(|(_, s)| s));
            for ((id, _), v) in pending.iter().zip(&vecs) {
                top.push(*id, rank::distance(q, v));
            }
            embed_ms.set(embed_ms.get() + ms(t0));
        }
        pending.clear();
    };

    let t_pass = Instant::now();
    corpus::pass(root, &files, &opts.params, want_sem, want_bm25, |_, work| {
        for (chunk, doc, tokens) in work.docs {
            let chunk_id = chunks.len() as u32;
            chunks.push(chunk);
            if let (Some(b), Some(t)) = (&mut bm25, tokens) {
                b.add_tokenized(t);
            }
            if let Some(doc) = doc {
                pending.push((chunk_id, doc));
                if pending.len() >= EMBED_BATCH {
                    flush(&mut pending, &mut top);
                }
            }
        }
    });
    flush(&mut pending, &mut top);
    let pass_total = ms(t_pass);
    let mut stages: Vec<(String, f64)> = vec![
        ("walk".into(), walk_ms as f64),
        ("pass:embed".into(), embed_ms.get()),
        ("pass:read+tokenize".into(), pass_total - embed_ms.get()),
    ];

    let t0 = Instant::now();
    let bm25_ranked = match &mut bm25 {
        Some(b) => {
            b.finalize();
            b.query(query, pool)
        }
        None => Vec::new(),
    };
    stages.push(("rank:bm25".into(), ms(t0)));
    let sem_ranked = if want_sem { top.into_sorted() } else { Vec::new() };
    let n_chunks = chunks.len();
    let t0 = Instant::now();
    let ranked = rank::fuse(opts.mode, bm25_ranked, sem_ranked, opts.k * 3, opts.sem_weight);
    stages.push(("rank:fuse".into(), ms(t0)));
    let rank_ms = t_rank.elapsed().as_millis();

    let cands: Vec<hit::Candidate> = ranked
        .into_iter()
        .map(|(chunk_id, score)| {
            let chunk = chunks[chunk_id as usize];
            hit::Candidate {
                id: chunk_id,
                chunk,
                path: files[chunk.file_id as usize].path.clone(),
                score,
            }
        })
        .collect();
    // No embedding matrix in streaming mode: re-embed candidate chunks on
    // demand (≤ 3k texts, negligible next to the corpus pass just done).
    let t0 = Instant::now();
    let hits = hit::finalize(root, query, cands, opts, "", |c| {
        let fm = &files[c.chunk.file_id as usize];
        let text = corpus::lines(root, &fm.path, &c.chunk)?;
        Some(text::embed_query(&corpus::doc_text(&fm.path, &text)).to_vec())
    });
    stages.push(("finalize".into(), ms(t0)));

    Ok(SearchResult {
        hits,
        report: SearchReport {
            used_index: false,
            used_hnsw: false,
            stale_files: 0,
            n_chunks_considered: n_chunks,
            walk_ms,
            rank_ms,
            stages,
            ..Default::default()
        },
    })
}
