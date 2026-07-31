# Retrieval evaluation: setup, candidates, results

*Generated 2026-07-31; semantic column re-measured after MaxSim became that
mode's default (§13.10). Numbers here come from `eval/results/`, which holds the
per-query ranks that produced them. Regenerate the index with
`python3 eval/results.py`.*

---

## 1. The question

An agent searching a codebase has one job: get to the right code in as few
round-trips as possible. Today it does that with `ripgrep`, which means every
natural-language intent is compressed into a regex guess before the tool sees
it. semgrep takes the intent verbatim and ranks.

So the question this eval answers is narrow and testable:

> **Given the same intent, how often does the target appear in the top 5?**

Not "is semantic search good." Not "is the embedding model strong." One
number, per condition, per corpus, with the per-query ranks kept so any two
conditions can be compared statistically later.

---

## 2. Setup

### 2.1 Corpora — seven, spanning 166 to 84,225 files

| corpus | language | files | source | pinned |
|---|---|---|---|---|
| linux | C | 84,225 | 1,147 MB | v6.9 |
| vscode | TS/JS | 4,389 | 64 MB | SHA (since 2026-07-30) |
| wikipedia | English prose | 1,008 | 262 MB | **cannot be** — dumps expire |
| cosqa | Python (one fn/file) | 20,604 | 5.7 MB | HF dataset |
| etcd | Go | 1,110 | 15 MB | SHA |
| commons-lang | Java | 625 | 10 MB | SHA |
| tokio | Rust | 790 | 6 MB | SHA |
| jekyll | Ruby | 166 | 3 MB | SHA |

The four small ones were added because `eval/symbols.py` supports
python/js/ts/rust/go/c/java/ruby and only C and TS were ever exercised — the
Go, Java, Ruby and Rust extractors were tested against hand-written fixtures
alone. They also sit in the **<2k-file band where RESEARCH.md §9.7 found engine
variants actually diverge**; the original three are 84k, 4k and 1k.

Every tree carries a content digest in `bench/corpora/MANIFEST.json`, so a
corpus that changed is detectable (`python3 bench/manifest.py --check`) whether
or not it could be pinned.

### 2.2 Query sets — and which to trust

| set | n | who wrote the queries | trust |
|---|---|---|---|
| `cosqa-1200` | 1,200 | **real humans** (Bing logs) | **highest** |
| `replay-agent` | 497 | **real agents** (harvested from run logs) | **highest** |
| `tokio/commons-lang/etcd/jekyll` | 1,374 | `claude`, symbol-anchored | medium |
| `linux/vscode/wikipedia` | 1,198 | `claude`, window-anchored | historical |

All are checked into `eval/queries/` with a manifest. They used to be
gitignored, which meant no published number was reproducible from the repo.

**The two at the top are the only sets nobody on this project wrote**, and they
are the ones any quality claim should rest on. Everything else we authored, and
authored sets leak:

| set | kind | contains gold identifier | median words |
|---|---|---|---|
| ours, `direct` | | **66–70%** | 10 |
| ours, `paraphrase` | | 2% | 17 |
| CoSQA | real humans | **0%** | 6 |
| replay | real agents | 47% | **4** |

`direct` hands the tool the answer's name. `paraphrase` strips vocabulary a
user would actually type and runs 17 words where humans type 6 and agents type
4. **Neither pole is where anyone is** — they bracket reality rather than
represent it.

### 2.3 Guards that run beside the numbers

Each exists because of a failure that produced a *plausible wrong answer*
rather than an error.

| guard | what it catches |
|---|---|
| `eval/leakage.py` | how much of the answer the query already contains — printed above every table |
| `eval/validate_queries.py` | gold spans that drifted from the corpus; run refuses rather than scoring 0 |
| `bench/manifest.py --check` | a corpus tree that changed under a comparison |
| index-freshness check | semgrep modes scored against an index older than the binary |
| `--checkpoint` | a multi-hour run losing everything to an interruption |

---

## 3. The candidates

Six conditions, all scored on the same queries, same corpus, same k.

| condition | what it does | is it a baseline? |
|---|---|---|
| **`bm25`** | rarity-weighted lexical ranking over code-aware tokens | semgrep |
| **`semantic`** | 256-dim static embeddings, cosine, MaxSim-reranked (default since §13.10) | semgrep |
| **`hybrid`** | bm25 + semantic fused (the shipped default) | semgrep |
| **`rg`** | legacy baseline: tokenizer excludes `_`, picks 2 longest words | **weak — kept only for comparability** |
| **`rg-strong`** | what a competent agent does: grep the identifiers first | **the fair baseline** |
| **`rg-oracle`** | tries *every* query token, keeps whichever scored best | **a ceiling, not a baseline** |

