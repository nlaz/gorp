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
    /// Chunk embeddings are SIF-weighted (RESEARCH.md §9.1) — queries must
    /// be embedded with the same corpus stats (`sif.bin`).
    #[serde(default)]
    pub sif: bool,
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
    /// SIF-weighted chunk embeddings (experimental, RESEARCH.md §9.1):
    /// adds a frequency-counting pre-pass and stores `sif.bin`.
    pub sif: bool,
    /// SIF smoothing constant a in a/(a+p(w)); larger = milder weighting.
    pub sif_a: f64,
    /// Subtract the sample-estimated common component (SIF's second half).
    pub sif_center: bool,
}

impl Default for BuildOptions {
    fn default() -> Self {
        Self {
            params: ChunkParams::default(),
            hnsw: false,
            sif: false,
            sif_a: semantic::SIF_A,
            sif_center: false,
        }
    }
}

pub fn index_dir(root: &Path) -> PathBuf {
    root.join(SEMGREP_DIR)
}

pub fn exists(root: &Path) -> bool {
    index_dir(root).join("meta.json").is_file()
}

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

    // SIF pre-pass: count corpus token frequencies before any embedding, so
    // weighted pooling has its p(w). Cheap relative to the embed pass.
    let sif: Option<semantic::SifStats> = opts.sif.then(|| {
        let mut all = semantic::SifStats::default();
        for (_, batch) in corpus::pass_batches(&files, 256, 16 << 20) {
            let partials: Vec<semantic::SifStats> = batch
                .par_iter()
                .map(|fm| {
                    let mut s = semantic::SifStats::default();
                    if let Some(text) = corpus::read_text(&corpus::abs_path(root, fm)) {
                        s.count(&text);
                    }
                    s
                })
                .collect();
            for p in partials {
                all.merge(p);
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
                    let (_, slice) = corpus::chunk_lines(0, &text, &opts.params).into_iter().next()?;
                    Some(semantic::embed_sif(&corpus::doc_text(&fm.path, slice), &all))
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
    let mut hnsw = opts.hnsw.then(semantic::new_hnsw);
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
            Some(s) => pending.par_iter().map(|doc| semantic::embed_sif(doc, s)).collect(),
            None => ese::encode(pending.iter()),
        };
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

    // The pass is batched: each batch of files is read/chunked/tokenized in
    // parallel (the serial version left ~9 cores idle for 2/3 of the pass),
    // then folded serially in file order so chunk ids stay in lockstep with
    // the chunk table, BM25 add order, and emb.bin rows. Bounded batches keep
    // resident text to a few MB regardless of corpus size.
    for (base, batch) in corpus::pass_batches(&files, 256, 16 << 20) {
        let works: Vec<corpus::FileWork> = batch
            .par_iter()
            .enumerate()
            .map(|(i, fm)| {
                corpus::process_file(root, (base + i) as u32, fm, &opts.params, true, true)
            })
            .collect();
        for (i, work) in works.into_iter().enumerate() {
            progress(base + i + 1, files.len());
            stats.bytes_indexed += work.bytes;
            for (chunk, doc, tokens) in work.docs {
                bm25.add_tokenized(tokens.expect("build pass tokenizes"));
                chunks.push(chunk);
                pending.push(doc.expect("build pass keeps text"));
                if pending.len() >= EMBED_BATCH {
                    flush(&mut pending, &mut emb_out, &mut hnsw)?;
                }
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
        sif: sif.is_some(),
    };
    std::fs::write(dir.join("meta.json"), serde_json::to_vec_pretty(&meta)?)?;
    std::fs::write(dir.join("chunks.bin"), postcard::to_allocvec(&chunks)?)?;
    std::fs::write(dir.join("bm25.flat"), bm25.to_flat_bytes())?;
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

    for name in ["meta.json", "chunks.bin", "bm25.flat", "emb.bin", "hnsw.bin", "sif.bin"] {
        if let Ok(m) = std::fs::metadata(dir.join(name)) {
            stats.index_bytes += m.len();
        }
    }
    Ok(stats)
}

// ---------------------------------------------------------------------------
// Discovery + central cache (the index is a cache — RESEARCH.md §8/8.1).
//
// A query path resolves to an index by: (1) `.semgrep/` at the path itself,
// (2) `.semgrep/` at an ancestor, walking up like git finds `.git` (stopping
// at the repo boundary, $HOME, or the filesystem root), (3) the deepest
// central-cache entry whose root contains the path. Cache entries live under
// `$SEMGREP_CACHE_DIR` (default `~/.cache/semgrep`), keyed by canonical
// corpus root, and are written as a side effect of cold ranked searches.
// ---------------------------------------------------------------------------

/// An index resolved for a query path.
#[derive(Debug, Clone)]
pub struct Discovered {
    /// Directory holding meta.json / chunks.bin / bm25.flat / emb.bin.
    pub index_dir: PathBuf,
    /// Canonical corpus root the index describes.
    pub root: PathBuf,
    /// Query scope relative to `root`, `/`-separated ("" = whole corpus).
    pub prefix: String,
}

fn rel_prefix(root: &Path, scope: &Path) -> String {
    scope
        .strip_prefix(root)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_default()
}

/// Resolve the index that should serve a query over `query_root`, if any.
pub fn discover(query_root: &Path) -> Option<Discovered> {
    let canon = std::fs::canonicalize(query_root).ok()?;
    if !canon.is_dir() {
        return None;
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);

    // (1)+(2): in-tree .semgrep at the scope or an ancestor.
    let mut cur = canon.clone();
    loop {
        if exists(&cur) {
            return Some(Discovered {
                index_dir: index_dir(&cur),
                prefix: rel_prefix(&cur, &canon),
                root: cur,
            });
        }
        // Stop *after* checking: the repo root itself may hold the index,
        // but the walk must not escape into an enclosing repo or past $HOME.
        if cur.join(".git").exists() || home.as_deref() == Some(cur.as_path()) {
            break;
        }
        match cur.parent() {
            Some(p) => cur = p.to_path_buf(),
            None => break,
        }
    }

    // (3): deepest central-cache entry covering the scope.
    cache_entries()
        .into_iter()
        .filter(|(dir, root)| canon.starts_with(root) && dir.join("meta.json").is_file())
        .max_by_key(|(_, root)| root.components().count())
        .map(|(dir, root)| Discovered {
            index_dir: dir,
            prefix: rel_prefix(&root, &canon),
            root,
        })
}

/// Base directory for cache entries. `SEMGREP_CACHE_DIR` overrides (tests,
/// hygiene); default is `$XDG_CACHE_HOME`/semgrep or `~/.cache/semgrep`.
/// Resolved once per process so concurrent env reads never race.
pub fn cache_base() -> PathBuf {
    static BASE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    BASE.get_or_init(|| {
        if let Some(d) = std::env::var_os("SEMGREP_CACHE_DIR") {
            return PathBuf::from(d);
        }
        std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cache")
            })
            .join("semgrep")
    })
    .clone()
}

/// All cache entries as (entry_dir, canonical_root). Entries whose recorded
/// root no longer exists are skipped (GC candidates, not errors).
pub fn cache_entries() -> Vec<(PathBuf, PathBuf)> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(cache_base()) else { return out };
    for e in rd.flatten() {
        let dir = e.path();
        let Ok(root) = std::fs::read_to_string(dir.join("root.txt")) else { continue };
        let root = PathBuf::from(root.trim());
        if root.is_dir() {
            out.push((dir, root));
        }
    }
    out
}

/// The entry directory for a canonical root, allocating a collision-free
/// name on first use (hash bucket + root.txt verification).
fn cache_entry_dir(root: &Path) -> PathBuf {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    root.hash(&mut h);
    let base = cache_base();
    let stem = format!("{:016x}", h.finish());
    for i in 0..64 {
        let dir = if i == 0 {
            base.join(&stem)
        } else {
            base.join(format!("{stem}-{i}"))
        };
        match std::fs::read_to_string(dir.join("root.txt")) {
            Ok(r) if PathBuf::from(r.trim()) != root => continue, // collision
            _ => return dir,
        }
    }
    base.join(stem) // unreachable in practice
}

/// Write-through: build a cache entry covering `root` (must be canonical),
/// then retire any entries for scopes strictly inside it (scope promotion —
/// the wider entry serves every descendant via the prefix filter).
pub fn write_cache_entry(
    root: &Path,
    opts: &BuildOptions,
    progress: impl FnMut(usize, usize),
) -> Result<PathBuf> {
    let dir = cache_entry_dir(root);
    std::fs::create_dir_all(&dir)?;
    build_at(&dir, root, opts, progress)?;
    std::fs::write(dir.join("root.txt"), root.to_string_lossy().as_bytes())?;
    for (edir, eroot) in cache_entries() {
        if eroot != root && eroot.starts_with(root) {
            let _ = std::fs::remove_dir_all(edir);
        }
    }
    Ok(dir)
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
    /// Corpus token stats when the index is SIF-weighted — queries must be
    /// pooled with the same weights or the spaces don't match.
    pub sif: Option<semantic::SifStats>,
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
        let sif = if meta.sif {
            Some(postcard::from_bytes(&std::fs::read(dir.join("sif.bin"))?)?)
        } else {
            None
        };
        Ok(Self { root: root.to_path_buf(), meta, chunks, bm25, emb, hnsw, sif, timings: t })
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
