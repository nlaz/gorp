//! On-disk index: the `.semgrep/` directory.
//!
//! ```text
//! .semgrep/meta.json   version, chunk params, dims, file table (staleness)
//! .semgrep/chunks.bin  postcard Vec<Chunk>
//! .semgrep/bm25.flat   flat mmap-able BM25 (term table + postings + doc lens)
//! .semgrep/emb.bin     raw little-endian f32, n_chunks × EMBED_DIM (mmap'd)
//! .semgrep/hnsw.bin    optional anny HNSW graph
//! ```

use crate::bm25::{Bm25Index, FlatBm25};
use crate::semantic::{self, SemgrepHnsw};
use crate::{Chunk, ChunkParams, EMBED_DIM, FileMeta, corpus};
use anyhow::{Context, Result, bail};
use std::io::Write;
use std::path::{Path, PathBuf};

pub const SEMGREP_DIR: &str = ".semgrep";
const FORMAT_VERSION: u32 = 2;
/// Chunk texts are embedded in batches this large (ese goes parallel >=16).
const EMBED_BATCH: usize = 1024;

#[derive(serde::Serialize, serde::Deserialize)]
pub struct IndexMeta {
    pub version: u32,
    pub dims: usize,
    pub params: ChunkParams,
    pub files: Vec<FileMeta>,
    pub n_chunks: u64,
    pub has_hnsw: bool,
    /// v2: emb.bin rows are unit-normalized, enabling the dot-product scan.
    #[serde(default)]
    pub normalized: bool,
    /// v2: emb.bin stores i8-quantized rows (n × EMBED_DIM bytes) — 4× less
    /// IO for the brute scan, which provenance showed is fault/IO bound.
    #[serde(default)]
    pub quantized: bool,
}

#[derive(Debug, Default)]
pub struct BuildStats {
    pub n_files: usize,
    pub n_chunks: usize,
    pub bytes_indexed: u64,
    pub index_bytes: u64,
}

pub struct BuildOptions {
    pub params: ChunkParams,
    pub hnsw: bool,
}

impl Default for BuildOptions {
    fn default() -> Self {
        Self { params: ChunkParams::default(), hnsw: false }
    }
}

pub fn index_dir(root: &Path) -> PathBuf {
    root.join(SEMGREP_DIR)
}

pub fn exists(root: &Path) -> bool {
    index_dir(root).join("meta.json").is_file()
}

/// Build (or fully rebuild) the index for `root`.
pub fn build(
    root: &Path,
    opts: &BuildOptions,
    mut progress: impl FnMut(usize, usize),
) -> Result<BuildStats> {
    let files = corpus::walk(root, &opts.params)?;
    let dir = index_dir(root);
    std::fs::create_dir_all(&dir)?;

    let mut bm25 = Bm25Index::new();
    let mut chunks: Vec<Chunk> = Vec::new();
    let mut hnsw = opts.hnsw.then(semantic::new_hnsw);
    let mut emb_out = std::io::BufWriter::new(
        std::fs::File::create(dir.join("emb.bin")).context("create emb.bin")?,
    );
    let mut stats = BuildStats { n_files: files.len(), ..Default::default() };

    // Batched embedding across file boundaries: texts are owned copies but
    // only one batch is resident at a time.
    let mut pending: Vec<String> = Vec::with_capacity(EMBED_BATCH);
    let flush = |pending: &mut Vec<String>,
                     emb_out: &mut std::io::BufWriter<std::fs::File>,
                     hnsw: &mut Option<SemgrepHnsw>|
     -> Result<()> {
        if pending.is_empty() {
            return Ok(());
        }
        let mut vecs = ese::encode(pending.iter());
        for v in &mut vecs {
            // Unit-normalize, then quantize to i8 for the on-disk matrix.
            crate::semantic::normalize(v);
            let q = crate::semantic::quantize_i8(v);
            emb_out.write_all(unsafe {
                std::slice::from_raw_parts(q.as_ptr() as *const u8, q.len())
            })?;
            if let Some(h) = hnsw {
                h.insert(*v);
            }
        }
        pending.clear();
        Ok(())
    };

    for (file_id, fm) in files.iter().enumerate() {
        progress(file_id + 1, files.len());
        let Some(text) = corpus::read_text(&corpus::abs_path(root, fm)) else {
            continue;
        };
        stats.bytes_indexed += text.len() as u64;
        for (chunk, slice) in corpus::chunk_lines(file_id as u32, &text, &opts.params) {
            let doc = corpus::doc_text(&fm.path, slice);
            bm25.add_doc(&doc);
            chunks.push(chunk);
            pending.push(doc);
            if pending.len() >= EMBED_BATCH {
                flush(&mut pending, &mut emb_out, &mut hnsw)?;
            }
        }
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
        normalized: true,
        quantized: true,
    };
    std::fs::write(dir.join("meta.json"), serde_json::to_vec_pretty(&meta)?)?;
    std::fs::write(dir.join("chunks.bin"), postcard::to_allocvec(&chunks)?)?;
    std::fs::write(dir.join("bm25.flat"), bm25.to_flat_bytes())?;
    match hnsw {
        Some(h) => std::fs::write(dir.join("hnsw.bin"), h.to_bytes())?,
        None => {
            let _ = std::fs::remove_file(dir.join("hnsw.bin"));
        }
    }

    for name in ["meta.json", "chunks.bin", "bm25.flat", "emb.bin", "hnsw.bin"] {
        if let Ok(m) = std::fs::metadata(dir.join(name)) {
            stats.index_bytes += m.len();
        }
    }
    Ok(stats)
}

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

