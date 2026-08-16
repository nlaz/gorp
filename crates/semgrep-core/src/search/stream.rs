//! The cold path: one streaming pass over the corpus, no index involved.
//!
//! Mirrors `indexed` — rank lexically, rank semantically, fuse, materialize —
//! but computes the representations instead of loading them, and keeps only what
//! a single query needs. The semantic side holds a top-k heap rather than a
//! matrix, so its memory is bounded by k; BM25 postings are held for the
//! duration of the query, which is the larger cost and is why a big corpus wants
//! an index.

use super::{Prelude, SearchOptions, SearchReport, SearchResult, hit};
use crate::rank::bm25::Bm25Index;
use crate::rank::{self, Mode, TopK};
use crate::trace::{SCHEDULE_COLD, Stage, Trace, elapsed_ms};
use crate::{Chunk, FileMeta, corpus, text};
use anyhow::Result;
use std::path::Path;
use std::time::Instant;

pub fn run(
    root: &Path,
    q: &super::Query,
    opts: &SearchOptions,
    prelude: &Prelude,
) -> Result<SearchResult> {
    let pool = super::FUSION_POOL.max(opts.k);
    let mut trace = Trace::new(SCHEDULE_COLD);
    trace.replay(prelude);

    let files = trace.time(Stage::Walk, || corpus::walk(root, &opts.params))?;

    // ONE pass over the corpus regardless of phrase count (RESEARCH.md §31):
    // the pass is the whole cost of a cold search, and the Embedder scores
    // every batch row against each phrase's vector — an extra dot per phrase,
    // against a file read it already paid for.
    let mut pass = corpus_pass(root, &files, &q.phrases, opts, pool, &mut trace);
    trace.time(Stage::RankBm25, || {
        if let Some(b) = pass.bm25.as_mut() {
            b.finalize();
        }
    });
    // Bridge expansion (§33): mining needs the finalized postings, but the
    // semantic heaps were filled during the pass with the ORIGINAL phrase
    // vectors — and the effect is mostly semantic-side (§33.0). When any
    // phrase expands, the corpus is passed AGAIN, sem-only, with the
    // expanded strings, and those heaps replace the first pass's. Cold pays
    // a second read when the flag fires; warm pays one extra query
    // embedding — the usual asymmetry, and the reason indexes exist.
    let mut expanded_phrases: Vec<String> = q.phrases.clone();
    let mut per_phrase_terms: Vec<Vec<String>> = vec![Vec::new(); q.phrases.len()];
    if opts.bridge_expand > 0
        && let Some(b) = pass.bm25.as_ref()
    {
        trace.time(Stage::RankBridge, || {
            for (i, phrase) in q.phrases.iter().enumerate() {
                let terms = rank::bridge::bridge_terms(
                    phrase,
                    b,
                    |cid| files[pass.chunks[cid as usize].file_id as usize].path.clone(),
                    None,
                    |path| std::fs::read_to_string(root.join(path)).ok(),
                    opts.bridge_expand,
                );
                if !terms.is_empty() {
                    let mut s = phrase.clone();
                    for t in &terms {
                        s.push(' ');
                        s.push_str(t);
                    }
                    expanded_phrases[i] = s;
                    per_phrase_terms[i] = terms;
                }
            }
        });
        if per_phrase_terms.iter().any(|t| !t.is_empty())
            && matches!(opts.mode, Mode::Semantic | Mode::Hybrid)
        {
            // Chunk ids are a deterministic function of (files, params), so
            // the re-pass's heap ids align with `pass.chunks` — the same
            // lockstep the build asserts.
            let sem_only = SearchOptions {
                bridge_expand: 0,
                bm25_pin: 0,
                mode: Mode::Semantic,
                ..opts.clone()
            };
            let pass2 = corpus_pass(root, &files, &expanded_phrases, &sem_only, pool, &mut trace);
            pass.nearest = pass2.nearest;
        }
    }
    let mut nearest = pass.nearest.take();

    let mut per_phrase: Vec<Vec<hit::Candidate>> = Vec::with_capacity(q.phrases.len());
    let mut bridge_terms_used: Vec<String> = Vec::new();
    for (p, query) in q.phrases.iter().enumerate() {
        let query = query.as_str();
        let lexical = trace.time(Stage::RankBm25, || match pass.bm25.as_ref() {
            Some(b) => b.query(query, pool),
            None => Vec::new(),
        });
        // PRF, mirroring `indexed::expand_query` — same feedback depth, same
        // scorer, same "re-run the lexical pass with the grown query". This
        // path had no PRF at all until §33 found the split: `--prf N` made a
        // cached scope answer differently from an uncached one, which the
        // parity invariant forbids and only the `prf_terms: 0` default hid.
        let lexical = match pass.bm25.as_ref() {
            Some(b) if opts.prf_terms > 0 => trace.time(Stage::RankPrf, || {
                let texts: Vec<String> = lexical
                    .iter()
                    .take(10)
                    .filter_map(|&(id, _)| {
                        let chunk = pass.chunks[id as usize];
                        corpus::lines(root, &files[chunk.file_id as usize].path, &chunk)
                    })
                    .collect();
                let terms = rank::prf::expansion_terms(query, &texts, b, opts.prf_terms);
                if terms.is_empty() {
                    lexical
                } else {
                    b.query(&rank::prf::expand(query, &terms), pool)
                }
            }),
            _ => lexical,
        };
        // Bridge terms were mined above (they needed the finalized
        // postings); here the lexical pass re-scores with them weighted,
        // mirroring `indexed::bridge_expand`.
        let bterms = &per_phrase_terms[p];
        let lexical = match pass.bm25.as_ref() {
            Some(b) if !bterms.is_empty() => trace.time(Stage::RankBridge, || {
                let weighted = rank::bridge::weighted_terms(query, bterms, opts.bridge_weight);
                rank::top_k_weighted(b, &weighted, pool, None, None)
            }),
            _ => lexical,
        };
        for t in bterms {
            if !bridge_terms_used.contains(t) {
                bridge_terms_used.push(t.clone());
            }
        }
        // Retrieval-side maxsim sees the expanded phrase, same as warm.
        let sem_query = expanded_phrases[p].as_str();
        // Mirrors `indexed`: the lexical head survives fusion so the
        // `bm25_pin` guarantee acts identically on both paths (§32.4a).
        let bm25_head: Vec<u32> = lexical.iter().take(16).map(|r| r.0).collect();
        let semantic = nearest
            .as_mut()
            .map(|heaps| std::mem::replace(&mut heaps[p], TopK::new(0)).into_sorted())
            .unwrap_or_default();
        // The MaxSim rerank has to happen on this path too, and before fusion,
        // or cold and warm answer the same question differently — the
        // invariant `cold_and_warm_return_identical_results` exists for.
        // `indexed` reranks here; this mirrors it against the same chunk
        // texts.
        let semantic = if opts.maxsim_post {
            semantic
        } else {
            rerank_maxsim(root, &files, &pass.chunks, sem_query, semantic, opts, &mut trace)
        };
        let ranked = trace.time(Stage::RankFuse, || {
            rank::fuse(opts.mode, lexical, semantic, super::fused_width(pool), opts.sem_weight)
        });
        // Post-fusion placement, mirroring `indexed`. Both paths must rerank
        // at the same point or a cached scope answers differently from an
        // uncached one. Same sign conversion as `indexed::rerank_fused`: fuse
        // emits higher-is-better, blend_head speaks lower-is-better.
        let ranked = if opts.maxsim_post && opts.rerank_maxsim {
            let as_dist: Vec<(u32, f32)> = ranked.iter().map(|&(id, s)| (id, -s)).collect();
            let o = SearchOptions { maxsim_post: false, ..opts.clone() };
            rerank_maxsim(root, &files, &pass.chunks, sem_query, as_dist, &o, &mut trace)
                .into_iter().map(|(id, pd)| (id, -pd)).collect()
        } else {
            ranked
        };

        // Structural boost (declarations + paths), post-fusion, mirroring
        // `indexed`. Both paths must apply it at the same point or a cached
        // scope answers differently from an uncached one — the same contract
        // `rerank_maxsim` above is bound by.
        let mut ranked = ranked;
        let _shares = trace.time(Stage::RankDeclBoost, || {
            super::apply_structural_boost(&mut ranked, query, opts, |id| {
                let chunk = pass.chunks[id as usize];
                let fm = &files[chunk.file_id as usize];
                let text = corpus::lines(root, &fm.path, &chunk);
                (fm.path.clone(), text)
            })
        });

        let ranked = super::append_bm25_pins(ranked, &bm25_head, opts);
        per_phrase.push(trace.time(Stage::Candidates, || {
            // A file scope takes every chunk; see `file_scope_candidate_width`.
            // Only reachable here — a file scope never resolves an index, so
            // the warm path cannot be asked the same question.
            let width = if opts.file_scope_window > 0 && root.is_file() {
                super::file_scope_candidate_width()
            } else {
                super::candidate_width(opts.k)
            };
            candidates(ranked, &pass.chunks, &files, width, &bm25_head, opts.bm25_pin)
        }));
    }
    let cands = if q.is_multi() {
        super::merge_interleave(per_phrase)
    } else {
        per_phrase.pop().expect("one phrase")
    };
    // No embedding matrix here, so candidate vectors are recomputed on demand.
    // At most a few thousand texts, which is nothing beside the pass just done —
    // but it lands in `finalize:vectors`, which is therefore a much larger
    // number cold than warm under the same name.
    let fin = hit::finalize(root, q, cands, opts, "", &mut trace, |c| {
        let fm = &files[c.chunk.file_id as usize];
        let text = corpus::lines(root, &fm.path, &c.chunk)?;
        let doc = corpus::doc_text(&fm.path, &text);
        let embedded =
            text::embed_query(&text::prose_render_doc(&doc, opts.embed_preproc, opts.path_render));
        // Through the index's quantization, so diversity reranking sees the
        // same vectors it would warm. Without this the two paths diversify
        // differently and a cached scope answers a query differently from an
        // uncached one.
        Some(rank::as_stored(&embedded))
    });

    Ok(SearchResult {
        report: SearchReport {
            used_index: false,
            n_chunks_considered: pass.chunks.len(),
            files_walked: files.len(),
            floored: fin.floored,
            best_signal: fin.best_signal,
            n_phrases: q.phrases.len(),
            phrase_signals: fin.phrase_signals,
            phrases: q.is_multi().then(|| q.phrases.clone()),
            floored_mask: fin.floored_mask,
            bridge_terms: (!bridge_terms_used.is_empty()).then_some(bridge_terms_used),
            stages: trace.finish(),
            ..Default::default()
        },
        hits: fin.hits,
    })
}

