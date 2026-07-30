//! The warm path: an index answers, with a read-repair overlay fused in.

use super::hit;
use super::{SearchOptions, SearchReport, SearchResult};
use crate::rank::bm25::Postings;
use crate::rank::{self, Mode};
use crate::store::LoadedIndex;
use crate::text::token as tokenize;
use crate::{Chunk, cache, corpus, store, text};
use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::time::Instant;

pub fn run(d: &cache::Discovered, query: &str, opts: &SearchOptions) -> Result<SearchResult> {
    let pool = super::FUSION_POOL.max(opts.k);
    // A one-shot CLI process pays full graph deserialization per query:
    // ~20 s for a kernel-scale (3.1 GB) hnsw.bin, vs ~3.5 s to brute-force
    // the mmap'd matrix. Until the graph has a zero-copy format, only load
    // it when it's small enough to win. (A persistent server mode amortizes
    // this and should always use the graph.)
    const HNSW_LOAD_CAP_BYTES: u64 = 1 << 30;
    let hnsw_small_enough = std::fs::metadata(d.index_dir.join("hnsw.bin"))
        .map(|m| m.len() < HNSW_LOAD_CAP_BYTES)
        .unwrap_or(false);
    let needs = store::LoadNeeds {
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
    let repair = cache::repair::scope(d, &idx, &mut stages);
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
    if opts.prf_terms > 0
        && let Some(b) = &idx.bm25
    {
        let t0 = Instant::now();
        let query_toks: HashSet<String> = tokenize::tokens(query).into_iter().collect();
        let mut tf: HashMap<String, f32> = HashMap::new();
        for &(id, _) in bm25_ranked.iter().take(10) {
            let (chunk, path) = resolve(id);
            let Some(text) = corpus::lines(&d.root, &path, &chunk) else { continue };
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
    let mut used_hnsw = false;
    let sem_ranked: Vec<(u32, f32)> = match opts.mode {
        Mode::Semantic | Mode::Hybrid => {
            let t0 = Instant::now();
            // A SIF index pools chunks by rarity weight; the query must be
            // pooled in the same space or distances are meaningless.
            let mut q = match &idx.sif {
                Some(s) => text::embed_sif(query, s),
                None => text::embed_query(query),
            };
            rank::normalize(&mut q);
            stages.push(("rank:embed-query".into(), ms(t0)));
            let t0 = Instant::now();
            let mut ranked: Vec<(u32, f32)> = match (&idx.hnsw, opts.use_hnsw) {
                // HNSW returns up to compile-time K=128 candidates.
                (Some(h), true) if pool <= 128 => {
                    used_hnsw = true;
                    h.search(&q).into_iter().map(|(d, id)| (id, d)).take(pool).collect()
                }
                _ => {
                    let qi = rank::quantize_i8(&q);
                    rank::brute_force_top_k_i8(&qi, idx.emb_matrix_i8(), pool)
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
                        .map(|(i, v)| (n_base + i as u32, rank::distance(&q, v))),
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
                let qtoks = text::token_vectors(query, idx.sif.as_ref());
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
                            let sim = corpus::lines(&d.root, &path, &chunk)
                                .map(|text| {
                                    let dtoks = text::token_vectors(
                                        &corpus::doc_text(&path, &text),
                                        None,
                                    );
                                    rank::maxsim(&qtoks, &dtoks)
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
    let ranked = rank::fuse(opts.mode, bm25_ranked, sem_ranked, pool * 2, opts.sem_weight);
    stages.push(("rank:fuse".into(), ms(t0)));
    let rank_ms = t_rank.elapsed().as_millis();

    let in_scope = |path: &str| {
        d.prefix.is_empty() || path.strip_prefix(&d.prefix).is_some_and(|r| r.starts_with('/'))
    };
    let cands: Vec<hit::Candidate> = ranked
        .into_iter()
        .filter_map(|(id, score)| {
            let (chunk, path) = resolve(id);
            in_scope(&path).then_some(hit::Candidate { id, chunk, path, score })
        })
        .take(opts.k * 3)
        .collect();
    // Chunk ids equal embedding-matrix row ids, so vectors are free here —
    // dequantized on the fly, and only for the handful of candidate rows.
    let t0 = Instant::now();
    let n_delta = repair.as_ref().map_or(0, |r| r.delta.chunks.len());
    let hits = hit::finalize(&d.root, query, cands, opts, &d.prefix, |c| {
        if c.id >= n_base {
            let r = repair.as_ref()?;
            return Some(r.delta.vecs[(c.id - n_base) as usize].clone());
        }
        let row = c.id as usize;
        let m = idx.emb_matrix_i8();
        Some(
            m[row * crate::EMBED_DIM..(row + 1) * crate::EMBED_DIM]
                .iter()
                .map(|&x| x as f32 / 127.0)
                .collect(),
        )
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