### Why `rg-oracle` exists, and what it does *not* bound

RESEARCH.md §12.1 found the original `rg` baseline was a strawman: its
tokenizer excluded `_`, so `blkg_rwstat_add` was shredded before ripgrep saw
it. Fixing that (`rg-strong`) collapsed a published "30× gap" to ~2.9×. The
obvious follow-up question is whether `rg-strong` is *also* leaving performance
on the table — it is still a hand-tuned heuristic.

`rg-oracle` removes the guesswork entirely. It consults the answer, tries every
content token as its own pattern, and keeps the best rank any achieved. **No
agent can run it.** It is the most ripgrep could possibly do.

> **It bounds ripgrep's query *formulation*. It does not bound ripgrep's
> absent *ranking*.** That is why it can score below bm25 — see §5.2, which
> makes the mechanism visible.

---

## 4. Worked example

One gold span, two ways of asking for it. Corpus: linux kernel. Gold:
`block/blk-cgroup-rwstat.h:42-71` — the `blkg_rwstat_add` helper.

### 4.1 `direct` — the query names the function

```
blkg_rwstat_add inline function choosing percpu counter for discard write read
```

| condition | top 5 | |
|---|---|---|
| **bm25** | `blk-cgroup-rwstat.h:49` ← **GOLD**, `…h:121`, `…c:25`, `…h:1`, `bfq-cgroup.c:25` | **rank 1** |
| **hybrid** | `blk-cgroup-rwstat.h:49` ← **GOLD**, `…c:25`, `…h:97`, `…h:1`, `…c:121` | **rank 1** |
| **rg-strong** | `bfq-cgroup.c:225`, `:233`, `:238`, `rwstat.h:53` ← **GOLD**, `:61` | rank 4 |
| **rg-oracle** | *identical to rg-strong* | rank 4 |
| **rg** (legacy) | `i915_reg.h:58`, `topro.c:4094` | **miss** |

Three things to read here:

- **The legacy baseline isn't just worse, it's answering a different
  question.** It grepped for `choosing`/`function` — the two longest words —
  and returned a GPU register header and a USB camera driver.
- **`rg-strong` does the sensible thing** and greps `blkg_rwstat_add` first. It
  still lands at rank 4, because `bfq-cgroup.c` sorts before
  `blk-cgroup-rwstat.h` and ripgrep returns path order.
- **`rg-oracle` finds nothing better.** On a query containing a rare
  identifier, "grep the identifier" is already near-optimal. This is why the
  kernel ceiling is only 1.4× the heuristic.

### 4.2 `paraphrase` — same target, vocabulary stripped

```
helper that increments the right per-cpu block cgroup statistic based on
request operation type
```

| condition | top 5 | |
|---|---|---|
| **bm25** | `bpf.h:4321`, `bpf.h:4321`, `bpf.h:3625`, … | miss |
| **hybrid** | `bpf.h:4321`, `bpf.h:4321`, `bpf.h:3625`, … | miss |
| **rg-strong** | `kernel/sched/fair.c:2600` | miss |
| **rg-oracle** | **(nothing — no token in the query appears in the gold span)** | miss |

This single line is the mechanism behind the most striking number in the whole
eval. The oracle is allowed to read the answer and try every token, and it
**returns nothing at all**, because there is no shared token to try. Grep skill
is irrelevant when the query and the target share no vocabulary.

Note that **semgrep misses this one too.** The paraphrase stratum is hard for
everything; semgrep gets 4% of them and ripgrep gets 0%.

### 4.3 Where ranking beats token choice (CoSQA)

```
python print all environment variables join     →  gold: d7408.py
```

```python
def show():
    """Show (print out) current environment variables."""
    env = get_environment()
    for key, val in sorted(env.env.items(), key=lambda item: item[0]):
        click.secho('%s = %s' % (key, val))
```

The oracle picks the best token available and still misses:

| token | in gold? | files matched | gold's rank |
|---|---|---|---|
| `environment` | yes | 131 | **position 112 of 131** |
| `variables` | yes | 98 | not in top 10 |
| `print` | yes | 879 | not in top 10 |

ripgrep returns those 131 files in **path order** —
`d10218.py, d10350.py, d10454.py, …` — and gold sits at position 112. There is
nothing ripgrep can do about that; it has no notion of which of the 131 is most
relevant.

