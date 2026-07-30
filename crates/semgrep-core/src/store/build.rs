//! Building an index: one corpus pass, folded into a chunk table, a BM25 store,
//! and a quantized embedding matrix.

use super::{BuildOptions, BuildStats, FORMAT_VERSION, IndexMeta, index_dir};
use crate::rank::bm25::Bm25Index;
use crate::text::{self, SemgrepHnsw};
use crate::{Chunk, EMBED_DIM, corpus};
use anyhow::{Context, Result};
use std::io::Write;
use std::path::Path;

/// Chunk texts are embedded in batches this large (ese goes parallel >=16).
const EMBED_BATCH: usize = 1024;

/// Build (or fully rebuild) the repo-local `.semgrep/` index for `root`.
pub fn build(
    root: &Path,
    opts: &BuildOptions,
    progress: impl FnMut(usize, usize),
) -> Result<BuildStats> {
    build_at(&index_dir(root), root, opts, progress)
}

/// Build an index describing `root` into an arbitrary directory (repo-local
/// `.semgrep/` or a central cache entry).
pub fn build_at(
    dir: &Path,
    root: &Path,
    opts: &BuildOptions,
    mut progress: impl FnMut(usize, usize),
) -> Result<BuildStats> {
    use rayon::prelude::*;
    let files = corpus::walk(root, &opts.params)?;
    std::fs::create_dir_all(dir)?;
    // Unpublish before touching anything. A rebuild overwrites emb.bin in
    // place, so leaving the old meta.json readable would let a concurrent
    // query pair a stale file table with a half-rewritten matrix. Absent is a
    // miss and costs a streaming pass; mixed is a wrong answer.
    let _ = std::fs::remove_file(dir.join("meta.json"));

    // SIF pre-pass: count corpus token frequencies before any embedding, so
    // weighted pooling has its p(w). Cheap relative to the embed pass.
    let sif: Option<text::SifStats> = opts.sif.then(|| {
        let mut all = text::SifStats::default();
        for (_, batch) in corpus::pass_batches(&files, 256, 16 << 20) {
            let partials: Vec<text::SifStats> = batch
                .par_iter()
                .map(|fm| {
                    let mut s = text::SifStats::default();
                    if let Some(text) = corpus::read_text(&corpus::abs_path(root, fm)) {
                        s.count(&text);
                    }
                    s
                })
                .collect();
            for p in partials {
                all.merge_counts(p);
            }
        }
        all.a = opts.sif_a;
        // Common-component estimation from a file sample: pool one chunk per
        // sampled file with the freshly-counted weights, take the mean. All
        // later embeds (chunks and queries) subtract it via embed_sif.
        if opts.sif_center && !files.is_empty() {
            let stride = (files.len() / 512).max(1);
            let samples: Vec<[f32; EMBED_DIM]> = files
                .iter()
                .step_by(stride)
                .take(512)
                .collect::<Vec<_>>()
                .par_iter()
                .filter_map(|fm| {
                    let text = corpus::read_text(&corpus::abs_path(root, fm))?;
                    let (_, slice) =
                        corpus::chunk_lines(0, &text, &opts.params).into_iter().next()?;
                    Some(text::embed_sif(&corpus::doc_text(&fm.path, slice), &all))
                })
                .collect();
            if !samples.is_empty() {
                let mut mean = vec![0.0f32; EMBED_DIM];
                for s in &samples {
                    for (m, x) in mean.iter_mut().zip(s.iter()) {
                        *m += x;
                    }
                }
                for m in mean.iter_mut() {
                    *m /= samples.len() as f32;
                }
                all.mean = Some(mean);
            }
        }
        all
    });

    let mut bm25 = Bm25Index::new();
    let mut chunks: Vec<Chunk> = Vec::new();
    let mut hnsw = opts.hnsw.then(text::new_hnsw);
    let mut emb_out = std::io::BufWriter::new(
        std::fs::File::create(dir.join("emb.bin")).context("create emb.bin")?,
    );
    let mut stats = BuildStats { n_files: files.len(), ..Default::default() };

    // Batched embedding across file boundaries: texts are owned copies but
    // only one batch is resident at a time.
    let sif_ref = sif.as_ref();
    let mut pending: Vec<String> = Vec::with_capacity(EMBED_BATCH);
    let flush = |pending: &mut Vec<String>,
                 emb_out: &mut std::io::BufWriter<std::fs::File>,
                 hnsw: &mut Option<SemgrepHnsw>|
     -> Result<()> {
        if pending.is_empty() {
            return Ok(());
        }
        let mut vecs = match sif_ref {
            Some(s) => pending.par_iter().map(|doc| text::embed_sif(doc, s)).collect(),
            None => ese::encode(pending.iter()),
        };
        for v in &mut vecs {
            // Unit-normalize, then quantize to i8 for the on-disk matrix.
            crate::rank::normalize(v);
            let q = crate::rank::quantize_i8(v);
            // SAFETY: i8 and u8 share size and alignment (1), so a fully
            // initialized i8 slice is a valid u8 slice of the same length.
            let bytes = unsafe { std::slice::from_raw_parts(q.as_ptr() as *const u8, q.len()) };
            emb_out.write_all(bytes)?;
            if let Some(h) = hnsw {
                h.insert(*v);
            }
        }
        pending.clear();
        Ok(())
    };

    // Errors from the embed flush have to escape the fold closure, which cannot
    // return them: hold the first one and stop feeding it work.
    let mut flush_err: Option<anyhow::Error> = None;
    let n_files = files.len();
    corpus::pass(root, &files, &opts.params, true, true, |i, work| {
        if flush_err.is_some() {
            return;
        }
        progress(i + 1, n_files);
        stats.bytes_indexed += work.bytes;
        for (chunk, doc, tokens) in work.docs {
            bm25.add_tokenized(tokens.expect("build pass tokenizes"));
            chunks.push(chunk);
            pending.push(doc.expect("build pass keeps text"));
            if pending.len() >= EMBED_BATCH
                && let Err(e) = flush(&mut pending, &mut emb_out, &mut hnsw)
            {
                flush_err = Some(e);
                return;
            }
        }
    });
    if let Some(e) = flush_err {
        return Err(e);
    }
    flush(&mut pending, &mut emb_out, &mut hnsw)?;
    emb_out.flush()?;
    bm25.finalize();
    stats.n_chunks = chunks.len();

    let meta = IndexMeta {
        version: FORMAT_VERSION,
        dims: EMBED_DIM,
        params: opts.params,
        files,
        n_chunks: chunks.len() as u64,
        has_hnsw: hnsw.is_some(),
        sif: sif.is_some(),
    };
    std::fs::write(dir.join("chunks.bin"), postcard::to_allocvec(&chunks)?)?;
    std::fs::write(dir.join("bm25.flat"), crate::store::bm25::to_flat_bytes(&bm25))?;
    match &sif {
        Some(s) => std::fs::write(dir.join("sif.bin"), postcard::to_allocvec(s)?)?,
        None => {
            let _ = std::fs::remove_file(dir.join("sif.bin"));
        }
    }
    match hnsw {
        Some(h) => std::fs::write(dir.join("hnsw.bin"), h.to_bytes())?,
        None => {
            let _ = std::fs::remove_file(dir.join("hnsw.bin"));
        }
    }
    // meta.json last: it is what `discover` keys on, so writing it is what
    // publishes the index. Written first (as it used to be), a concurrent
    // reader could find the entry, fail to load the chunks.bin that did not
    // exist yet, and — for a cache entry, where a load failure is a miss —
    // delete the directory this build was still writing into.
    std::fs::write(dir.join("meta.json"), serde_json::to_vec_pretty(&meta)?)?;

    for name in ["meta.json", "chunks.bin", "bm25.flat", "emb.bin", "hnsw.bin", "sif.bin"] {
        if let Ok(m) = std::fs::metadata(dir.join(name)) {
            stats.index_bytes += m.len();
        }
    }
    Ok(stats)
}