/// Per-component load timings (ms) — performance provenance for the warm
/// path, where "loading the index" hides very different costs per component.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct LoadTimings {
    pub meta_ms: f64,
    pub chunks_ms: f64,
    pub bm25_ms: f64,
    pub mmap_ms: f64,
    pub hnsw_ms: f64,
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
    pub hnsw: Option<SemgrepHnsw>,
    pub timings: LoadTimings,
}

impl LoadedIndex {
    pub fn load(root: &Path, needs: LoadNeeds) -> Result<Self> {
        let dir = index_dir(root);
        let mut t = LoadTimings::default();
        let ms = |start: std::time::Instant| start.elapsed().as_secs_f64() * 1e3;

        let t0 = std::time::Instant::now();
        let meta: IndexMeta = serde_json::from_slice(
            &std::fs::read(dir.join("meta.json")).context("no .semgrep index here")?,
        )?;
        t.meta_ms = ms(t0);
        if meta.version != FORMAT_VERSION {
            bail!("index format v{} != supported v{FORMAT_VERSION}; re-run `semgrep index`", meta.version);
        }
        if meta.dims != EMBED_DIM {
            bail!("index built with {} dims but this binary embeds {}; re-run `semgrep index`", meta.dims, EMBED_DIM);
        }
        let t0 = std::time::Instant::now();
        let chunks: Vec<Chunk> = postcard::from_bytes(&std::fs::read(dir.join("chunks.bin"))?)?;
        t.chunks_ms = ms(t0);
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
        let row_bytes = if meta.quantized { EMBED_DIM } else { EMBED_DIM * 4 };
        if emb.len() != chunks.len() * row_bytes {
            bail!("emb.bin size mismatch; index is corrupt — re-run `semgrep index`");
        }
        let hnsw = if meta.has_hnsw && needs.hnsw {
            let t0 = std::time::Instant::now();
            let bytes = std::fs::read(dir.join("hnsw.bin"))?;
            let h = SemgrepHnsw::from_bytes(anny::metric::Cosine, &bytes)
                .map_err(|e| anyhow::anyhow!("hnsw.bin corrupt: {e:?}"))?;
            t.hnsw_ms = ms(t0);
            Some(h)
        } else {
            None
        };
        Ok(Self { root: root.to_path_buf(), meta, chunks, bm25, emb, hnsw, timings: t })
    }

    /// The embedding matrix as a flat f32 slice (mmap-backed, page-aligned).
    /// Only valid when `!meta.quantized` (v1-era indexes).
    pub fn emb_matrix(&self) -> &[f32] {
        let bytes: &[u8] = &self.emb;
        unsafe {
            std::slice::from_raw_parts(bytes.as_ptr() as *const f32, bytes.len() / 4)
        }
    }

    /// The i8-quantized embedding matrix (v2 indexes, `meta.quantized`).
    pub fn emb_matrix_i8(&self) -> &[i8] {
        let bytes: &[u8] = &self.emb;
        unsafe { std::slice::from_raw_parts(bytes.as_ptr() as *const i8, bytes.len()) }
    }

    pub fn file(&self, chunk: &Chunk) -> &FileMeta {
        &self.meta.files[chunk.file_id as usize]
    }

    /// Count files that changed/appeared/disappeared since the index was
    /// built. Cheap staleness signal — callers decide what to do with it.
    pub fn stale_files(&self) -> Result<usize> {
        let live = corpus::walk(&self.root, &self.meta.params)?;
        let mut indexed: std::collections::HashMap<&str, (u64, u64)> = self
            .meta
            .files
            .iter()
            .map(|f| (f.path.as_str(), (f.size, f.mtime)))
            .collect();
        let mut stale = 0usize;
        for f in &live {
            match indexed.remove(f.path.as_str()) {
                Some((size, mtime)) if size == f.size && mtime == f.mtime => {}
                _ => stale += 1,
            }
        }
        Ok(stale + indexed.len())
    }
}
