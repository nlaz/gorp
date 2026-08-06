# semgrep — benchmark & eval results

2026-07-27, M-series Mac. Corpora: Linux kernel 6.9 source (1.15 GB text,
84k files, 1.51M chunks), VS Code repo (49 MB text, 4k files), Simple English
Wikipedia extract (262 MB text). Competitors invoked by absolute path; stdout
written to real files (some tools short-circuit on /dev/null). Medians of 3
runs after warmup unless noted. Full per-cell data: `bench/results/`.

## 1. Keyword mode (grep semantics, all tools)

Median wall / peak RSS across 4 query shapes per corpus:

| Tool | Linux kernel | VS Code | Wikipedia |
|---|---|---|---|
| **semgrep** | **1.72s** / 12 MB | 0.05s / 8 MB | **0.02s** / 7 MB |
| ripgrep | 1.86s / 11 MB | 0.05s / 10 MB | 0.02s / 7 MB |
| ugrep | 3.43s / 12 MB | **0.04s** / 16 MB | 0.02s / 5 MB |
| GNU grep | 13.1s / 3 MB | 0.18s / 4 MB | 0.45s / 4 MB |
| ack | 16.1s / 20 MB | 0.33s / 26 MB | 0.74s / 23 MB |
| BSD grep | 44.2s / 511 MB | 2.08s / 118 MB | 3.77s / 177 MB |

semgrep ties ripgrep (same engine crates; by design). CPU utilization explains
the ladder: semgrep/rg ~1.9–7×, ugrep ~1×, GNU grep/ack single-threaded.

## 2. Ranked modes (index format v2: i8 embeddings + flat BM25)

Warm indexed queries, kernel corpus (worst case, 1.51M chunks), end-to-end
including process start and index load:

| Mode | v1 engine | v2 engine | Peak RSS (v2) |
|---|---|---|---|
| bm25 | 710 ms | **80 ms** | 70 MB |
| semantic | 4,060 ms | **80 ms** | 824 MB |
| hybrid | 4,040 ms | **135 ms** | 844 MB |

VS Code / Wikipedia warm queries: ≤ 20 ms in every mode.

Cold (no index, single streaming pass over the corpus), kernel: semantic
20.7s / 154 MB, bm25 42.7s / 916 MB, hybrid 59.2s. **Superseded:** these
predate the parallel corpus pass and index-as-cache (RESEARCH.md §8.2).
Re-measured 2026-07-28, hybrid + write-through: whole kernel 32s / 1.3 GB RSS,
kernel `drivers/net/` 3.9s, VS Code repo 2.5s — then 90ms / 20ms / 10ms warm. (Cold costs rose ~10%
vs v1: path-augmented chunk text adds tokens to embed/tokenize — the price
of the quality gains in §3.) The same NL intents
keyword-ized for the grep family ("nl fallback"): rg 1.7s, ugrep 3.0s,
GNU grep 11.6s, ack 13.9s, BSD grep 74.2s — fast, but see §3 for how often
those searches actually find the target.

Index build (kernel): 59s, 1.3 GB on disk (737 MB i8 embeddings, 515 MB flat
BM25). VS Code 5s / 78 MB; Wikipedia 16s / 239 MB.

### Provenance-driven optimization (what `--stats` found)

| Bottleneck | Cost found | Fix | After |
|---|---|---|---|
| `load:hnsw` — 3.1 GB graph deserialized every query, every mode | 20–35 s | lazy per-mode loading; skip graphs > 1 GiB in one-shot CLI | 0 ms |
| `load:bm25` — postcard→HashMap of 319 MB | 839 ms | `bm25.flat`: mmap'd sorted term table, zero deserialization | 0.1 ms |
| `rank:brute` — page-fault/IO bound (188k faults, 0.45× CPU), not compute | 2.9 s | i8-quantized unit vectors, 4× smaller matrix | 65 ms |

HNSW itself ranks well (496 ms warm at kernel scale) but only pays off in a
persistent process; it needs a zero-copy format before the CLI can use it at
scale. Quantization verified quality-neutral (§3, v2 column ≈ v1 within noise).

## 3. Retrieval quality (LLM-generated evals, 200 chunks / 400 queries per corpus)

> These are **generated** queries. §5 measured that they do not predict agent
> behaviour for a rendering or ranking change, in either direction. Read this
> board as a regression floor and a corpus-level comparison, not as grounds to
> accept or reject an engine change.

