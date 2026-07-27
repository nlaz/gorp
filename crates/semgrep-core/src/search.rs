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

    let mut result = if !opts.no_index && index::exists(root) {
        search_indexed(root, query, opts)?
    } else {
        search_streaming(root, query, opts)?
    };
    result.report.total_ms = t0.elapsed().as_millis();
    Ok(result)
}

// ---------------------------------------------------------------------------
// indexed path
// ---------------------------------------------------------------------------

fn search_indexed(root: &Path, query: &str, opts: &SearchOptions) -> Result<SearchResult> {
    let pool = FUSION_POOL.max(opts.k);
    // A one-shot CLI process pays full graph deserialization per query:
    // ~20 s for a kernel-scale (3.1 GB) hnsw.bin, vs ~3.5 s to brute-force
    // the mmap'd matrix. Until the graph has a zero-copy format, only load
    // it when it's small enough to win. (A persistent server mode amortizes
    // this and should always use the graph.)
    const HNSW_LOAD_CAP_BYTES: u64 = 1 << 30;
    let hnsw_small_enough = std::fs::metadata(index::index_dir(root).join("hnsw.bin"))
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
    let idx = LoadedIndex::load(root, needs)?;
    let walk_ms = t_load.elapsed().as_millis();

    let ms = |t: Instant| t.elapsed().as_secs_f64() * 1e3;
    let mut stages: Vec<(String, f64)> = vec![
        ("load:meta".into(), idx.timings.meta_ms),
        ("load:chunks".into(), idx.timings.chunks_ms),
        ("load:bm25".into(), idx.timings.bm25_ms),
        ("load:mmap".into(), idx.timings.mmap_ms),
        ("load:hnsw".into(), idx.timings.hnsw_ms),
    ];

    let t_rank = Instant::now();
    let t0 = Instant::now();
    let bm25_ranked: Vec<(u32, f32)> = match (&idx.bm25, opts.mode) {
        (Some(b), Mode::Bm25 | Mode::Hybrid) => b.query(query, pool),
        _ => Vec::new(),
    };
    stages.push(("rank:bm25".into(), ms(t0)));
    let mut used_hnsw = false;
    let sem_ranked: Vec<(u32, f32)> = match opts.mode {
        Mode::Semantic | Mode::Hybrid => {
            let t0 = Instant::now();
            let mut q = semantic::embed_query(query);
            if idx.meta.normalized {
                semantic::normalize(&mut q);
            }
            stages.push(("rank:embed-query".into(), ms(t0)));
            let t0 = Instant::now();
            let ranked = match (&idx.hnsw, opts.use_hnsw) {
                // HNSW returns up to compile-time K=128 candidates.
                (Some(h), true) if pool <= 128 => {
                    used_hnsw = true;
                    h.search(&q).into_iter().map(|(d, id)| (id, d)).take(pool).collect()
                }
                _ if idx.meta.quantized => {
                    let qi = semantic::quantize_i8(&q);
                    semantic::brute_force_top_k_i8(&qi, idx.emb_matrix_i8(), pool)
                }
                _ => semantic::brute_force_top_k(&q, idx.emb_matrix(), pool, idx.meta.normalized),
            };
            stages.push((
                if used_hnsw { "rank:ann" } else { "rank:brute" }.to_string(),
                ms(t0),
            ));
            ranked
        }
        _ => Vec::new(),
    };
    let t0 = Instant::now();
    let ranked = fuse(opts.mode, bm25_ranked, sem_ranked, opts.k * 3, opts.sem_weight);
    stages.push(("rank:fuse".into(), ms(t0)));
    let rank_ms = t_rank.elapsed().as_millis();

    let cands: Vec<Candidate> = ranked
        .into_iter()
        .map(|(chunk_id, score)| {
            let chunk = idx.chunks[chunk_id as usize];
            Candidate {
                id: chunk_id,
                chunk,
                path: idx.file(&chunk).path.clone(),
                score,
            }
        })
        .collect();
    // Chunk ids equal embedding-matrix row ids, so vectors are free here
    // (dequantized on the fly for v2 indexes — a handful of rows).
    let t0 = Instant::now();
    let hits = finalize_hits(root, query, cands, opts, |c| {
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
                0
            },
            n_chunks_considered: idx.chunks.len(),
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

    let t_pass = Instant::now();
    for (file_id, fm) in files.iter().enumerate() {
        let Some(text) = corpus::read_text(&corpus::abs_path(root, fm)) else {
            continue;
        };
        for (chunk, slice) in corpus::chunk_lines(file_id as u32, &text, &opts.params) {
            let chunk_id = chunks.len() as u32;
            chunks.push(chunk);
            let doc = corpus::doc_text(&fm.path, slice);
            if let Some(b) = &mut bm25 {
                b.add_doc(&doc);
            }
            if want_sem {
                pending.push((chunk_id, doc));
                if pending.len() >= 1024 {
                    flush(&mut pending, &mut top);
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
    let hits = finalize_hits(root, query, cands, opts, |c| {
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
/// candidate (by index into `cands`) when diversification needs it.
fn finalize_hits(
    root: &Path,
    query: &str,
    mut cands: Vec<Candidate>,
    opts: &SearchOptions,
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
        if let Some(hit) = materialize(root, &c.path, c.chunk, c.score, &query_tokens) {
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
        let equal = fuse(
            Mode::Hybrid,
            vec![(1, 9.0), (2, 5.0)],
            vec![(2, 0.1), (1, 0.4)],
            10,
            1.0,
        );
        // sanity: at weight 1.0 the two docs tie on RRF and order falls to id
        assert_eq!(equal.len(), 2);
    }

    #[test]
    fn mmr_prefers_diverse_over_redundant() {
        // a and b are near-identical vectors with top scores; c is distinct
        // with a slightly lower score. With diversity on, c should beat b.
        let cands = vec![
            cand(0, "a.rs", 1, 1.0),
            cand(1, "b.rs", 1, 0.95),
            cand(2, "c.rs", 1, 0.80),
        ];
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
        let hits = finalize_hits(dir.path(), "alpha", cands, &opts, |_| None);
        assert_eq!(hits.len(), 1, "overlapping same-file spans should collapse");
        assert_eq!(hits[0].start_line, 1);
    }
}