/// What one streaming pass retained: the chunk table, and whichever engines this
/// mode asked for.
struct Pass {
    chunks: Vec<Chunk>,
    bm25: Option<Bm25Index>,
    /// The k nearest chunks seen so far, one heap per phrase. Heaps, not a
    /// matrix — this is what keeps a cold semantic search's memory
    /// independent of corpus size, and per-phrase because each phrase is its
    /// own retrieval (RESEARCH.md §31).
    nearest: Option<Vec<TopK>>,
}

/// Read, chunk, tokenize, and embed the corpus, keeping only the query's answer.
fn corpus_pass(
    root: &Path,
    files: &[FileMeta],
    phrases: &[String],
    opts: &SearchOptions,
    pool: usize,
    trace: &mut Trace,
) -> Pass {
    let want_bm25 = super::wants_lexical(opts);
    let want_sem = matches!(opts.mode, Mode::Semantic | Mode::Hybrid);

    let mut chunks: Vec<Chunk> = Vec::new();
    let mut bm25 = want_bm25.then(Bm25Index::new);
    // Timed, and under the same name the warm path uses. It sat outside every
    // span before, which is how a whole `ese::encode` call went unattributed on
    // a path whose entire job is embedding.
    let mut embedder = trace.time(Stage::RankEmbedQuery, || {
        want_sem.then(|| {
            let qs: Vec<[f32; crate::EMBED_DIM]> = phrases
                .iter()
                .map(|p| text::embed_query(&text::prose_render_query(p, opts.embed_preproc)))
                .collect();
            Embedder::new(qs, pool, opts.embed_preproc, opts.path_render)
        })
    });

    let t_pass = Instant::now();
    corpus::pass(root, files, &opts.params, want_sem, want_bm25, |_, work| {
        for (chunk, doc, tokens) in work.docs {
            // Lockstep: the chunk id is this chunk's position in the table, and
            // the BM25 document and the embedding are queued under the same id.
            let chunk_id = chunks.len() as u32;
            chunks.push(chunk);
            if let (Some(b), Some(t)) = (&mut bm25, tokens) {
                b.add_tokenized(t);
            }
            if let (Some(e), Some(doc)) = (&mut embedder, doc) {
                e.push(chunk_id, doc);
            }
        }
    });
    let (nearest, embed_ms) = match embedder {
        Some(e) => {
            let (top, ms) = e.finish();
            (Some(top), ms)
        }
        None => (None, 0.0),
    };
    // Embedding is separated from reading and tokenizing because they scale
    // differently: one is CPU-bound in ese, the other IO-bound in the walk.
    let total = elapsed_ms(t_pass);
    trace.record(Stage::PassEmbed, embed_ms);
    trace.record(Stage::PassRead, (total - embed_ms).max(0.0));

    Pass { chunks, bm25, nearest }
}