Ground truth = source chunk ±10 lines. "direct" queries name identifiers;
"paraphrase" queries deliberately avoid the chunk's vocabulary. "rg" is the
agent-style fallback (phrase → rarest-words AND → OR, case-insensitive).
Tuned = path-augmented chunk text + weighted RRF (sem_weight 0.2) + MMR.

**Recall@5:**

| Condition | Kernel | VS Code | Wikipedia |
|---|---|---|---|
| bm25 direct | **0.92** | **0.88** | **0.99** |
| hybrid direct | 0.90 | 0.86 | 0.97 |
| semantic direct | 0.68 | 0.57 | 0.79 |
| rg direct | 0.03 | 0.17 | 0.27 |
| bm25 paraphrase | 0.04 | 0.14 | 0.41 |
| hybrid paraphrase | 0.05 | 0.14 | **0.41** |
| semantic paraphrase | 0.01 | 0.01 | 0.24 |
| rg paraphrase | 0.00 | 0.01 | 0.03 |

Tuning gains (hybrid MRR@10): VS Code 0.595 → 0.753, Wikipedia 0.789 → 0.880.
Weighted fusion fixed the dilution problem — hybrid now ≈ bm25 on direct and
≥ any single engine on paraphrase (Wikipedia paraphrase R@10 0.52 vs bm25's
0.47). MMR diversity costs nothing on recall (ablation within noise) while
spreading results across files.

### Findings

1. **Ranked lexical search is the headline win.** BM25/hybrid find the target
   in top-5 88–99% of the time on identifier queries, vs 3–27% for the same
   intents keyword-ized into ripgrep. Every miss is an agent retry loop that
   never happens.

   > **Retracted, 2026-07-30:** this finding used to end "on the kernel it is
   > a 30× gap." That number came from a ripgrep baseline whose tokenizer
   > excluded `_`, so `blkg_rwstat_add` was shredded before ripgrep saw it.
   > Against a fair baseline (`rg-strong`) the kernel gap is **~2.9×**, and
   > fixing the tokenizer alone improves ripgrep 6.4× on the kernel. See
   > RESEARCH.md §12.1–§12.2. The 3–27% figures above are the legacy
   > baseline's and are kept only for comparability; read §12.2's table
   > instead.
2. **Speed of a miss isn't worth much.** rg answers in 1.7s but on paraphrase
   intents finds the target 0–3% of the time; semgrep's warm hybrid answers
   in 135 ms and does strictly better in every cell.
3. **Static embeddings struggle on paraphrased *code*.** Semantic mode helps
   on prose (0.24–0.41 paraphrase R@5 on Wikipedia) but adds little on C/TS
   beyond what path-augmented BM25 already catches. Kernel paraphrase (≤0.05
   for everything) is the open research problem — likely needs better code
   embeddings or LLM query expansion, not more tuning of this stack.
4. Scoring is strict single-truth (right file, ±10 lines); absolute numbers
   understate usefulness, cross-tool deltas are the signal.

## 4. The three boards (2026-08-02, RESEARCH.md §15–§16)

Results split three ways: the **guess board** (primary: real agent search
strings, §16), the **blind board** (model-experiment instrument, §15.10),
and the **named-identifier board** (regression floor).

**Agent-level A/B, the powered one (§16.10, 2026-08-03).** 1,115 agent runs,
556/560 Loc-Bench instances paired, semantic-default semgrep vs ripgrep as
the agent's only search tool: **parity**. Function-level localization 0.674
vs 0.673 (Δ +0.002, CI [−0.018, +0.022]); every secondary within ±0.01.
The headline is the bound: **any advantage is under 2.2pp**, at 27% higher
cost ($182.80 vs $143.69). What did move, 14×: one sentence of tool
description takes ranked-search usage from 7% to **98%** with no accuracy
consequence in either direction. And 80% of the benchmark is decided before
search matters (357 instances solved by both arms, 164 by neither) —
§11.5's instrument limit, confirmed at full scale.

**Guess board (primary).** Over 2,113 real guess-groups from agent logs:
one ranked hybrid query built from the agent's own guess beats the agent's
actual exact-mode workflow by +0.034 hit@5 (clustered CI [+0.002, +0.071]),
with the gap widening monotonically in ladder length (+0.030 → +0.053) —
ranked search pays exactly where the agent is guessing hardest. 19.6% of
multi-guess `-e` ladders were mechanically dead (`\|` is a literal pipe to
the engine). Rescue rate over failed exact guesses is 6.3% — real but far
below hopes; the supported claim is a better default posture, not a safety
net. Full scorecard: RESEARCH.md §16.5–§16.6.

