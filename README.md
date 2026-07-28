# semgrep

Semantic grep for agents: a grep-shaped search tool that ranks by relevance
instead of matching by regex. Named for its lineage with grep/ripgrep — the
incumbent agent search tools it benchmarks against. (Not affiliated with
r2c/Semgrep, the static-analysis tool.)

## Why

Coding agents search with ripgrep, which means every natural-language intent
("where is the retry backoff computed") must first be compressed into a regex
guess. When the guess misses — wrong identifier, wrong vocabulary — the agent
gets nothing back and burns a retry loop: another guess, another tool call,
more tokens, more latency. Measured on real query sets, that failure mode is
the norm, not the edge case: keyword-ized natural-language queries find the
target in the top results **3–27%** of the time; semgrep's ranked search finds
it **88–99%** of the time on the same intents (details below).

```
"where is the retry backoff computed?"
        │                                  │
        ▼                                  ▼
 agent compresses intent            semgrep, verbatim
 into a regex guess                        │
        │                                  ▼
 rg "retry.*backoff" ── 0 hits      1. net/backoff.c:41
        ▼                           2. client/retry.c:88
 rg "backoff" ──── 4,000 hits       3. ...
        ▼
 rg -i "delay" ─── noise ...        one call, ranked
        ▼
 (each miss = one full agent
  round-trip: tokens + latency)
```

The bet: for agents, the quality of the *first* search result matters more
than the raw speed of the scan, because every miss is a full agent round-trip
that never needed to happen. rg answers a miss in 1.7 s; semgrep answers a hit
in 135 ms.

## What it is

One verb, one escape hatch. Give it anything — an identifier, a phrase, a
question — and it returns the k most relevant locations as `path:line:text`
(a tuned BM25+embedding hybrid under the hood; no mode to pick). Ranked, not
exhaustive: if the answer isn't on the first page, rephrase. Use `-e` for
exact regex with grep semantics when you need every occurrence or proof of
absence.

```sh
semgrep "where is the retry backoff computed" src/   # ranked, works cold (no index)
semgrep index . && semgrep "retry backoff" .         # ranked, indexed: ~100 ms warm
semgrep -e 'fn \w+_config' .                         # exact regex (grep semantics)
semgrep --json -k 20 "..." docs/                     # JSONL for harnesses
```

Design choices aimed at agents specifically:

- **Grep-shaped contract.** `path:line:text` on stdout, exit 0 on hits / 1 on
  none — agents adopt it without new habits or new prompt scaffolding.
- **Every reply teaches the next move** (on stderr, so stdout stays parseable).
  Ranked results report `ranked top k of N candidates`; misses suggest
  rephrasing or `-e`; exact-mode floods truncate with the true total and a
  pointer back to ranked search. An exact-mode miss on an indexed corpus even
  prints the top-3 *ranked* hits for the same terms — a wrong identifier guess
  costs one call instead of two.

  ```
  stdout (grep-shaped, pipeable)     stderr (guidance, next move)
  ─────────────────────────────      ────────────────────────────────
  net/backoff.c:41:u32 delay = …     ranked top 10 of 1,514 candidates
  client/retry.c:88:if (retries…     miss? rephrase, or -e for exact
  ```
- **No index required.** Cold ranked search streams the corpus in one pass
  (~20 s on the 1.15 GB Linux kernel tree); an index makes it ~100 ms.

## How it works

Built on the Bog stack: [`ese`](../ese) (static 512-dim text embeddings,
compiled into the binary, CPU-only — what makes *unindexed* semantic search
feasible) and [`anny`](../anny) (HNSW ANN). One query fans out to two engines
over one shared chunk table, then fuses:

```
        corpus  (streamed cold, or .semgrep/ index warm)
                            │
              line-window chunks: 32 lines, 8 overlap
              ┌── 1 ─────────────── 32 ──┐
              │        ┌── 25 ─────────────── 56 ──┐
              │        │        ┌── 49 ──────────  …
                            │
            ┌───────────────┴───────────────┐
            ▼                               ▼
   BM25 (code-aware:              embeddings (ese 512-dim,
   camelCase/snake_case           i8-quantized, mmap'd)
   subtokens + full ids)                    │
            │                               │
         top-128                         top-128
            └───────────────┬───────────────┘
                            ▼
            weighted RRF  (semantic × 0.2)
                            │
              MMR  (spread across files)
                            ▼
                 path:line:text, top-k
```

- **One chunk table for everything.** BM25 and embeddings score the same
  chunks, so fusion and eval scoring are apples-to-apples and every result
  maps back to `file:line`.
- **Code-aware lexical ranking.** The BM25 tokenizer splits
  `camelCase`/`snake_case` into subtokens while keeping the whole identifier,
  and chunk text is path-augmented so file names count as evidence.
- **Weighted fusion + diversity.** Reciprocal-rank fusion of the BM25 and
  semantic lists (semantic down-weighted 0.2 — tuned, not guessed), then MMR
  so results spread across files instead of stacking in one.
- **An index format shaped by measurement.** `--stats` prints per-stage
  timing provenance; each v2 format decision closed a measured bottleneck:
  BM25 postings became a zero-deserialization mmap'd table (839 ms → 0.1 ms
  load), embeddings became i8-quantized unit vectors (the brute scan was
  page-fault-bound, so 4× fewer bytes took it from 2.9 s → 65 ms, quality
  verified neutral), and the HNSW graph is lazily skipped where it loses to
  the quantized brute scan.
- **Keyword mode is literally ripgrep's engine crates**, so exact search
  gives up nothing to the incumbent.

Full design in [DESIGN.md](DESIGN.md); the mode-collapse and CLI-surface
research in [RESEARCH.md](RESEARCH.md).

## Results

Measured 2026-07-27 on an M-series Mac across three corpora: Linux kernel 6.9
(1.15 GB text, 84k files — the canonical grep benchmark), VS Code (real agent
coding target), and a Simple English Wikipedia extract (prose). Full tables
and methodology in [RESULTS.md](RESULTS.md).

**Retrieval quality** — 400 LLM-generated queries per corpus, ground truth =
source chunk ±10 lines. "direct" queries name identifiers; "paraphrase"
queries deliberately avoid the chunk's vocabulary. "rg" is the agent-style
fallback (query keyword-ized into ripgrep, how agents use it today).

| Recall@5 | Kernel | VS Code | Wikipedia |
|---|---|---|---|
| semgrep, direct | **0.92** | **0.88** | **0.99** |
| rg, direct | 0.03 | 0.17 | 0.27 |
| semgrep, paraphrase | 0.05 | 0.14 | **0.41** |
| rg, paraphrase | 0.00 | 0.01 | 0.03 |

On the kernel that is a **30× gap** on identifier queries. Scoring is strict
single-truth (exact chunk, ±10 lines), so absolute numbers understate
usefulness — the cross-tool delta is the signal.

**Speed and cost** — semgrep gives up nothing to get there:

- Keyword mode ties ripgrep (1.72 s vs 1.86 s median on the kernel, ~12 MB
  RSS) and beats GNU grep ~8× and BSD grep ~25×.
- Warm indexed ranked queries on the kernel (1.51M chunks, end-to-end
  including process start): bm25 80 ms, semantic 80 ms, hybrid 135 ms.
  Smaller corpora: ≤ 20 ms.
- Index build: 59 s / 1.3 GB on disk for the kernel; 5 s / 78 MB for VS Code.
- Cold (no index): hybrid ~59 s on the kernel in one streaming pass — usable
  as a first resort, and the index removes the cost thereafter.

**Honest limits.** Paraphrased queries over *code* remain the open problem:
every engine scores ≤ 0.05 recall@5 on kernel paraphrase, and static
embeddings only clearly pay off on prose. That likely needs better code
embeddings or LLM query expansion, not more tuning of this stack. Next on the
roadmap is the end-to-end product claim: agent-task evals measuring
searches-to-success and tokens per task, rg-only vs semgrep.