/// Batches chunk texts, embeds them, and keeps the k nearest to the query.
///
/// Scoring goes through the same quantization the index uses, rather than
/// comparing f32 vectors directly. That is what makes a cold answer equal a warm
/// one: the warm path scores `dot_distance_i8` over the i8 rows in `emb.bin`, so
/// a cold path scoring full-precision cosine would rank near-ties differently and
/// "the index is a cache" would be false for semantic search. Quantizing costs a
/// multiply and a round per dimension, against an embedding per chunk.
struct Embedder {
    /// One quantized query per phrase; each batch row is dotted against all
    /// of them — an extra 256-wide dot per phrase against a file read the
    /// pass already paid for (RESEARCH.md §31).
    queries: Vec<Vec<i8>>,
    pending: Vec<(u32, String)>,
    nearest: Vec<TopK>,
    embed_ms: f64,
    /// Same rendering the build side applies, or cold and warm diverge.
    preproc: text::EmbedPreproc,
    path_render: text::PathRender,
}

impl Embedder {
    /// Texts per batch. `ese` parallelizes internally above 16, and batching is
    /// what bounds resident text regardless of corpus size.
    const BATCH: usize = 1024;

    fn new(
        queries: Vec<[f32; crate::EMBED_DIM]>,
        k: usize,
        preproc: text::EmbedPreproc,
        path_render: text::PathRender,
    ) -> Self {
        let n = queries.len();
        Self {
            queries: queries
                .into_iter()
                .map(|mut q| {
                    rank::normalize(&mut q);
                    rank::quantize_i8(&q)
                })
                .collect(),
            pending: Vec::with_capacity(Self::BATCH),
            nearest: (0..n).map(|_| TopK::new(k)).collect(),
            path_render,
            embed_ms: 0.0,
            preproc,
        }
    }