**Blind board.** On *real-blind* queries — the 847 CoSQA human queries with
zero gold-identifier hits — champion semantic is at parity with bm25
(R@5 0.148 vs 0.169, CI spanning zero; MRR dead even). On *strict-blind*
generated sets (overlap ≈ 0.07, below anything real users emit) every engine
sits at 1–6% R@5 — semantic 0.028 vs bm25 0.024 pooled over 1,042 rows,
indistinguishable — and ripgrep collapses entirely (rg-strong ≤ 0.015,
oracle ≤ 0.025). Those sets are the instrument for the §9.9 code-teacher
model experiment, not a battleground for the current stack. Full tables and
the prediction scorecard: RESEARCH.md §15.6–§15.8.

### 4.1 The named-identifier board (regression floor; was "the semantic-first scoreboard")

The §14 standings, retained unchanged — every condition scored on the same
queries within a chart, ripgrep reported at all three levels per §13.4
(legacy, strong, and the oracle *ceiling*, which consults the answer and
which no agent can run).

**CoSQA — 1,200 real human queries (R@5 / R@10):**

| condition | R@5 | R@10 |
|---|---|---|
| ripgrep (≈ rg-strong here) | 0.030 | 0.051 |
| rg-oracle (ceiling) | 0.101 | 0.158 |
| semantic, raw index (old default) | 0.108 | 0.173 |
| **semantic, `--embed-preproc split --sif`** | **0.188** | **0.286** |
| hybrid (retired default) | 0.208 | 0.330 |
| bm25 (the bar) | 0.222 | 0.325 |

**VS Code `direct` — 200 identifier-bearing queries (R@5 / R@10):**

| condition | R@5 | R@10 |
|---|---|---|
| ripgrep (legacy) | 0.155 | 0.240 |
| rg-strong | 0.360 | 0.440 |
| rg-oracle (ceiling, first measured 2026-08-01) | 0.540 | 0.635 |
| semantic, raw index | 0.710 | 0.730 |
| **semantic, `--embed-preproc split --sif`** | **0.825** | **0.840** |
| hybrid | 0.870 | 0.885 |
| bm25 | 0.880 | 0.885 |

What moved and why (§14.4–§14.6): prose-rendering the embedded text fixes the
token *units* and pays on camelCase corpora; SIF pooling fixes the token
*weights* and pays on real snake_case queries; MaxSim's rerank contribution
grows on the rendered stream (+0.040 CoSQA R@5). The remaining gap to bm25 is
1.18× on CoSQA. Even the oracle-grade ripgrep sits below semantic mode in both
tables — token choice is not the hard part; ranking the files that contain the
token is. The §3 finding stands corrected in degree, not direction: ranked
lexical still leads, but the semantic branch now recovers 85% of it on real
queries, from 49%.

## 5. Chunk rendering, and what the offline harness is for (2026-08-05, §20–§23)

Four campaigns asked whether rewriting a chunk before embedding it helps.
The answer is no, and finding that out cost three instruments a demotion.

**The transfer failure (§21.2).** The same renderings, measured two ways:

| rendering | offline, generated queries | real agent queries |
|---|---|---|
| `prune-decl` | **−0.15 to −0.28, p<0.001 on all five corpora** | **−0.009 [−0.065, +0.039]** |
| `prune-kw` | +0.080 p=0.002 (linux, etcd) | −0.022 [−0.055, +0.012] |

Generated queries are 10–15 words and contain the gold file's own identifier
~70% of the time; real agent queries are ~5 words and do it 0.6% of the time.
**`run_eval.py` cannot referee a rendering or ranking change** — and the new
half is that offline *losses* fail to transfer too, so a negative offline
result is not grounds to reject a design either. Third confirmation of §9.7's
rule, first with the size of the miss measured.

**The powered bound (§23.2).** 7,657 real agent queries over 467 instances,
six description regimes, 62,808 rows, $0:

| arm | Δ recall@5 vs shipped | 95% cluster CI |
|---|---|---|
| `split` | −0.011 | [−0.022, −0.002] pooled; [−0.024, +0.000] on the clean half |
| `champion` (`split`+`sif`) | +0.005 | [−0.013, +0.023] |
| `prune-kw-pos` | −0.007 | [−0.021, +0.007] |

**No document-side rendering improves retrieval on real agent queries by more
than 0.023.** The §14.4 `split`+`sif` recommendation is retired — it is
indistinguishable from doing nothing, and the shipped `EmbedPreproc::None`
stands on a number rather than on absence of evidence.