bm25 does: it weights by term rarity, counts how many of the six query terms
co-occur, and normalizes by document length. The gold is 7 lines carrying three
of them densely.

```
bm25 top 5:  d11479.py, d20352.py, d7408.py ← GOLD, d17645.py, d15517.py
```

---

## 5. Results

### 5.1 Recall@5 across all seven corpora

**`direct` / identifier-style queries:**

| corpus | rg | rg-strong | **rg-oracle** | semantic | bm25 | hybrid |
|---|---|---|---|---|---|---|
| linux | 0.025 | 0.342 | **0.462** | 0.739 | **0.920** | 0.899 |
| vscode | 0.155 | 0.360 | — | 0.710 | 0.880 | 0.870 |
| jekyll | 0.034 | 0.057 | **0.205** | 0.716 | 0.864 | **0.886** |
| commons-lang | 0.070 | 0.106 | **0.236** | 0.583 | 0.849 | **0.864** |
| tokio | 0.065 | 0.085 | **0.190** | 0.530 | **0.710** | 0.700 |
| etcd | 0.090 | 0.090 | **0.165** | 0.420 | **0.705** | 0.695 |

**`paraphrase` / vocabulary-stripped queries:**

| corpus | rg-strong | **rg-oracle** | semantic | bm25 | hybrid |
|---|---|---|---|---|---|
| linux | 0.000 | **0.000** | 0.010 | 0.035 | 0.040 |
| jekyll | 0.000 | **0.068** | 0.102 | 0.136 | 0.182 |
| commons-lang | 0.015 | **0.035** | 0.121 | 0.146 | 0.171 |
| tokio | 0.010 | **0.050** | 0.045 | 0.090 | 0.085 |
| etcd | 0.000 | **0.030** | 0.060 | 0.065 | 0.065 |

**Real human queries (CoSQA, 1,200):**

| rg | rg-strong | **rg-oracle** | semantic | hybrid | bm25 |
|---|---|---|---|---|---|
| 0.030 | 0.030 | **0.101** | 0.108 | 0.208 | **0.222** |

**Real agent queries (replay, 497, hit@5 on first gold file):**

| semantic | bm25 | hybrid |
|---|---|---|
| 0.461 | 0.473 | **0.493** |

### 5.2 What the ceiling changes

| claim | vs `rg-strong` | **vs the ceiling** |
|---|---|---|
| kernel, identifier queries | 2.7× | **2.0×** |
| CoSQA, real human queries | 7.4× | **2.2×** |

**This is the headline correction.** RESEARCH.md §12.3 reported the real-query
advantage as 8.3×. Against a ripgrep permitted to read the answer first it is
**2.2×**. Most of that gap was query planning, not retrieval.

That is the *second* time this has happened — §12.2 cut "30×" to 2.9× by fixing
the baseline's tokenizer. **The direction of the claim survived both
corrections; the magnitude has now been wrong twice, in the same direction, for
the same reason.** Future gaps get quoted against the ceiling.

### 5.3 Why it matters

**Ranked lexical retrieval is the product.** bm25 alone carries nearly all of
it: 0.222 vs hybrid's 0.208 on real queries, and bm25 ≥ hybrid on three of the
four new corpora. The semantic half does not earn its keep on code.

**An oracle-grep and our semantic mode are level** on real queries — 0.101 vs
0.108, 121 against 130 of 1,200, CI [-0.013,+0.029], inconclusive.

> **Retracted 2026-07-31.** This section previously read "an oracle-grep *beats*
> our semantic mode, 0.101 vs 0.083." That was measured before MaxSim became
> the default for `--mode semantic`, which lifted CoSQA semantic to 0.108
> (+0.026, CI [+0.006,+0.046], p=0.011). The comparison is now a tie, and the
> stronger claim is withdrawn. It was caught by a reader asking whether the new
> reranker was in these numbers — not by the harness, which has no way to know
> a doc is quoting a figure the binary no longer produces.

The underlying point still holds and is worth keeping: semantic-only (0.108) is
less than half of bm25 alone (0.222), and §9.9 measured why — on code the static
embedding functions as a fuzzy lexical matcher (`def~function` 0.037,
`mutex~lock` 0.045) rather than a semantic model.

**The paraphrase asymmetry is the strongest evidence in the eval.** `rg-oracle`
scores **exactly 0.000** on all 199 kernel paraphrase queries. Not 0.005. A
ripgrep that inspects the answer and tries every token cannot locate one of 199
targets once the query stops naming them; semgrep finds 4%.