    fn push(&mut self, chunk_id: u32, doc: String) {
        self.pending.push((chunk_id, doc));
        if self.pending.len() >= Self::BATCH {
            self.flush();
        }
    }

    /// The heaps, and the milliseconds spent embedding.
    fn finish(mut self) -> (Vec<TopK>, f64) {
        self.flush();
        (self.nearest, self.embed_ms)
    }

    fn flush(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let start = Instant::now();
        let mut vecs =
            ese::encode(
            self.pending.iter().map(|(_, t)| {
                text::prose_render_doc(t, self.preproc, self.path_render)
            }),
        );
        for ((id, _), v) in self.pending.iter().zip(vecs.iter_mut()) {
            // Exactly what store::build writes into emb.bin, so the two paths
            // score the same numbers.
            rank::normalize(v);
            let v_i8 = rank::quantize_i8(v);
            for (q, heap) in self.queries.iter().zip(self.nearest.iter_mut()) {
                heap.push(*id, rank::dot_distance_i8(q, &v_i8));
            }
        }
        self.embed_ms += elapsed_ms(start);
        self.pending.clear();
    }
}

/// Fused ids into candidates. No scope filter: the cold path walks exactly the
/// queried subtree, so everything it ranked is in scope by construction.
fn candidates(
    ranked: Vec<(u32, f32)>,
    chunks: &[Chunk],
    files: &[FileMeta],
    limit: usize,
    bm25_head: &[u32],
    pin: usize,
) -> Vec<hit::Candidate> {
    // Same pinned-ride-along rule as `indexed::candidates`: ids the
    // `bm25_pin` guarantee appended past the width cut must survive it.
    let pinned: std::collections::HashSet<u32> =
        bm25_head.iter().take(pin).copied().collect();
    let mut out: Vec<hit::Candidate> = Vec::with_capacity(limit.min(ranked.len()));
    for (id, score) in ranked {
        if out.len() >= limit {
            if pinned.is_empty() {
                break;
            }
            if !pinned.contains(&id) {
                continue;
            }
        }
        let chunk = chunks[id as usize];
        out.push(hit::Candidate {
            id,
            chunk,
            path: files[chunk.file_id as usize].path.clone(),
            score,
            phrases: 1,
            fine: None,
            bm25_rank: bm25_head.iter().position(|&h| h == id).map(|i| i as u16 + 1),
        });
    }
    out
}

