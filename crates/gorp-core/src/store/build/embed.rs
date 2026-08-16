//! Writing `emb.bin`: embed in batches, unit-normalize, quantize to i8.
//!
//! Batching is what keeps the build's memory flat. Chunk texts are owned copies,
//! but only one batch is resident at a time, so a corpus of any size costs the
//! same working set. `ese` also parallelizes internally above 16 texts, so
//! batches are the unit of that too.

use crate::text::{self, EmbedPreproc, PathRender, GorpHnsw, SifStats};
use crate::trace::elapsed_ms;
use anyhow::{Context, Result};
use rayon::prelude::*;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::time::Instant;

/// Texts per embedding batch.
const BATCH: usize = 1024;

/// What the write cost, split by what scales differently: embedding is CPU-bound
/// in `ese`, graph insertion is CPU-bound in `anny` and grows with the graph, and
/// what remains is IO. Reported separately because a build that is slow for one
/// reason wants a different fix than a build that is slow for another.
#[derive(Debug, Default, Clone, Copy)]
pub struct EmbedTimings {
    pub embed_ms: f64,
    pub hnsw_ms: f64,
}

pub struct EmbedWriter<'a> {
    out: BufWriter<File>,
    pending: Vec<String>,
    /// SIF statistics when the index is SIF-weighted; chunk and query pooling
    /// must use the same ones or the vectors are not in the same space.
    sif: Option<&'a SifStats>,
    /// Prose rendering applied to every doc before it is embedded — the query
    /// side reads the same choice back from `meta.json` (RESEARCH.md §14.2).
    preproc: EmbedPreproc,
    /// How each doc's path line is rendered (RESEARCH.md §20).
    path_render: PathRender,
    /// Built alongside the matrix when requested, from the same vectors.
    hnsw: Option<GorpHnsw>,
    rows: usize,
    timings: EmbedTimings,
}

impl<'a> EmbedWriter<'a> {
    pub fn create(
        dir: &Path,
        hnsw: bool,
        sif: Option<&'a SifStats>,
        preproc: EmbedPreproc,
        path_render: PathRender,
    ) -> Result<Self> {
        let file = File::create(dir.join("emb.bin")).context("create emb.bin")?;
        Ok(Self {
            out: BufWriter::new(file),
            pending: Vec::with_capacity(BATCH),
            sif,
            preproc,
            path_render,
            hnsw: hnsw.then(text::new_hnsw),
            rows: 0,
            timings: EmbedTimings::default(),
        })
    }

    /// Queue one chunk's text. Rows land in call order, which is what keeps them
    /// in lockstep with the chunk table and the BM25 documents.
    pub fn push(&mut self, doc: String) -> Result<()> {
        self.pending.push(doc);
        if self.pending.len() >= BATCH {
            self.flush()?;
        }
        Ok(())
    }

    /// Flush the tail and write `hnsw.bin` if a graph was built. Returns the row
    /// count — which must equal the chunk count — and where the time went.
    pub fn finish(mut self, dir: &Path) -> Result<(usize, EmbedTimings)> {
        self.flush()?;
        self.out.flush()?;
        let t = Instant::now();
        match self.hnsw {
            Some(graph) => std::fs::write(dir.join("hnsw.bin"), graph.to_bytes())?,
            // Clear a graph left by a previous build that had --hnsw on, or a
            // later load would trust meta.json and find a stale one.
            None => {
                let _ = std::fs::remove_file(dir.join("hnsw.bin"));
            }
        }
        self.timings.hnsw_ms += elapsed_ms(t);
        Ok((self.rows, self.timings))
    }

    fn flush(&mut self) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let t_embed = Instant::now();
        // Rendered here, in the batch, so the (parallel) embedding pass pays
        // for it rather than the serial fold that queues the docs.
        let rendered: Vec<std::borrow::Cow<'_, str>> = self
            .pending
            .par_iter()
            .map(|doc| text::prose_render_doc(doc, self.preproc, self.path_render))
            .collect();
        let mut vecs = match self.sif {
            Some(s) => rendered.par_iter().map(|doc| text::embed_sif(doc, s)).collect(),
            None => ese::encode(rendered.iter()),
        };
        self.timings.embed_ms += elapsed_ms(t_embed);
        for v in &mut vecs {
            // Normalize first: the i8 quantization assumes unit vectors, and the
            // query-time scan compares dot products against that assumption.
            crate::rank::normalize(v);
            let quantized = crate::rank::quantize_i8(v);
            // SAFETY: i8 and u8 share size and alignment (1), so a fully
            // initialized i8 slice is a valid u8 slice of the same length.
            let bytes = unsafe {
                std::slice::from_raw_parts(quantized.as_ptr() as *const u8, quantized.len())
            };
            self.out.write_all(bytes)?;
            if let Some(graph) = &mut self.hnsw {
                let t = Instant::now();
                graph.insert(*v);
                self.timings.hnsw_ms += elapsed_ms(t);
            }
            self.rows += 1;
        }
        self.pending.clear();
        Ok(())
    }
}