§12.2 argued this asymmetry showed a real capability difference rather than an
artifact — "improving the opponent closes the gap exactly where theory says it
should, and nowhere else." That was against a *heuristic* opponent. It now
holds against a *perfect* one, which is the strongest form the argument can
take.

The corollary belongs in the same breath: **4% versus 0% is a real difference
and it is still 4%.** The paraphrase wall stands.

---

## 6. What we got wrong

Four measurements in this campaign first looked like findings and were
instrument faults. All four were caught by comparing against something
*external* — a reference implementation, a repeat run, file mtimes — and none
would have been caught by the harness checking itself.

| looked like | actually was | how found |
|---|---|---|
| the ceiling losing to what it bounds (53/1374) | single-token vocabulary can't bound a conjunctive one | property check on real corpora |
| 12 residual violations | **ripgrep's output order was never deterministic** | 6 repeat runs, 2 orderings |
| kernel semgrep numbers drifting | **not** the stale index — that changed nothing | rebuilt and rescored: 1,194/1,194 ranks identical |
| a fast-rg rewrite agreeing on every spot-check | rg sorts path *components*; `tokio/` before `tokio-macros/` | full comparison, 3 of 27 pairs differed |

The nondeterminism one matters most: **every `rg` figure this harness ever
produced carried thread-scheduling variance** (measured spread 0.0067). Small
enough to overturn no conclusion, large enough to matter against §11.5's target
of resolving 3pp effects.

**A pre-registered prediction also missed.** §13.4 predicted the kernel ceiling
at 0.60–0.80; it came in at **0.462**, having overestimated ripgrep. Recorded
as a miss rather than reframed.

---

## 7. Limitations

- **wikipedia cannot be symbol-anchored** — it is prose, 0 of 1,013 files are
  parseable — so §11.4's chunking-neutrality caveat still applies to it.
- **The `linux`/`vscode`/`wikipedia` sets are window-anchored**, so they cannot
  referee a chunking change: their ground truth is one of the strategies under
  test. The four language sets are symbol-anchored and can.
- **The kernel semgrep column does not reproduce §12.2 exactly** (−0.021
  direct) and the cause is open. Index staleness was tested and ruled out.
- **CoSQA scores one gold function out of 20,604**, so 0.222 is a floor, not an
  accuracy.
- **92 historical runs carry no provenance.** They are listed as such in
  `eval/results/INDEX.md` rather than mixed in.

---

## 8. Reproducing

```sh
bench/fetch-corpora.sh                    # pinned corpora + tree digests
python3 bench/manifest.py --check         # confirm no tree moved

python3 eval/run_eval.py eval/queries/cosqa-1200.jsonl eval/data/cosqa/corpus \
    --modes bm25,semantic,hybrid,rg,rg-strong,rg-oracle \
    --checkpoint /tmp/ck.jsonl --out eval/results/oracle-cosqa.json

python3 eval/results.py                   # regenerate INDEX.md
python3 -m pytest eval/tests -q           # 174 tests over the scorers
```

Long runs are resumable — pass `--checkpoint`. A kernel oracle run is hours;
`--sort`-free deterministic rg made it 3.6× cheaper, and checkpointing is what
made it finishable at all.

---

## 9. Changelog

**2026-07-31 — MaxSim became the default for `--mode semantic`.** The semantic
column above was re-measured; every other column is unchanged, which is the
point (RESEARCH.md §13.10):

| corpus | semantic before | after | hybrid |
|---|---|---|---|
| linux | 0.668 | **0.739** | unchanged |
| vscode | 0.560 | **0.710** | unchanged |
| jekyll | 0.636 | **0.716** | unchanged |
| commons-lang | 0.492 | **0.583** | unchanged |
| tokio | 0.420 | **0.530** | unchanged |
| etcd | 0.340 | **0.420** | unchanged |
| cosqa | 0.083 | **0.108** | unchanged |

Reranking the semantic list is worth +0.07 to +0.15 R@5. Fused hybrid does not
move at all, because BM25 carries it — which is why MaxSim is on for semantic
and off for hybrid rather than on everywhere.

Rerank head is 96, the value three separate sweeps have now agreed on (§9.6,
32-vs-96, 42-vs-96). The gain concentrates in the `paraphrase` stratum both
times it was isolated: a deep head matters where the ranked list is weakest.
