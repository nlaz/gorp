//! Loading an index, and reporting how stale it has become.

use super::{FORMAT_VERSION, IndexMeta, index_dir};
use crate::store::bm25::FlatBm25;
use crate::text::GorpHnsw;
use crate::{Chunk, EMBED_DIM, FileMeta, corpus, text};
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

/// Which index components a query actually needs. Loading everything
/// unconditionally is ruinous: a 3.1 GB HNSW graph deserialize turned every
/// warm kernel query into ~35 s regardless of mode.
#[derive(Debug, Clone, Copy)]
pub struct LoadNeeds {
    pub bm25: bool,
    pub hnsw: bool,
}

impl LoadNeeds {
    pub fn all() -> Self {
        Self { bm25: true, hnsw: true }
    }
}

/// An index this binary cannot read because a *different* binary wrote it —
/// not because it is damaged.
///
/// A distinct type rather than a message, for the reason [`DriftTooLarge`]
/// gives: the arm in [`crate::search::search`] that catches this has to be
/// told apart from the "this entry is unreadable" arm beside it, and matching
/// on a string would make that distinction a typo away from wrong.
///
/// The difference matters because the remedy differs. A truncated write or a
/// mixed-generation restore is damage, and damage in an artifact the user
/// created on purpose is worth raising. A format or dims mismatch is neither:
/// it means the index predates this build. The corpus is fine, the streaming
/// path answers it correctly, and failing the query instead buys the caller
/// nothing. Measured before this existed: a v2 `.gorp` committed as a test
/// fixture made every query scoped at that directory exit 2 with empty stdout
/// and the reason on stderr alone — which, to an agent piping `2>/dev/null`,
/// is silence indistinguishable from "no matches".
#[derive(Debug, Clone, Copy)]
pub enum IndexNotMine {
    Format { found: u32, supported: u32 },
    Dims { found: usize, supported: usize },
}

impl std::fmt::Display for IndexNotMine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Format { found, supported } => write!(
                f,
                "index format v{found} != supported v{supported}; re-run `gorp index`"
            ),
            Self::Dims { found, supported } => write!(
                f,
                "index built with {found} dims but this binary embeds {supported}; \
                 re-run `gorp index`"
            ),
        }
    }
}

impl std::error::Error for IndexNotMine {}

/// Per-component load timings (ms) — performance provenance for the warm
/// path, where "loading the index" hides very different costs per component.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct LoadTimings {
    pub meta_ms: f64,
    pub chunks_ms: f64,
    pub bm25_ms: f64,
    pub mmap_ms: f64,
    pub hnsw_ms: f64,
    pub sif_ms: f64,
}

/// A loaded index. The embedding matrix stays mmap'd — resident memory grows
/// only with the pages actually touched by queries. BM25 postings and the
/// HNSW graph are loaded only when the query mode needs them.
pub struct LoadedIndex {
    pub root: PathBuf,
    pub meta: IndexMeta,
    pub chunks: Vec<Chunk>,
    pub bm25: Option<FlatBm25>,
    emb: memmap2::Mmap,
    pub hnsw: Option<GorpHnsw>,
    /// Corpus token stats when the index is SIF-weighted — queries must be
    /// pooled with the same weights or the spaces don't match.
    pub sif: Option<text::SifStats>,
    pub timings: LoadTimings,
}

impl LoadedIndex {
    pub fn load(root: &Path, needs: LoadNeeds) -> Result<Self> {
        Self::load_dir(&index_dir(root), root, needs)
    }

