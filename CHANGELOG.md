# Changelog

Findings and performance improvements, newest first. Measured numbers are
medians on an M-series Mac; "kernel" = Linux 6.9 source (1.15 GB, 1.51M
chunks). Full data: `RESULTS.md`, `bench/results/`, `eval/data/`.

## 2026-08-01 — embed preprocessing: semantic recovers 85% of bm25 on real queries

The §14.3 campaign (5 corpora, 2,798 queries, paired stats): rendering code
as prose before embedding (`--embed-preproc split`) plus SIF pooling takes
CoSQA semantic R@5 from 0.108 to **0.188** (CI [+0.060, +0.099], 133w/37l)
against bm25's 0.222 — the gap falls from 2.7× (§13.8) to 1.18×. Two facets
of one failure: rendering fixes the token *units* and pays on camelCase
corpora (vscode direct +0.115, etcd +0.175); SIF fixes the *weights* and
pays on real snake_case queries (+0.062) — and §9.4's rejection of SIF turns
out to have been an artifact of synthetic query sets. Kernel paraphrase wall
stands (0.035 at best) but semantic now ties bm25 behind it. Not yet the
default build: gated on query replay (§14.5), per the twice-learned §9.7
lesson. Full record: RESEARCH.md §14.4.

## 2026-08-01 — semantic-first: the default mode is now `semantic`

Maintainer decision, recorded with its cost in RESEARCH.md §14: the success
criterion for this project is semantic search beating lexical on real queries,
and a hybrid default whose fused result is 97% BM25 (§13.10) was hiding the
component under test. `--mode hybrid` remains, tuned as before; the exact-miss
suggestion path follows the default. Known cost today: CoSQA R@5 0.208 → 0.083
until the §14.2 preprocessing campaign and the model work close the gap —
that trade is the point, not an oversight. MaxSim reranking is on in semantic
mode, so it is now part of the default search.

## 2026-07-30 — reorganization: layers, and the defects it surfaced

Restructured `semgrep-core` into six layers with a one-way dependency rule
(`corpus` → `text` → `rank` → `store` → `cache` → `search`; `keyword` apart), and
split the CLI into flags / verbs / output. The two files that held 56% of the code
are gone: no engine file is over 265 lines and no function over 60. Tests went
41 → 101, plus 45 pytest cases for the harness scorers, which had none.

The reorganization was a means, not the point. Thirteen defects surfaced, nine of
them invisible to reading — they appeared only once a snapshot tripwire, a
property test, or a measurement was pointed at the code. Full ledger in
`FIXES.md`; the ones that changed behavior:

- **Cache entries ignored chunk parameters.** One search with a non-default
  `--window` wrote an entry that every later search of that scope was served
  from, silently returning spans of the wrong size. The eval harness sweeps
  `--window` against the same cache ordinary use has, so **every §9 lever number
  was measured through a contaminated cache** and deserves a re-run.
- **Cold and warm searches did not agree.** The cold path scored full-precision
  cosine over f32 embeddings; the warm path scored i8 dot products over the
  quantized matrix. 37 of 54 query/mode pairs differed. The parity test that
  existed compared the first hit only and could not see it. Cold now quantizes as
  the build does: **0 of 54 differ**, asserted over the full top-k.
- **The repair overlay was inconsistent with its own base** in the same way, and
  computed BM25 idf over its own handful of files rather than the corpus. Repair
  now matches a full rebuild on 147 of 147 comparisons for hit set, with two
  order divergences pinned by name.
- **BM25 scores were not reproducible.** Two `HashMap` iteration orders feeding
  non-associative f32 addition meant near-tied chunks swapped rank between runs
  of the same binary on the same corpus.
- **One unreadable file NaN'd an entire reranked head** (`--maxsim` only).
- Interrupted cache builds leaked entries that `cache --status` could not see and
  `cache --prune` could not free; index publication was not atomic; read-repair
  wrote into a committed `.semgrep/`.

Removed ~110 lines of unreachable f32 embedding code, dead since format v2, and
collapsed seven eval campaign scripts into two.

New guardrails: `tools/snapshot.sh` byte-compares ranked output over a frozen
fixture corpus (it caught the reproducibility bug); `crates/semgrep/tests/cli.rs`
covers the process contract, which had no tests; `tests/docs.rs` checks that
source comments citing `RESEARCH.md §N` still resolve.

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
