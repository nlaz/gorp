# Changelog

Findings and performance improvements, newest first. Measured numbers are
medians on an M-series Mac; "kernel" = Linux 6.9 source (1.15 GB, 1.51M
chunks). Full data: `RESULTS.md`, `bench/results/`, `eval/data/`.

## 2026-07-27 — index format v2: provenance-driven perf round

Added per-stage timing provenance to every query (`--stats` prints
`load:{meta,chunks,bm25,mmap,hnsw} rank:{bm25,embed-query,brute|ann,fuse}
finalize`). It attributed the warm path almost entirely to three costs, each
now fixed:

| Bottleneck | Was | Fix | Now |
|---|---|---|---|
| `load:hnsw`: 3.1 GB graph eagerly deserialized every query, all modes | 20–35 s | lazy per-mode component loading; one-shot CLI skips graphs > 1 GiB | 0 ms |
| `load:bm25`: postcard → HashMap of 319 MB | 839 ms | `bm25.flat`: mmap'd sorted term table binary-searched in place, zero deserialization | 0.1 ms |
| `rank:brute`: f32 scan was page-fault/IO bound (188k faults, 0.45× CPU) — not compute bound | 2.9 s | i8-quantized unit-normalized vectors; 4× smaller matrix stays cache-resident | 65 ms |

**Net (kernel, warm, end-to-end): bm25 710→80 ms, semantic 4,060→80 ms,
hybrid 4,040→135 ms.** BM25 query RSS 1.2 GB→70 MB. Index on disk 3.4→1.3 GB;
build 59 s. Small corpora warm ≤ 20 ms. Quantization verified quality-neutral
on the 400-query VS Code eval (MRR within 0.004 of f32).

Other findings this round:
- The brute-force "compute" optimization tried first (pre-normalized vectors
  + 1-FMA dot kernel) bought only ~5% — profile before optimizing: the scan
  was IO-bound, so bytes-on-disk was the real lever.
- anny HNSW ranks well warm (496 ms at kernel scale) but `from_bytes` on a
  3.1 GB graph costs ~20 s, so it loses to the quantized brute scan in a
  one-shot CLI. Needs a zero-copy/mmap graph format or a persistent server
  mode. HNSW build also peaked at 8.8 GB RSS.
- `--check-stale` split out of `--stats`: the staleness re-walk cost ~1 s per
  query on 84k files and was silently polluting totals.
- Cold streaming got ~10% slower (kernel hybrid 53→59 s) from path-augmented
  chunk text — accepted as the price of the quality gains below.

## 2026-07-27 — retrieval-quality tuning round

Evals: 200 LLM-generated chunks → 400 queries per corpus (direct +
paraphrase), ground truth ±10 lines, vs agent-style rg fallback.

Changes, each validated on the evals:
- **Path-augmented chunks**: the relative file path is prepended to each
  chunk before BM25 tokenization and embedding. Largest single quality win —
  e.g. VS Code hybrid R@1 0.49→0.63 before any fusion tuning.
- **Weighted RRF** (`--sem-weight`, default 0.2 after a 0.2–0.8 sweep):
  equal-weight fusion let the weaker semantic list dilute BM25 (hybrid R@5
  0.76 vs bm25's 0.86 on VS Code). At 0.2, hybrid ≈ bm25 on direct queries
  and ≥ any single engine on paraphrase (Wikipedia paraphrase R@10 0.52 vs
  bm25 0.47).
- **MMR diversity reranking** (`--mmr-lambda 0.75`, on by default) +
  same-file span-overlap dedupe: spreads top-k across different files or
  regions. Ablation: free on single-truth recall (paraphrase R@10 actually
  +0.02).

**Net hybrid MRR@10: VS Code 0.595→0.753, Wikipedia 0.789→0.880.**

Findings:
- Ranked lexical search is the headline: BM25/hybrid top-5 recall 88–99% on
  identifier queries vs 3–27% for the same intents keyword-ized into rg
  (kernel: 0.92 vs 0.03 — 30×).
- Static embeddings underperform on paraphrased *code* (kernel paraphrase
  ≤ 0.05 R@5 for every method; VS Code semantic-paraphrase 0.01) while fine
  on prose (0.24–0.41). Open problem: better code embeddings or LLM query
  expansion, not more fusion tuning.

## 2026-07-27 — v1: engine, CLI, benchmark + eval harnesses

- `semgrep-core` + `semgrep` CLI: keyword mode on ripgrep's engine crates
  (`grep-regex`/`grep-searcher`/`ignore`), chunk-based BM25 with a
  camelCase/snake_case-aware tokenizer, semantic mode on ese embeddings
  (anny HNSW optional), RRF hybrid. Indexed (`.semgrep/`) and unindexed
  (single streaming pass, top-k heap) paths; grep-compatible output + JSONL.
- Benchmark harness (`bench/`) vs BSD/GNU grep, ripgrep, ugrep, ack on
  kernel/VS Code/Wikipedia — wall, peak RSS, CPU via `/usr/bin/time -l`.
  Harness lessons: BSD/GNU grep need `-E` (BRE broke the regex cells), and
  ugrep detects `/dev/null` and short-circuits (0.00 s cells) — all tools now
  write to real files.
- Eval harness (`eval/`): parallel LLM query generation, recall@k/MRR
  scoring, agent-task protocol.
- v1 findings: semgrep-keyword ties rg (same engine; 1.72 vs 1.86 s kernel).
  Cold *semantic* search of the whole kernel needs no index at all — 20 s /
  154 MB (ese embeds 1.5M chunks on the fly) — while cold *BM25* was the
  memory hog (39 s / 916 MB of in-memory postings). BSD grep: 44 s / 511 MB.