    /// Load an index from `dir` describing corpus `root` (repo-local or a
    /// central cache entry — the two differ only in where the files live).
    pub fn load_dir(dir: &Path, root: &Path, needs: LoadNeeds) -> Result<Self> {
        let dir = dir.to_path_buf();
        let mut t = LoadTimings::default();
        let ms = |start: std::time::Instant| start.elapsed().as_secs_f64() * 1e3;

        let t0 = std::time::Instant::now();
        let meta: IndexMeta = serde_json::from_slice(
            &std::fs::read(dir.join("meta.json")).context("no .gorp index here")?,
        )?;
        t.meta_ms = ms(t0);
        if meta.version != FORMAT_VERSION {
            return Err(anyhow::Error::new(IndexNotMine::Format {
                found: meta.version,
                supported: FORMAT_VERSION,
            }));
        }
        if meta.dims != EMBED_DIM {
            return Err(anyhow::Error::new(IndexNotMine::Dims {
                found: meta.dims,
                supported: EMBED_DIM,
            }));
        }
        let t0 = std::time::Instant::now();
        let chunks: Vec<Chunk> = postcard::from_bytes(&std::fs::read(dir.join("chunks.bin"))?)?;
        t.chunks_ms = ms(t0);
        // The one cross-artifact invariant [`LoadedIndex::file`] indexes by
        // without checking: every chunk's file_id must resolve in meta.json's
        // file table. A mixed-generation restore (valid meta.json from one
        // build, chunks.bin from another) is the realistic way they diverge,
        // and an error here is a disposable-entry miss; a panic later is not.
        if let Some(max_id) = chunks.iter().map(|c| c.file_id).max()
            && max_id as usize >= meta.files.len()
        {
            bail!(
                "chunks.bin references file_id {max_id} but meta.json lists {} files; \
                 index is corrupt — re-run `gorp index`",
                meta.files.len()
            );
        }
        let bm25 = if needs.bm25 {
            let t0 = std::time::Instant::now();
            let b = FlatBm25::open(&dir.join("bm25.flat"))?;
            t.bm25_ms = ms(t0);
            Some(b)
        } else {
            None
        };
        let t0 = std::time::Instant::now();
        let file = std::fs::File::open(dir.join("emb.bin"))?;
        let emb = unsafe { memmap2::Mmap::map(&file)? };
        t.mmap_ms = ms(t0);
        if emb.len() != chunks.len() * EMBED_DIM {
            bail!("emb.bin size mismatch; index is corrupt — re-run `gorp index`");
        }
        let hnsw = if meta.has_hnsw && needs.hnsw {
            let t0 = std::time::Instant::now();
            let bytes = std::fs::read(dir.join("hnsw.bin"))?;
            let h = GorpHnsw::from_bytes(anny::metric::Cosine, &bytes)
                .map_err(|e| anyhow::anyhow!("hnsw.bin corrupt: {e:?}"))?;
            t.hnsw_ms = ms(t0);
            Some(h)
        } else {
            None
        };
        let sif = if meta.sif {
            let t0 = std::time::Instant::now();
            let s = postcard::from_bytes(&std::fs::read(dir.join("sif.bin"))?)?;
            t.sif_ms = ms(t0);
            Some(s)
        } else {
            None
        };
        Ok(Self { root: root.to_path_buf(), meta, chunks, bm25, emb, hnsw, sif, timings: t })
    }

    /// The embedding matrix: `n_chunks × EMBED_DIM` i8, one unit-normalized
    /// row per chunk, mmap-backed.
    pub fn emb_matrix_i8(&self) -> &[i8] {
        let bytes: &[u8] = &self.emb;
        // SAFETY: i8 and u8 have identical size and alignment (1), so any
        // initialized byte slice is a valid i8 slice of the same length.
        unsafe { std::slice::from_raw_parts(bytes.as_ptr() as *const i8, bytes.len()) }
    }

    pub fn file(&self, chunk: &Chunk) -> &FileMeta {
        &self.meta.files[chunk.file_id as usize]
    }

    /// Count files that changed/appeared/disappeared since the index was
    /// built. Cheap staleness signal — callers decide what to do with it.
    pub fn stale_files(&self) -> Result<usize> {
        Ok(self.drift()?.len())
    }

    /// Whole-corpus drift against the file table. The same comparison
    /// read-repair does per query scope, unscoped.
    pub fn drift(&self) -> Result<corpus::Diff> {
        let live = corpus::walk(&self.root, &self.meta.params)?;
        Ok(corpus::diff(&self.meta.files, &live, ""))
    }
}
