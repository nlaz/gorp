//! High-level search API tying the engines together, with indexed and
//! unindexed (streaming) paths and hybrid RRF fusion.

use crate::bm25::Bm25Index;
use crate::index::{self, LoadedIndex};
use crate::keyword::{self, KeywordOptions};
use crate::semantic::{self, TopK};
use crate::{Chunk, ChunkParams, corpus, tokenize};
use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Instant;

/// How many candidates each ranked engine contributes to RRF fusion.
const FUSION_POOL: usize = 128;
const RRF_K: f32 = 60.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Keyword,
    Bm25,
    Semantic,
    Hybrid,
}

#[derive(Debug, Clone)]
pub struct SearchOptions {
    pub mode: Mode,
    pub k: usize,
    /// Force the streaming path even when a .semgrep index exists.
    pub no_index: bool,
    /// Use the HNSW graph when present (else exact brute force).
    pub use_hnsw: bool,
    /// Re-walk the corpus after an indexed search to count stale files.
    /// Costs a full directory walk (~1s on 80k files), so off by default.
    pub check_stale: bool,
    /// Weight of the semantic list in hybrid RRF (BM25 is 1.0). Evals showed
    /// equal weighting lets a weak semantic list dilute BM25.
    pub sem_weight: f32,
    /// MMR diversity reranking: trade a little rank fidelity for results
    /// spread across different files/regions instead of near-duplicates.
    pub diversify: bool,
    /// MMR lambda: 1.0 = pure relevance, 0.0 = pure diversity.
    pub mmr_lambda: f32,
    /// PRF (pseudo-relevance feedback): expand the query with this many
    /// discriminative terms from the first pass's top hits, then re-rank
    /// lexically (RESEARCH.md §9.3). 0 = off.
    pub prf_terms: usize,
    /// Rerank the candidate pool by MaxSim late interaction (§9.2).
    pub rerank_maxsim: bool,
    /// MaxSim rerank head size (0 = auto: k*3, min 24).
    pub maxsim_pool: usize,
    /// Blend of MaxSim vs original embedding order within the reranked
    /// head: 1.0 = pure MaxSim (default), 0.0 = original order.
    pub maxsim_blend: f32,
    pub params: ChunkParams,
    pub keyword: KeywordOptions,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            mode: Mode::Hybrid,
            k: 10,
            no_index: false,
            use_hnsw: true,
            check_stale: false,
            sem_weight: 0.2,
            diversify: true,
            mmr_lambda: 0.75,
            prf_terms: 0,
            rerank_maxsim: false,
            maxsim_pool: 0,
            maxsim_blend: 1.0,
            params: ChunkParams::default(),
            keyword: KeywordOptions::default(),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SearchHit {
    pub path: String,
    pub start_line: u32,
    pub end_line: u32,
    /// Best-matching line within the chunk (== start_line for keyword mode).
    pub line: u32,
    pub text: String,
    pub score: f32,
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct SearchReport {
    pub used_index: bool,
    pub used_hnsw: bool,
    /// This query's cold pass was persisted as a cache entry (write-through).
    pub wrote_cache: bool,
    /// Files repaired-around this query (read-repair overlay), or the full
    /// stale count when `check_stale` was requested.
    pub stale_files: usize,
    pub n_chunks_considered: usize,
    pub walk_ms: u128,
    pub rank_ms: u128,
    pub total_ms: u128,
    /// Performance provenance: ordered (stage, ms) pairs covering the whole
    /// query so bottlenecks are attributable from a single --stats line.
    pub stages: Vec<(String, f64)>,
}

pub struct SearchResult {
    pub hits: Vec<SearchHit>,
    pub report: SearchReport,
}

pub fn search(root: &Path, query: &str, opts: &SearchOptions) -> Result<SearchResult> {
    let t0 = Instant::now();
    if opts.mode == Mode::Keyword {
        let hits = keyword::scan(root, query, &opts.keyword)?;
        let hits = hits
            .into_iter()
            .map(|h| SearchHit {
                path: h.path,
                start_line: h.line as u32,
                end_line: h.line as u32,
                line: h.line as u32,
                text: h.text,
                score: 1.0,
            })
            .collect();
        return Ok(SearchResult {
            hits,
            report: SearchReport { total_ms: t0.elapsed().as_millis(), ..Default::default() },
        });
    }

    // The index is a cache (RESEARCH.md §8): resolve one for this scope —
    // local, ancestor, or central — and on a full miss, write-through: the
    // cold pass is the same work as a build, so persist it and answer warm.
    let mut wrote_cache = false;
    let discovered = if opts.no_index {
        None
    } else {
        index::discover(root).or_else(|| {
            let canon = std::fs::canonicalize(root).ok()?;
            let build = index::BuildOptions { params: opts.params, ..Default::default() };
            index::write_cache_entry(&canon, &build, |_, _| {}).ok()?;
            wrote_cache = true;
            index::discover(root)
        })
    };

    // A cache entry is disposable: if it cannot be read for *any* reason —
    // truncated write, half-deleted directory, a format this binary predates
    // — that is a miss, not the caller's problem. Discard it and answer from
    // the streaming path, which repopulates on the way through. A repo-local
    // `.semgrep` is an explicit artifact, so its failures still propagate.
    let mut result = match discovered {
        Some(d) => match search_indexed(&d, query, opts) {
            Ok(r) => r,
            Err(_) if d.from_cache => {
                let _ = std::fs::remove_dir_all(&d.index_dir);
                search_streaming(root, query, opts)?
            }
            Err(e) => return Err(e),
        },
        None => search_streaming(root, query, opts)?,
    };
    result.report.wrote_cache = wrote_cache;
    result.report.total_ms = t0.elapsed().as_millis();
    Ok(result)
}

// ---------------------------------------------------------------------------
// read-repair overlay: correctness against a possibly-stale/partial cache
// ---------------------------------------------------------------------------

/// In-memory index over the files the cache doesn't know correctly: changed,
/// new, or never-covered. Fused with the warm base at rank time so answers
/// are always true of the current tree (RESEARCH.md §8 "read-repair", §8.1
/// "lazy fill" — same mechanism).
struct Delta {
    chunks: Vec<Chunk>,
    /// Index-root-relative path per delta chunk.
    paths: Vec<String>,
    /// Normalized f32 embeddings per delta chunk.
    vecs: Vec<Vec<f32>>,
    bm25: Bm25Index,
}

struct Repair {
    /// Base file_ids whose chunks must not be served (changed or deleted).
    tombstones: HashSet<u32>,
    delta: Delta,
    n_dirty: usize,
}

fn repair_ttl_secs() -> u64 {
    static TTL: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *TTL.get_or_init(|| {
        std::env::var("SEMGREP_CACHE_TTL_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(60)
    })
}

/// Throttled scoped validation: diff the live tree under the query scope
/// against the index's file table; build the overlay if anything drifted.
fn repair_scope(
    d: &index::Discovered,
    idx: &index::LoadedIndex,
    stages: &mut Vec<(String, f64)>,
) -> Option<Repair> {
    let marker = d.index_dir.join("last_check");
    let ttl = repair_ttl_secs();
    if ttl > 0 {
        let fresh = std::fs::metadata(&marker)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age.as_secs() < ttl);
        if fresh {
            return None;
        }
    }
    let _ = std::fs::write(&marker, b"");

    let t0 = Instant::now();
    let scope_abs = if d.prefix.is_empty() { d.root.clone() } else { d.root.join(&d.prefix) };
    let live = corpus::walk(&scope_abs, &idx.meta.params).ok()?;
    let under_prefix = |path: &str| {
        d.prefix.is_empty() || path.strip_prefix(&d.prefix).is_some_and(|r| r.starts_with('/'))
    };
    let mut known: HashMap<&str, (u32, u64, u64)> = idx
        .meta
        .files
        .iter()
        .enumerate()
        .filter(|(_, f)| under_prefix(&f.path))
        .map(|(id, f)| (f.path.as_str(), (id as u32, f.size, f.mtime)))
        .collect();
    let mut changed: Vec<String> = Vec::new();
    let mut tombstones: HashSet<u32> = HashSet::new();
    let mut n_modified = 0usize;
    for f in &live {
        let irel = if d.prefix.is_empty() {
            f.path.clone()
        } else {
            format!("{}/{}", d.prefix, f.path)
        };
        match known.remove(irel.as_str()) {
            Some((_, size, mtime)) if size == f.size && mtime == f.mtime => {}
            Some((id, _, _)) => {
                n_modified += 1;
                tombstones.insert(id);
                changed.push(irel);
            }
            None => changed.push(irel),
        }
    }
    // Files the index knows under this scope that no longer exist.
    for (_, (id, _, _)) in known {
        tombstones.insert(id);
    }
    stages.push(("repair:walk".into(), t0.elapsed().as_secs_f64() * 1e3));
    if changed.is_empty() && tombstones.is_empty() {
        return None;
    }

    let t0 = Instant::now();
    let mut delta = Delta {
        chunks: Vec::new(),
        paths: Vec::new(),
        vecs: Vec::new(),
        bm25: Bm25Index::new(),
    };
    let mut texts: Vec<String> = Vec::new();
    for path in &changed {
        let Some(text) = corpus::read_text(&d.root.join(path)) else { continue };
        for (chunk, slice) in corpus::chunk_lines(0, &text, &idx.meta.params) {
            let doc = corpus::doc_text(path, slice);
            delta.bm25.add_doc(&doc);
            delta.chunks.push(chunk);
            delta.paths.push(path.clone());
            texts.push(doc);
        }
    }
    delta.bm25.finalize();
    // Delta vectors must live in the same space as the base matrix.
    let mut vecs: Vec<[f32; crate::EMBED_DIM]> = match &idx.sif {
        Some(s) => texts.iter().map(|t| semantic::embed_sif(t, s)).collect(),
        None => ese::encode(texts.iter()),
    };
    for v in &mut vecs {
        semantic::normalize(v);
    }
    delta.vecs = vecs.into_iter().map(|v| v.to_vec()).collect();
    stages.push(("repair:delta".into(), t0.elapsed().as_secs_f64() * 1e3));
    // Each drifted file counted once: new + modified (in `changed`) plus
    // deletions (tombstoned but never seen live).
    let n_dirty = changed.len() + (tombstones.len() - n_modified);
    Some(Repair { tombstones, delta, n_dirty })
}

// ---------------------------------------------------------------------------
// indexed path
// ---------------------------------------------------------------------------

fn search_indexed(
    d: &index::Discovered,
    query: &str,
    opts: &SearchOptions,
) -> Result<SearchResult> {
    let pool = FUSION_POOL.max(opts.k);
    // A one-shot CLI process pays full graph deserialization per query:
    // ~20 s for a kernel-scale (3.1 GB) hnsw.bin, vs ~3.5 s to brute-force
    // the mmap'd matrix. Until the graph has a zero-copy format, only load
    // it when it's small enough to win. (A persistent server mode amortizes
    // this and should always use the graph.)
    const HNSW_LOAD_CAP_BYTES: u64 = 1 << 30;
    let hnsw_small_enough = std::fs::metadata(d.index_dir.join("hnsw.bin"))
        .map(|m| m.len() < HNSW_LOAD_CAP_BYTES)
        .unwrap_or(false);
    let needs = index::LoadNeeds {
        bm25: matches!(opts.mode, Mode::Bm25 | Mode::Hybrid),
        hnsw: matches!(opts.mode, Mode::Semantic | Mode::Hybrid)
            && opts.use_hnsw
            && pool <= 128
            && hnsw_small_enough,
    };
    let t_load = Instant::now();
    let idx = LoadedIndex::load_dir(&d.index_dir, &d.root, needs)?;
    let walk_ms = t_load.elapsed().as_millis();

    let ms = |t: Instant| t.elapsed().as_secs_f64() * 1e3;
    let mut stages: Vec<(String, f64)> = vec![
        ("load:meta".into(), idx.timings.meta_ms),
        ("load:chunks".into(), idx.timings.chunks_ms),
        ("load:bm25".into(), idx.timings.bm25_ms),
        ("load:mmap".into(), idx.timings.mmap_ms),
        ("load:hnsw".into(), idx.timings.hnsw_ms),
    ];

    // Read-repair overlay: tombstone drifted files out of the warm base and
    // rank their current content (plus never-covered files) in memory.
    let repair = repair_scope(d, &idx, &mut stages);
    let n_base = idx.chunks.len() as u32;
    let tombstoned = |id: u32| {
        repair.as_ref().is_some_and(|r| r.tombstones.contains(&idx.chunks[id as usize].file_id))
    };

    // Map a ranked id (base row or delta overlay) to its chunk + path.
    let resolve = |id: u32| -> (Chunk, String) {
        if id < n_base {
            let c = idx.chunks[id as usize];
            (c, idx.file(&c).path.clone())
        } else {
            let r = repair.as_ref().expect("delta ids only exist with a repair");
            let j = (id - n_base) as usize;
            (r.delta.chunks[j], r.delta.paths[j].clone())
        }
    };

    let bm25_rank = |q: &str| -> Vec<(u32, f32)> {
        match (&idx.bm25, opts.mode) {
            (Some(b), Mode::Bm25 | Mode::Hybrid) => {
                let mut base: Vec<(u32, f32)> =
                    b.query(q, pool).into_iter().filter(|&(id, _)| !tombstoned(id)).collect();
                if let Some(r) = &repair {
                    // Delta BM25 idf differs slightly from the base corpus
                    // stats; close enough to merge for a small overlay.
                    base.extend(
                        r.delta.bm25.query(q, pool).into_iter().map(|(i, s)| (n_base + i, s)),
                    );
                    base.sort_unstable_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
                    base.truncate(pool);
                }
                base
            }
            _ => Vec::new(),
        }
    };

    let t_rank = Instant::now();
    let t0 = Instant::now();
    let mut bm25_ranked = bm25_rank(query);
    stages.push(("rank:bm25".into(), ms(t0)));

    // PRF: mine the lexical pass's top hits for discriminative terms (high
    // tf in hits, low df in corpus), expand the query, re-rank lexically.
    // "LLM query expansion without the LLM" — the NL query only has to land
    // near the target once; the neighborhood's vocabulary does the rest.
    if opts.prf_terms > 0 {
        if let Some(b) = &idx.bm25 {
            let t0 = Instant::now();
            let query_toks: HashSet<String> = tokenize::tokens(query).into_iter().collect();
            let mut tf: HashMap<String, f32> = HashMap::new();
            for &(id, _) in bm25_ranked.iter().take(10) {
                let (chunk, path) = resolve(id);
                let Some(text) = corpus::chunk_text_rel(&d.root, &path, &chunk) else {
                    continue;
                };
                tokenize::for_each_token(&text, |tok| {
                    if !query_toks.contains(tok) && tok.len() >= 3 {
                        *tf.entry(tok.to_string()).or_insert(0.0) += 1.0;
                    }
                });
            }
            let n_docs = b.n_docs() as f32;
            let mut scored: Vec<(String, f32)> = tf
                .into_iter()
                .map(|(t, f)| {
                    let df = b.df(&t).max(1) as f32;
                    let idf = ((n_docs - df + 0.5) / (df + 0.5) + 1.0).ln();
                    (t, f * idf)
                })
                .collect();
            scored.sort_unstable_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
            let expansion: Vec<String> =
                scored.into_iter().take(opts.prf_terms).map(|(t, _)| t).collect();
            if !expansion.is_empty() {
                let expanded = format!("{query} {}", expansion.join(" "));
                bm25_ranked = bm25_rank(&expanded);
            }
            stages.push(("rank:prf".into(), ms(t0)));
        }
    }
    let mut used_hnsw = false;
    let sem_ranked: Vec<(u32, f32)> = match opts.mode {
        Mode::Semantic | Mode::Hybrid => {
            let t0 = Instant::now();
            // A SIF index pools chunks by rarity weight; the query must be
            // pooled in the same space or distances are meaningless.
            let mut q = match &idx.sif {
                Some(s) => semantic::embed_sif(query, s),
                None => semantic::embed_query(query),
            };
            if idx.meta.normalized {
                semantic::normalize(&mut q);
            }
            stages.push(("rank:embed-query".into(), ms(t0)));
            let t0 = Instant::now();
            let mut ranked: Vec<(u32, f32)> = match (&idx.hnsw, opts.use_hnsw) {
                // HNSW returns up to compile-time K=128 candidates.
                (Some(h), true) if pool <= 128 => {
                    used_hnsw = true;
                    h.search(&q).into_iter().map(|(d, id)| (id, d)).take(pool).collect()
                }
                _ if idx.meta.quantized => {
                    let qi = semantic::quantize_i8(&q);
                    semantic::brute_force_top_k_i8(&qi, idx.emb_matrix_i8(), pool)
                }
                _ => {
                    semantic::brute_force_top_k(&q, idx.emb_matrix(), pool, idx.meta.normalized)
                }
            };
            ranked.retain(|&(id, _)| !tombstoned(id));
            if let Some(r) = &repair {
                // f32 distances vs the base's i8-quantized ones: quantization
                // was verified quality-neutral, so the scales merge cleanly.
                ranked.extend(
                    r.delta
                        .vecs
                        .iter()
                        .enumerate()
                        .map(|(i, v)| (n_base + i as u32, semantic::distance(&q, v))),
                );
                ranked.sort_unstable_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
                ranked.truncate(pool);
            }
            stages
                .push((if used_hnsw { "rank:ann" } else { "rank:brute" }.to_string(), ms(t0)));
            // Pre-fusion MaxSim rerank (§9.4): reorder the semantic list's
            // head by late-interaction score *before* RRF, so BM25's
            // exact-match signal is fused with it rather than overridden by
            // it (the post-fusion wiring hurt code hybrid). Head size
            // matches what the eval validated (k*3, min 24).
            if opts.rerank_maxsim && !ranked.is_empty() {
                let t0 = Instant::now();
                let qtoks = semantic::token_vectors(query, idx.sif.as_ref());
                if !qtoks.is_empty() {
                    use rayon::prelude::*;
                    // Head 96 won the §9.6 sweep on all three corpora
                    // (semantic +0.03..0.06 R@5 over head 24, ~54 ms).
                    let auto = (opts.k * 3).max(96);
                    let m = ranked.len().min(if opts.maxsim_pool > 0 {
                        opts.maxsim_pool
                    } else {
                        auto
                    });
                    let scored: Vec<(u32, f32, f32)> = ranked[..m]
                        .par_iter()
                        .map(|&(id, dist)| {
                            let (chunk, path) = resolve(id);
                            let sim = corpus::chunk_text_rel(&d.root, &path, &chunk)
                                .map(|text| {
                                    let dtoks = semantic::token_vectors(
                                        &corpus::doc_text(&path, &text),
                                        None,
                                    );
                                    semantic::maxsim(&qtoks, &dtoks)
                                })
                                .unwrap_or(f32::NEG_INFINITY);
                            (id, sim, dist)
                        })
                        .collect();
                    // Blend MaxSim with the original embedding order (both
                    // min-max normalized within the head) when < 1.0.
                    let alpha = opts.maxsim_blend.clamp(0.0, 1.0);
                    let norm = |xs: Vec<f32>| -> Vec<f32> {
                        let (lo, hi) = xs
                            .iter()
                            .fold((f32::MAX, f32::MIN), |(l, h), &x| (l.min(x), h.max(x)));
                        let span = (hi - lo).max(f32::EPSILON);
                        xs.into_iter().map(|x| (x - lo) / span).collect()
                    };
                    let sim_n = norm(scored.iter().map(|&(_, s, _)| s).collect());
                    let emb_n = norm(scored.iter().map(|&(_, _, d)| -d).collect());
                    let mut head: Vec<(u32, f32)> = scored
                        .iter()
                        .zip(sim_n.iter().zip(emb_n.iter()))
                        .map(|(&(id, _, _), (&s, &e))| {
                            // Combined similarity → pseudo-distance so the
                            // list keeps its ascending-is-better contract.
                            (id, -(alpha * s + (1.0 - alpha) * e))
                        })
                        .collect();
                    head.sort_unstable_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
                    head.extend_from_slice(&ranked[m..]);
                    ranked = head;
                }
                stages.push(("rank:maxsim".into(), ms(t0)));
            }
            ranked
        }
        _ => Vec::new(),
    };
    let t0 = Instant::now();
    // Fuse over the full pool, then filter to the query's subtree *before*
    // truncating — otherwise out-of-scope hits can eat every result slot.
    let ranked = fuse(opts.mode, bm25_ranked, sem_ranked, pool * 2, opts.sem_weight);
    stages.push(("rank:fuse".into(), ms(t0)));
    let rank_ms = t_rank.elapsed().as_millis();

    let in_scope = |path: &str| {
        d.prefix.is_empty() || path.strip_prefix(&d.prefix).is_some_and(|r| r.starts_with('/'))
    };
    let cands: Vec<Candidate> = ranked
        .into_iter()
        .filter_map(|(id, score)| {
            let (chunk, path) = resolve(id);
            in_scope(&path).then_some(Candidate { id, chunk, path, score })
        })
        .take(opts.k * 3)
        .collect();
    // Chunk ids equal embedding-matrix row ids, so vectors are free here
    // (dequantized on the fly for v2 indexes — a handful of rows).
    let t0 = Instant::now();
    let n_delta = repair.as_ref().map_or(0, |r| r.delta.chunks.len());
    let hits = finalize_hits(&d.root, query, cands, opts, &d.prefix, |c| {
        if c.id >= n_base {
            let r = repair.as_ref()?;
            return Some(r.delta.vecs[(c.id - n_base) as usize].clone());
        }
        let row = c.id as usize;
        if idx.meta.quantized {
            let m = idx.emb_matrix_i8();
            Some(
                m[row * crate::EMBED_DIM..(row + 1) * crate::EMBED_DIM]
                    .iter()
                    .map(|&x| x as f32 / 127.0)
                    .collect(),
            )
        } else {
            let m = idx.emb_matrix();
            Some(m[row * crate::EMBED_DIM..(row + 1) * crate::EMBED_DIM].to_vec())
        }
    });
    stages.push(("finalize".into(), ms(t0)));

    Ok(SearchResult {
        hits,
        report: SearchReport {
            used_index: true,
            used_hnsw,
            stale_files: if opts.check_stale {
                idx.stale_files().unwrap_or(0)
            } else {
                repair.as_ref().map_or(0, |r| r.n_dirty)
            },
            n_chunks_considered: idx.chunks.len() + n_delta,
            walk_ms,
            rank_ms,
            stages,
            ..Default::default()
        },
    })
}

// ---------------------------------------------------------------------------
// streaming (unindexed) path — one pass, bounded memory for semantic;
// BM25 postings are held in memory for the duration of the query.
// ---------------------------------------------------------------------------

fn search_streaming(root: &Path, query: &str, opts: &SearchOptions) -> Result<SearchResult> {
    let want_bm25 = matches!(opts.mode, Mode::Bm25 | Mode::Hybrid);
    let want_sem = matches!(opts.mode, Mode::Semantic | Mode::Hybrid);
    let pool = FUSION_POOL.max(opts.k);

    let t_walk = Instant::now();
    let files = corpus::walk(root, &opts.params)?;
    let walk_ms = t_walk.elapsed().as_millis();

    let t_rank = Instant::now();
    let qvec = want_sem.then(|| semantic::embed_query(query));
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
                top.push(*id, semantic::distance(q, v));
            }
            embed_ms.set(embed_ms.get() + ms(t0));
        }
        pending.clear();
    };

    // Parallel batched pass (see index::build for the rationale): per-file
    // read/chunk/tokenize on rayon workers, serial in-order fold so chunk
    // ids stay deterministic.
    use rayon::prelude::*;
    let t_pass = Instant::now();
    for (base, batch) in corpus::pass_batches(&files, 256, 16 << 20) {
        let works: Vec<corpus::FileWork> = batch
            .par_iter()
            .enumerate()
            .map(|(i, fm)| {
                corpus::process_file(
                    root,
                    (base + i) as u32,
                    fm,
                    &opts.params,
                    want_sem,
                    want_bm25,
                )
            })
            .collect();
        for work in works {
            for (chunk, doc, tokens) in work.docs {
                let chunk_id = chunks.len() as u32;
                chunks.push(chunk);
                if let (Some(b), Some(t)) = (&mut bm25, tokens) {
                    b.add_tokenized(t);
                }
                if let Some(doc) = doc {
                    pending.push((chunk_id, doc));
                    if pending.len() >= 1024 {
                        flush(&mut pending, &mut top);
                    }
                }
            }
        }
    }
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
    let ranked = fuse(opts.mode, bm25_ranked, sem_ranked, opts.k * 3, opts.sem_weight);
    stages.push(("rank:fuse".into(), ms(t0)));
    let rank_ms = t_rank.elapsed().as_millis();

    let cands: Vec<Candidate> = ranked
        .into_iter()
        .map(|(chunk_id, score)| {
            let chunk = chunks[chunk_id as usize];
            Candidate {
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
    let hits = finalize_hits(root, query, cands, opts, "", |c| {
        let fm = &files[c.chunk.file_id as usize];
        let text = corpus::chunk_text(root, fm, &c.chunk).ok()?;
        Some(semantic::embed_query(&corpus::doc_text(&fm.path, &text)).to_vec())
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

// ---------------------------------------------------------------------------
// fusion + result materialization
// ---------------------------------------------------------------------------

/// For single-engine modes, pass scores through (semantic distance is turned
/// into a similarity so higher is always better). For hybrid, weighted RRF:
/// BM25 contributes at weight 1.0, semantic at `sem_weight` — evals showed
/// the lexical list is the stronger engine and equal weighting diluted it.
fn fuse(
    mode: Mode,
    bm25: Vec<(u32, f32)>,
    sem: Vec<(u32, f32)>,
    k: usize,
    sem_weight: f32,
) -> Vec<(u32, f32)> {
    let mut out = match mode {
        Mode::Bm25 => bm25,
        Mode::Semantic => sem.into_iter().map(|(id, d)| (id, 1.0 - d)).collect(),
        Mode::Hybrid => {
            let mut rrf: HashMap<u32, f32> = HashMap::new();
            for (rank, (id, _)) in bm25.iter().enumerate() {
                *rrf.entry(*id).or_insert(0.0) += 1.0 / (RRF_K + rank as f32 + 1.0);
            }
            for (rank, (id, _)) in sem.iter().enumerate() {
                *rrf.entry(*id).or_insert(0.0) += sem_weight / (RRF_K + rank as f32 + 1.0);
            }
            let mut v: Vec<(u32, f32)> = rrf.into_iter().collect();
            v.sort_unstable_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
            v
        }
        Mode::Keyword => unreachable!("keyword handled earlier"),
    };
    out.truncate(k);
    out
}

// ---------------------------------------------------------------------------
// finalization: span dedupe → MMR diversity → materialized hits
// ---------------------------------------------------------------------------

struct Candidate {
    /// Chunk id == row in the chunk table / embedding matrix.
    id: u32,
    chunk: Chunk,
    path: String,
    score: f32,
}

/// Shared tail of both search paths. `vec_of` supplies the embedding for a
/// candidate (by index into `cands`) when diversification needs it. `strip`
/// is the query's subtree prefix: candidate paths are index-root-relative
/// for file access, but hits display relative to the queried scope (the same
/// contract the streaming path has always had).
fn finalize_hits(
    root: &Path,
    query: &str,
    mut cands: Vec<Candidate>,
    opts: &SearchOptions,
    strip: &str,
    vec_of: impl Fn(&Candidate) -> Option<Vec<f32>>,
) -> Vec<SearchHit> {
    // Drop candidates whose line span overlaps an already-kept, higher-ranked
    // candidate in the same file — overlapping windows are near-duplicates.
    let mut kept: Vec<Candidate> = Vec::with_capacity(cands.len());
    for c in cands.drain(..) {
        let dup = kept.iter().any(|k| {
            k.path == c.path
                && c.chunk.start_line <= k.chunk.end_line
                && c.chunk.end_line >= k.chunk.start_line
        });
        if !dup {
            kept.push(c);
        }
    }

    // MMR: greedily pick relevant-but-dissimilar candidates so the top-k
    // surfaces different parts of the corpus instead of one hot region.
    let order: Vec<usize> = if opts.diversify && kept.len() > opts.k && opts.k > 1 {
        let vecs: Vec<Option<Vec<f32>>> = kept.iter().map(&vec_of).collect();
        mmr_order(&kept, &vecs, opts.k, opts.mmr_lambda)
    } else {
        (0..kept.len().min(opts.k)).collect()
    };

    let query_tokens: HashSet<String> = tokenize::tokens(query).into_iter().collect();
    let mut hits = Vec::with_capacity(opts.k);
    for i in order {
        let c = &kept[i];
        if let Some(mut hit) = materialize(root, &c.path, c.chunk, c.score, &query_tokens) {
            if !strip.is_empty() {
                if let Some(rest) = hit.path.strip_prefix(&format!("{strip}/")) {
                    hit.path = rest.to_string();
                }
            }
            hits.push(hit);
        }
        if hits.len() == opts.k {
            break;
        }
    }
    hits
}

/// Greedy maximal-marginal-relevance ordering over candidates.
/// Relevance = min-max-normalized fused score; similarity = embedding cosine
/// (candidates lacking a vector are treated as dissimilar to everything).
fn mmr_order(
    cands: &[Candidate],
    vecs: &[Option<Vec<f32>>],
    k: usize,
    lambda: f32,
) -> Vec<usize> {
    let (lo, hi) = cands
        .iter()
        .fold((f32::MAX, f32::MIN), |(lo, hi), c| (lo.min(c.score), hi.max(c.score)));
    let span = (hi - lo).max(f32::EPSILON);
    let rel: Vec<f32> = cands.iter().map(|c| (c.score - lo) / span).collect();

    let mut selected: Vec<usize> = Vec::with_capacity(k);
    let mut remaining: Vec<usize> = (0..cands.len()).collect();
    while selected.len() < k && !remaining.is_empty() {
        let (pos, &best) = remaining
            .iter()
            .enumerate()
            .max_by(|&(_, &a), &(_, &b)| {
                let ma = lambda * rel[a] - (1.0 - lambda) * max_sim(a, &selected, vecs);
                let mb = lambda * rel[b] - (1.0 - lambda) * max_sim(b, &selected, vecs);
                ma.total_cmp(&mb).then(b.cmp(&a)) // tie → lower index (higher rank)
            })
            .unwrap();
        selected.push(best);
        remaining.swap_remove(pos);
    }
    selected
}

fn max_sim(i: usize, selected: &[usize], vecs: &[Option<Vec<f32>>]) -> f32 {
    let Some(vi) = &vecs[i] else { return 0.0 };
    selected
        .iter()
        .filter_map(|&s| vecs[s].as_ref())
        .map(|vs| 1.0 - semantic::distance(vi, vs))
        .fold(0.0f32, f32::max)
}

/// Turn a ranked chunk into a displayable hit: re-read the file and pick the
/// line with the highest query-token overlap (first non-empty line as
/// fallback). Skips chunks whose file vanished since ranking.
fn materialize(
    root: &Path,
    rel_path: &str,
    chunk: Chunk,
    score: f32,
    query_tokens: &HashSet<String>,
) -> Option<SearchHit> {
    let text = corpus::read_text(&root.join(rel_path))?;
    let mut best: Option<(usize, u32, &str)> = None;
    for (i, line) in text.lines().enumerate() {
        let line_no = i as u32 + 1;
        if line_no < chunk.start_line {
            continue;
        }
        if line_no > chunk.end_line {
            break;
        }
        if line.trim().is_empty() {
            continue;
        }
        let mut overlap = 0usize;
        tokenize::for_each_token(line, |tok| {
            if query_tokens.contains(tok) {
                overlap += 1;
            }
        });
        match &best {
            Some((b, _, _)) if *b >= overlap => {}
            _ => best = Some((overlap, line_no, line)),
        }
    }
    let (_, line, line_text) = best?;
    Some(SearchHit {
        path: rel_path.to_string(),
        start_line: chunk.start_line,
        end_line: chunk.end_line,
        line,
        text: line_text.to_string(),
        score,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(id: u32, path: &str, start: u32, score: f32) -> Candidate {
        Candidate {
            id,
            chunk: Chunk { file_id: 0, start_line: start, end_line: start + 7 },
            path: path.to_string(),
            score,
        }
    }

    #[test]
    fn weighted_rrf_favors_bm25() {
        // doc 1 tops bm25, doc 2 tops semantic; equal ranks otherwise.
        let bm25 = vec![(1, 9.0), (2, 5.0)];
        let sem = vec![(2, 0.1), (1, 0.4)];
        let fused = fuse(Mode::Hybrid, bm25, sem, 10, 0.5);
        assert_eq!(fused[0].0, 1, "bm25 winner should lead at sem_weight<1");
        let equal =
            fuse(Mode::Hybrid, vec![(1, 9.0), (2, 5.0)], vec![(2, 0.1), (1, 0.4)], 10, 1.0);
        // sanity: at weight 1.0 the two docs tie on RRF and order falls to id
        assert_eq!(equal.len(), 2);
    }

    #[test]
    fn mmr_prefers_diverse_over_redundant() {
        // a and b are near-identical vectors with top scores; c is distinct
        // with a slightly lower score. With diversity on, c should beat b.
        let cands =
            vec![cand(0, "a.rs", 1, 1.0), cand(1, "b.rs", 1, 0.95), cand(2, "c.rs", 1, 0.80)];
        let mut va = vec![0.0f32; 8];
        va[0] = 1.0;
        let mut vb = va.clone();
        vb[1] = 0.05; // nearly parallel to va
        let mut vc = vec![0.0f32; 8];
        vc[3] = 1.0; // orthogonal
        let vecs = vec![Some(va), Some(vb), Some(vc)];
        let order = mmr_order(&cands, &vecs, 2, 0.5);
        assert_eq!(order, vec![0, 2]);
    }

    #[test]
    fn overlapping_spans_dedupe_keeps_higher_rank() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("x.rs"), "fn alpha() {}\n".repeat(60)).unwrap();
        let cands = vec![
            Candidate {
                id: 0,
                chunk: Chunk { file_id: 0, start_line: 1, end_line: 32 },
                path: "x.rs".into(),
                score: 1.0,
            },
            Candidate {
                id: 1,
                chunk: Chunk { file_id: 0, start_line: 25, end_line: 56 },
                path: "x.rs".into(),
                score: 0.9,
            },
        ];
        let opts = SearchOptions { k: 2, ..Default::default() };
        let hits = finalize_hits(dir.path(), "alpha", cands, &opts, "", |_| None);
        assert_eq!(hits.len(), 1, "overlapping same-file spans should collapse");
        assert_eq!(hits[0].start_line, 1);
    }
}
