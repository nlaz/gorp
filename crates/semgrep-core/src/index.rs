//! On-disk index: the `.semgrep/` directory.
//!
//! ```text
//! .semgrep/meta.json   version, chunk params, dims, file table (staleness)
//! .semgrep/chunks.bin  postcard Vec<Chunk>
//! .semgrep/bm25.flat   flat mmap-able BM25 (term table + postings + doc lens)
//! .semgrep/emb.bin     n_chunks × EMBED_DIM i8, unit-normalized then quantized
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
    // Unpublish before touching anything. A rebuild overwrites emb.bin in
    // place, so leaving the old meta.json readable would let a concurrent
    // query pair a stale file table with a half-rewritten matrix. Absent is a
    // miss and costs a streaming pass; mixed is a wrong answer.
    let _ = std::fs::remove_file(dir.join("meta.json"));

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
    /// True for a central-cache entry (disposable: any failure to load it is
    /// a miss). False for a repo-local `.semgrep`, an explicit artifact whose
    /// failures the user should see.
    pub from_cache: bool,
}

fn rel_prefix(root: &Path, scope: &Path) -> String {
    scope.strip_prefix(root).map(|p| p.to_string_lossy().replace('\\', "/")).unwrap_or_default()
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
                from_cache: false,
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

    // (3): deepest central-cache entry covering the scope. Entries live under
    // a *generation* directory keyed to this binary's format+table (see
    // `compat_key`), so an entry written by an incompatible binary is simply
    // not found here — no error to surface, nothing to evict on a read path.
    cache_entries()
        .into_iter()
        .filter(|(dir, root)| canon.starts_with(root) && dir.join("meta.json").is_file())
        .max_by_key(|(_, root)| root.components().count())
        .map(|(dir, root)| Discovered {
            index_dir: dir,
            prefix: rel_prefix(&root, &canon),
            root,
            from_cache: true,
        })
}

/// Fingerprint of the compiled-in embedding stack — table, tokenizer, and
/// pooling all at once, since it is just "what does this binary encode this
/// probe to". Two binaries agree iff their vectors are interchangeable.
///
/// `dims` alone is not enough: swapping to a *different* table of the same
/// width (e.g. a code-distilled 256-dim model) would pass a dims check and
/// then silently score yesterday's vectors against today's queries.
fn table_fingerprint() -> u64 {
    static FP: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *FP.get_or_init(|| {
        use std::hash::{Hash, Hasher};
        // Deliberately mixed: prose, code punctuation, and an OOV-ish token,
        // so tokenizer changes move the fingerprint too.
        let v = ese::encode_single("semgrep::probe scalar_None retry_backoff 42");
        let mut h = std::collections::hash_map::DefaultHasher::new();
        for x in v.iter() {
            // Quantize before hashing: identical tables must fingerprint
            // identically across build flags that only perturb the last bits.
            ((x * 4096.0).round() as i64).hash(&mut h);
        }
        h.finish()
    })
}

/// Directory name for this binary's cache generation. Entries from an
/// incompatible binary sort into a sibling directory and are ignored, then
/// reclaimed by [`gc_old_generations`].
pub fn compat_key() -> String {
    // 16 bits. This never has to resist collisions globally — only to tell
    // apart the tables one machine has actually used. It is, though, the only
    // guard against the *silent* failure: an entry with matching dims but a
    // different table loads cleanly and scores yesterday's vectors against
    // today's queries. 8 bits would collide once per ~256 table swaps for two
    // characters of path; 16 makes it once per ~65k.
    format!("v{FORMAT_VERSION}-d{EMBED_DIM}-{:04x}", table_fingerprint() as u16)
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
    let Ok(rd) = std::fs::read_dir(cache_generation()) else { return out };
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

/// This binary's generation directory: `<cache base>/<compat key>/`.
pub fn cache_generation() -> PathBuf {
    cache_base().join(compat_key())
}

/// Remove cache generations this binary cannot use, plus pre-generation
/// (flat) entries left by older builds. Called after a successful write,
/// never on a read path — reclaiming space must not be a side effect of
/// answering a query.
///
/// Only two shapes are ever removed, so an unrelated directory someone
/// pointed `SEMGREP_CACHE_DIR` at is left alone: a sibling generation
/// (`v<fmt>-d<dims>-<fp>`), and a legacy entry (holds `meta.json` directly —
/// generations hold entry *directories*).
pub fn gc_old_generations() {
    let (base, keep) = (cache_base(), compat_key());
    let Ok(rd) = std::fs::read_dir(&base) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if !p.is_dir() || p.file_name().is_some_and(|n| n == keep.as_str()) {
            continue;
        }
        let is_generation = p.file_name().is_some_and(|n| n.to_string_lossy().starts_with('v'))
            && !p.join(&keep).exists()
            && !p.join("meta.json").exists();
        let is_legacy_entry = p.join("meta.json").is_file() && p.join("root.txt").is_file();
        if is_generation || is_legacy_entry {
            let _ = std::fs::remove_dir_all(&p);
        }
    }
}

