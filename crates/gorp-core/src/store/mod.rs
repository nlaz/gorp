//! The on-disk index: reading and writing a `.gorp/` directory.
//!
//! ```text
//! meta.json   version, chunk params, dims, file table (staleness)
//! chunks.bin  postcard Vec<Chunk>
//! bm25.flat   flat mmap-able BM25 (term table + postings + doc lens)
//! emb.bin     n_chunks × EMBED_DIM i8, unit-normalized then quantized
//! hnsw.bin    optional anny HNSW graph
//! sif.bin     optional corpus token statistics (SIF-weighted indexes)
//! ```
//!
//! This layer persists and loads representations. Deciding *which* index answers
//! a query, and keeping it honest against a moving tree, is `cache`'s job.
//!
//! `meta.json` is written last and removed before a rebuild: writing it is what
//! publishes an index, so a reader either sees a complete one or none.

pub mod bm25;
mod build;
mod load;

pub use build::{build, build_at, build_staged, is_transient, staging_path, sweep_transients};
pub use load::{LoadNeeds, LoadTimings, LoadedIndex};

use crate::{ChunkParams, FileMeta};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Directory name for a repo-local index.
///
/// Three other places spell this name and must move with it, because the
/// layering rule forbids them importing it: `corpus::is_index_dir` and
/// `keyword`'s walk both exclude it (miss one and the tool indexes its own
/// index), and `cache::compat::cache_base` names the central cache.
pub const DIR: &str = ".gorp";
pub(crate) const FORMAT_VERSION: u32 = 2;

#[derive(Serialize, Deserialize)]
pub struct IndexMeta {
    pub version: u32,
    pub dims: usize,
    pub params: ChunkParams,
    pub files: Vec<FileMeta>,
    pub n_chunks: u64,
    pub has_hnsw: bool,
    /// Chunk embeddings are SIF-weighted (RESEARCH.md §9.1) — queries must
    /// be embedded with the same corpus stats (`sif.bin`).
    #[serde(default)]
    pub sif: bool,
    /// How chunk text was rendered before embedding (RESEARCH.md §14.2) —
    /// queries must be rendered the same way, so the warm path reads this,
    /// never a flag. Absent in old metas = `none`, which is exact.
    #[serde(default)]
    pub embed_preproc: crate::text::EmbedPreproc,
    /// How the path line of `doc_text` was rendered (RESEARCH.md §20). Same
    /// contract as `embed_preproc`: absent in old metas = `full`, which is
    /// exact, because `full` is what every index before §20 did.
    #[serde(default)]
    pub path_render: crate::text::PathRender,
}

#[derive(Debug, Default)]
pub struct BuildStats {
    pub n_files: usize,
    pub n_chunks: usize,
    pub bytes_indexed: u64,
    pub index_bytes: u64,
    /// Where the build's wall time went. A cold ranked search *is* a build
    /// (RESEARCH.md §8), so a query that pays for one folds these into its own
    /// report rather than leaving the dominant cost of a first search
    /// attributable only to `total_ms`.
    pub stages: crate::trace::Stages,
    pub total_ms: f64,
}

pub struct BuildOptions {
    pub params: ChunkParams,
    pub hnsw: bool,
    /// SIF-weighted chunk embeddings (experimental, RESEARCH.md §9.1):
    /// adds a frequency-counting pre-pass and stores `sif.bin`.
    pub sif: bool,
    /// SIF smoothing constant a in a/(a+p(w)); larger = milder weighting.
    pub sif_a: f64,
    /// Subtract the sample-estimated common component (SIF's second half).
    pub sif_center: bool,
    /// Weight pooling by BM25-style idf over document frequency instead of
    /// SIF's a/(a+p) (RESEARCH.md §14.7). Only meaningful with `sif`.
    pub sif_idf: bool,
    /// Prose-render chunk text before embedding (RESEARCH.md §14.2).
    pub embed_preproc: crate::text::EmbedPreproc,
    /// How the path line of `doc_text` is rendered (RESEARCH.md §20).
    pub path_render: crate::text::PathRender,
}

impl Default for BuildOptions {
    fn default() -> Self {
        Self {
            params: ChunkParams::default(),
            hnsw: false,
            sif: false,
            sif_a: crate::text::SIF_A,
            sif_center: false,
            sif_idf: false,
            embed_preproc: crate::text::EmbedPreproc::None,
            path_render: crate::text::PathRender::Full,
        }
    }
}

pub fn index_dir(root: &Path) -> PathBuf {
    root.join(DIR)
}

pub fn exists(root: &Path) -> bool {
    index_dir(root).join("meta.json").is_file()
}