**Two defects found and fixed on the way.** The keyword table deleted tokens
that are *identifier components* — `__init__`, `from_dict`, `as_completed` —
damaging **20.9% of the gold function names agents were hunting**; firing it
only on whole-word keywords cuts that to 0.7% (§22.1). And `guessplay` scored
file scopes at *file* level, where the answer is decided before ranking
begins, so 46% of real agent searches returned an exact `Δ = +0.000` for every
arm; scoring at *function* level recovered them and showed file scopes score
**0.272 against directory scopes' 0.152** (§22.2).

**Audited (§23.3).** Replay fidelity against the agents' own stored stdout is
**98.0%** where the tool worked. But 50.1% of the corpus was typed before
`b49e818`, when file-scoped ranked search returned nothing — point estimates
replicate across that split, significance does not, and §23.2 is amended
accordingly. `guessplay.py` is the gate an engine change must clear; it is
free and it replays real agent queries against real gold.

## 6. Within-file ranking, and the first engine win since §19 (2026-08-06, §24)

§5 closed document *rendering* with a powered bound. §24 aimed at document
*ordering* inside the file an agent already chose, and found both a measurement
error and a lever.

**The metric was hiding the effect size.** `rank_func` credits a hit only when
the chunk's best-matching *line* falls inside the gold function. Chunks are 32
lines; the median gold function is 12. Scored both ways over 2,149 reproduced
file-scoped agent searches:

| | @5 |
|---|---|
| strict (what §22 and §23 publish) | 52.9% |
| chunk overlap | 67.1% |
| **bracket** | **14.2pp** |

+19.8pp on gold functions under 10 lines, +6.8pp over 30 — chunk granularity,
not ranking. **14.2pp is larger than every effect §20–§23 tried to detect**, so
every file-scope number in those sections is a lower bound. Both metrics are now
emitted always, and a change that moves only one of them is a result about the
metric rather than the engine.

**Three candidates, measured factorially; one lived.**

| lever | strict@5 | overlap@5 | |
|---|---|---|---|
| same-file dedupe by overlap fraction | −0.003 [−0.011, +0.005] | −0.009 [−0.017, −0.000] | killed on its floor |
| finer chunk window at file scope | +0.008 [−0.013, +0.028] | −0.052 [−0.075, −0.030] | failed |
| **declaration boost** | **+0.027 [+0.006, +0.049]** | **+0.033 [+0.013, +0.052]** | **shipped** |

`--decl-boost` scales a chunk's fused score by the share of query tokens it
*declares* rather than merely mentions. Confirmed independently on the full
7,657-query corpus: **+0.039 strict / +0.048 overlap** on file scopes, **+0.017
bm25** on directory scopes — it gains where the tripwire only required it not to
lose. On by default at 0.5; costs 1.1–1.5 ms, flat in corpus size.

**This is the first engine change since §19 to beat an unrendered index on real
agent queries.** §20–§23 spent four sections on what a chunk is made of; this
changed what a chunk is worth.

**Both failures were argued from one vivid case, and floors written in advance
caught them.** The dedupe was sized by `--overlap 0`, which measured +2.0pp but
changes *chunking* — the proxy inverted the sign of the rule it stood in for.
The finer window was right about its mechanism (+4.8pp strict on gold functions
under 10 lines) and wrong about its value, and only the two-metric bracket could
tell those apart.

**What it does not claim.** Retrieval quality on replayed queries, not agent
behaviour (§11.5 stands). The recoverable pool — right file, gold function
outside the top 5 — is 9–13% of all agent searches and is a *ceiling*: an
unknown share of it is the agent asking a different question than the benchmark
grades.

## 7. Remaining roadmap

- **Bound the within-file ceiling** — label a sample of the recoverable pool for
  whether the query points at the gold function at all. One afternoon, and it
  decides how much of the remaining 9–13% is reachable by any ranker.
- **The 48% that aim at the wrong file** — nearly half of file-scoped agent
  searches name a file holding no gold function. No ranker reaches those; §19's
  tool-description instruments are the right tool.
- **Record provenance in the harvested corpus** — `guesses-*.jsonl` does not
  mark which rows a broken tool served, so any future campaign inherits §23.3's
  50% contamination silently. Rows should carry the serving binary or commit.
- **Validate `symbols.extract` spans** — both function-level metrics rest on a
  regex extractor that under-counts by design.
- Persistent server / MCP mode with resident index (amortizes load; makes
  HNSW worthwhile; sub-10 ms warm queries plausible).
- Two-pass streaming BM25 to cut the 916 MB cold-path RSS.
- fold-based incremental reindexing (watch mode).