/// MaxSim rerank for the cold path, mirroring `indexed::rerank_maxsim`.
///
/// Both paths must apply it, and apply it at the same point (before fusion),
/// or the same query answers differently depending on whether a scope happens
/// to be indexed — which is exactly what `cold_and_warm_return_identical_results`
/// forbids. The chunk text is re-read here rather than held from the pass: the
/// streaming pass deliberately keeps only the answer, not the corpus.
fn rerank_maxsim(
    root: &Path,
    files: &[FileMeta],
    chunks: &[Chunk],
    query: &str,
    ranked: Vec<(u32, f32)>,
    opts: &SearchOptions,
    trace: &mut Trace,
) -> Vec<(u32, f32)> {
    if !opts.rerank_maxsim || ranked.is_empty() {
        return ranked;
    }
    // No SIF stats on the cold path, matching a default-built index.
    let query_tokens =
        text::token_vectors(&text::prose_render_query(query, opts.embed_preproc), None);
    if query_tokens.is_empty() {
        return ranked;
    }
    trace.time(Stage::RankMaxsim, || {
        use rayon::prelude::*;
        let head = rank::maxsim_head_size(ranked.len(), opts.k, opts.maxsim_pool);
        let scored: Vec<(u32, f32, f32)> = ranked[..head]
            .par_iter()
            .map(|&(id, dist)| {
                let chunk = &chunks[id as usize];
                let fm = &files[chunk.file_id as usize];
                let sim = corpus::lines(root, &fm.path, chunk)
                    .map(|text| {
                        let raw = corpus::doc_text(&fm.path, &text);
                        let doc = text::token_vectors(
                            &text::prose_render_doc(
                                &raw,
                                opts.embed_preproc,
                                opts.path_render,
                            ),
                            None,
                        );
                        rank::maxsim(&query_tokens, &doc)
                    })
                    .unwrap_or(f32::NEG_INFINITY);
                (id, sim, dist)
            })
            .collect();
        let mut out = rank::maxsim_blend_head(&scored, opts.maxsim_blend);
        out.extend_from_slice(&ranked[head..]);
        out
    })
}