/// Total cache budget in bytes. `SEMGREP_CACHE_MAX_BYTES` overrides.
///
/// Default 2 GiB: the median real repo indexes to ~5 MB, so this holds
/// hundreds of ordinary projects, while one kernel-scale corpus (946 MB) can
/// still fit alongside a few others. Without a cap the cache only grows —
/// which was the honest caveat in the README, and is the thing that turns a
/// cache into a slow disk leak.
pub fn cache_max_bytes() -> u64 {
    std::env::var("SEMGREP_CACHE_MAX_BYTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2 * 1024 * 1024 * 1024)
}

/// How long an entry may sit without a `meta.json` before it is presumed
/// abandoned rather than mid-build. Generous: a kernel-scale build takes ~45 s.
const ABANDONED_AFTER_SECS: u64 = 600;

#[derive(Debug, Clone)]
pub struct CacheEntryInfo {
    pub dir: PathBuf,
    pub root: PathBuf,
    pub bytes: u64,
    /// Seconds since this entry was last read or written.
    pub age_secs: u64,
    /// False once the indexed directory no longer exists — a dead entry that
    /// can never be useful again.
    pub root_exists: bool,
    /// Registered but never published: `root.txt` without `meta.json`. Either a
    /// build in flight right now, or one that was interrupted and left this
    /// behind. Age tells the two apart.
    pub incomplete: bool,
}

fn dir_bytes(dir: &Path) -> u64 {
    let Ok(rd) = std::fs::read_dir(dir) else { return 0 };
    rd.flatten()
        .filter_map(|e| e.metadata().ok())
        .map(|m| if m.is_dir() { 0 } else { m.len() })
        .sum()
}

/// Every entry in this generation, with size and recency. Powers both the
/// budget enforcer and `semgrep cache --status`.
pub fn cache_status() -> Vec<CacheEntryInfo> {
    let now = std::time::SystemTime::now();
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(cache_generation()) else { return out };
    for e in rd.flatten() {
        let dir = e.path();
        let Ok(root) = std::fs::read_to_string(dir.join("root.txt")) else { continue };
        let root = PathBuf::from(root.trim());
        // `last_check` is touched by read-repair, `meta.json` by a build, so
        // the newer of the two is when this entry was last actually used.
        // `root.txt` is the fallback: an entry mid-build has only that.
        let age = ["last_check", "meta.json", "root.txt"]
            .iter()
            .filter_map(|f| std::fs::metadata(dir.join(f)).ok()?.modified().ok())
            .filter_map(|t| now.duration_since(t).ok())
            .map(|d| d.as_secs())
            .min()
            .unwrap_or(u64::MAX);
        out.push(CacheEntryInfo {
            bytes: dir_bytes(&dir),
            root_exists: root.is_dir(),
            incomplete: !dir.join("meta.json").is_file(),
            dir,
            root,
            age_secs: age,
        });
    }
    out.sort_by_key(|e| e.age_secs);
    out
}

/// Drop dead entries, then evict least-recently-used until under budget.
/// Returns (entries removed, bytes reclaimed). Called after a write, so the
/// cost lands on the path that already pays for a full corpus pass.
pub fn enforce_budget() -> (usize, u64) {
    enforce_budget_with_cap(cache_max_bytes(), ABANDONED_AFTER_SECS)
}

/// [`enforce_budget`] with explicit thresholds. Separated so a caller — a test,
/// or a future `--max-bytes` flag — can exercise reclamation without mutating
/// the process environment that `cache_max_bytes` reads.
pub fn enforce_budget_with_cap(cap: u64, abandoned_after_secs: u64) -> (usize, u64) {
    let mut entries = cache_status();
    let (mut n, mut freed) = (0usize, 0u64);

    // 1. Entries that can never serve a query, in either of the two ways:
    //    the repo is gone (a moved or deleted checkout would otherwise hold
    //    its index forever), or the build that registered them never published
    //    a meta.json and is long past finishing. A young incomplete entry is
    //    left alone — that is a build happening right now.
    entries.retain(|e| {
        let dead = !e.root_exists || (e.incomplete && e.age_secs >= abandoned_after_secs);
        if !dead {
            return true;
        }
        if std::fs::remove_dir_all(&e.dir).is_ok() {
            n += 1;
            freed += e.bytes;
        }
        false
    });

    // 2. LRU until under the cap. Oldest first; `cache_status` sorts by
    //    recency ascending, so walk from the back.
    let mut total: u64 = entries.iter().map(|e| e.bytes).sum();
    while total > cap {
        let Some(victim) = entries.pop() else { break };
        if std::fs::remove_dir_all(&victim.dir).is_ok() {
            total = total.saturating_sub(victim.bytes);
            n += 1;
            freed += victim.bytes;
        }
    }
    (n, freed)
}

/// Delete every entry in every generation. `semgrep cache --clear`.
pub fn cache_clear() -> (usize, u64) {
    let mut n = 0;
    let mut freed = 0;
    for e in cache_status() {
        if std::fs::remove_dir_all(&e.dir).is_ok() {
            n += 1;
            freed += e.bytes;
        }
    }
    gc_old_generations();
    (n, freed)
}

/// The entry directory for a canonical root, allocating a collision-free
/// name on first use (hash bucket + root.txt verification).
fn cache_entry_dir(root: &Path) -> PathBuf {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    root.hash(&mut h);
    let base = cache_generation();
    // Name entries after the directory they cache, not just a hash, so that
    // `ls ~/.cache/semgrep/*` and `du -sh *` are readable — you can see which
    // repos are costing you space without opening root.txt.
    // Last two components, so `.../semgrep-core/src` reads as
    // "semgrep-core-src" rather than an uninformative "src".
    let mut comps: Vec<String> = root
        .components()
        .rev()
        .take(2)
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .collect();
    comps.reverse();
    let label: String = comps
        .join("-")
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect::<String>()
        .chars()
        .rev()
        .take(28)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let stem = format!("{label}-{:08x}", h.finish() as u32);
    for i in 0..64 {
        let dir = if i == 0 { base.join(&stem) } else { base.join(format!("{stem}-{i}")) };
        match std::fs::read_to_string(dir.join("root.txt")) {
            Ok(r) if Path::new(r.trim()) != root => continue, // collision
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
    // root.txt first, before a single byte of index. It is what makes an entry
    // *enumerable*, and only enumerable entries can be counted or reclaimed —
    // written afterwards, a build interrupted partway (Ctrl-C during the first
    // search of a large repo, which is exactly when people interrupt) left a
    // directory that `cache --status` could not see and `cache --prune` could
    // not free. It does not make the entry discoverable: that needs meta.json,
    // which `build_at` writes last.
    std::fs::write(dir.join("root.txt"), root.to_string_lossy().as_bytes())?;
    build_at(&dir, root, opts, progress)?;

    // Reclaim only after the entry is complete and registered, so the budget
    // enforcer actually sees what was just built. Running it before the write
    // meant a corpus larger than the whole budget evicted every *other* entry
    // and then sat over the cap until some later write noticed.
    gc_old_generations();
    enforce_budget();

    // Scope promotion: a wider entry serves every descendant through the
    // prefix filter, so narrower ones are now dead weight.
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
            bail!(
                "index format v{} != supported v{FORMAT_VERSION}; re-run `semgrep index`",
                meta.version
            );
        }
        if meta.dims != EMBED_DIM {
            bail!(
                "index built with {} dims but this binary embeds {}; re-run `semgrep index`",
                meta.dims,
                EMBED_DIM
            );
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
        if emb.len() != chunks.len() * EMBED_DIM {
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
