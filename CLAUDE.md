# semgrep (repo dir: semgrep/)

Semantic grep for agents built on the Bog stack: `../ese` (static embeddings,
256-dim, compiled-in weights) + `../anny` (HNSW). See DESIGN.md for the full
design, README.md for usage, AUDIT.md and FIXES.md for the 2026-07 reorganization
and what it found.

SIMULATION.md is the session-level behavior audit (what `eval/sim/` found and
what got fixed). FOLD.md evaluates `../fold` as a durable store for the repair
overlay — design, verified constraints, and what to measure before committing;
RCA-FJALL-LOCK.md is the one blocker, drafted for upstream.

## Layout

### Layers

`crates/semgrep-core` is organized in layers, bottom up. **A layer may call
downward and not upward.** That is the rule to check a change against, and it is
what keeps the tree navigable: `rank` never touches the filesystem, `store` never
ranks, `cache` never scores, `search` orchestrates rather than computes.

- `trace.rs` — stage timing. A leaf every layer may use, and the one module
  outside the stack: `Stage` is a closed enum, each path declares a
  `SCHEDULE_*`, and every stage belongs to exactly one `Bucket` so
  `walk`/`load`/`rank` are *derived* sums and `unattributed_ms` means "work
  nothing is timing". `crates/semgrep-core/tests/trace.rs` bounds that residual.
- `corpus/` — directory into files into chunks. `mod` walks, `chunk` cuts and
  re-reads, `pass` drives the parallel read, `diff` compares a tree against an
  index, `funcchunk` cuts on function boundaries (tree-sitter, `func-chunk`).
- `text/` — text into representations. `token` (code-aware tokenizer), `embed`,
  `sif` (rarity-weighted pooling, §9.1), `prose` (how text is rendered before
  embedding, §14.2/§20) over `prose_vocab` (the frozen keyword tables — data
  with a provenance: each was measured against a published arm, so editing one
  silently re-defines that arm).
- `rank/` — query plus representations into ordered ids. `bm25` (one scorer over
  a `Postings` trait), `vec` (kernels and quantization), `topk`, `fuse`, `mmr`,
  `prf`, `maxsim`, `bridge` (vocabulary-gap expansion, §33).
- `store/` — representations on disk. `build` (+ `build/embed`, `build/sif`),
  `load`, `bm25` (the flat mmap layout).
- `cache/` — which index answers, and keeping it honest. `mod` (discovery,
  fill), `compat` (generations), `budget`, `repair` (the read-repair overlay).
- `search/` — orchestration, and the tail that turns ids into hits. `indexed`
  (warm), `stream` (cold), `rows` (the union id space), `options`
  (`SearchOptions`, the type the CLI mirrors flag for flag), `query` (phrase
  splitting, §31), `hit` (candidate sequencing) over `rerank` (the fine
  rerank, §29.1) and `materialize` (span overlap, budget, best-line re-read),
  `unit` (the unit view's rows, §34), `checklist` (the learned blend, §35.2).
- `keyword.rs` — the exact-match escape hatch, independent of all of it.

`crates/semgrep` is the CLI, built as **two binaries over one source**: `sg`
(the name to type) and `semgrep` (kept because nine scripts and the test
harness resolve it by name). Only the name differs — env vars,
`~/.cache/semgrep`, `.semgrep/` and the `semgrep: ` stderr prefix are all
unchanged, so `sg` prints `semgrep:`. Deliberate: that is the expensive half
of a rename and it invalidates every built index (RESEARCH.md §19.9).
Its modules: `cli` (flags), `cmd/` (one file per verb), `out`
(every write to stdout or stderr), `telemetry` (the `SEMGREP_TRACE_FILE`
envelope — schema `semgrep.trace/1`, and the contract the eval harnesses
read). **stdout is data, stderr is commentary** —
`crates/semgrep/tests/cli.rs` enforces it.

Engine integration tests split by concern under `crates/semgrep-core/tests/`:
`e2e_general.rs` (the four modes and cold==warm), `e2e_cache.rs` (§8 cache
behaviors), `e2e_publish.rs` (publication is a swap), over a shared
`common/mod.rs`. Each is its own process and therefore its own cache dir.

### Harnesses

- `bench/` — perf harness vs grep/ggrep/rg/ugrep/ack (`fetch-corpora.sh`,
  `run.py`, `report.py`, `queries.json`); corpora + results are gitignored
- **Agentic-guess search is the primary regime since 2026-08-02** (RESEARCH.md
  §16): success = one ranked query built from a real agent's own guess lands a
  gold file in the top 5 more often than the agent's actual exact-mode
  workflow (clustered CI excluding zero), and hybrid must not trail bm25 on
  the guess corpora (`eval/queries/guesses-*.jsonl`, harvested from locbench
  shim logs — real agent queries, never written by us). Strict-blind (§15) is
  retained as the **model-experiment instrument** — the gate the §9.9
  code-teacher swap must move (`<corpus>-blind.jsonl`, `eval/blind.sh`,
  `eval/blind_cut.py`, the §15.3 gate in `run_eval.py`). Named-identifier
  sets remain the regression floor.
- **`run_eval.py` cannot referee a rendering or ranking change** (RESEARCH.md
  §21.2, §22.2, §23.2). It scores *generated* queries — 10–15 words, containing
  the gold file's own identifier ~70% of the time. Real agent queries are ~5
  words and do it 0.6% of the time. Measured on the same arms: `prune-decl`
  lost **0.15–0.28 at p<0.001 on all five corpora** offline and **−0.009
  [−0.065, +0.039]** on real agent queries. Offline *losses* fail to transfer,
  not just gains, so a negative offline result is not grounds to reject a
  design either. Use it for regression floors and leakage cuts; gate any engine
  change on `eval/locbench/guessplay.py`, which replays real harvested agent
  queries against real gold for free. Three confirmations now: §9.7, §10.6, §21. §23.2 closes the direction with a
  powered bound: **no document-side rendering improves retrieval on real agent
  queries by more than 0.023** (7,657 queries, 467 instances), `split` is
  0.011 *worse* than no rendering, and the standing `split`+`sif` recommendation
  is retired — the shipped `EmbedPreproc::None` stands.
  **Score file scopes with both function metrics, never one** (§24.1):
  `rank_func` (the chunk's best line falls inside the gold function) and
  `rank_func_ovl` (the chunk overlaps it at all) differ by **14.2pp**, because
  chunks are 32 lines and the median gold function is 12. That bracket is wider
  than every effect §20–§23 tried to detect, so every §22/§23 file-scope number
  is a lower bound — and a lever that moves the two in *opposite* directions is
  changing chunk geometry rather than retrieval quality, which is how §24.2
  told the finer-window arm apart from a win. `--file-scopes-only` skips the
  index build those rows never read, which is what makes an arm ~10 minutes
  instead of hours; it is blind to the directory half by construction, so a
  candidate that passes still needs a full-corpus confirmation.
- `eval/` — retrieval-quality harness. `run_eval.py` scores recall@k/MRR with
  paired bootstrap CIs + sign tests (`--baseline`, `--compare-modes`), cuts by
  `--stratify`/`--where`, and prints leakage above every table;
  `generate.py --anchor symbol` makes chunking-neutral query sets via
  `symbols.py`; `fetch-cosqa.sh` pulls 9k real human queries (the only set we
  didn't write — prefer it for quality claims, RESEARCH.md §12);
  `locbench/replay.py` replays real agent queries offline (§13.2).
  `locbench/triage.py` is the **gate between campaign tiers** (§18): it reads
  the per-invocation trace envelopes (`SEMGREP_TRACE_FILE`, set by `run.py`)
  beside the shim logs and exits nonzero on tool failures, agent distress
  (help probes, consecutive empty searches, a query repeated fruitlessly),
  or harness trouble. `locbench/capture.py` → `locbench/viewer.py` turn a
  campaign into one self-contained HTML page carrying the numbers *and* the
  trajectories behind them — every search an agent ran, what came back, and
  what the engine did. Nothing external, opens offline.
  `locbench/queryshape.py` reads query *style* by condition out of the shim
  logs (via `harvest.py`; `--since` scopes it to one campaign, or it sweeps
  every campaign ever run and compares arms across different instances). It is
  how a **tool-description** arm gets checked before any scoring: §19's A/B
  registers "did agents change how they write queries" as the gate on its
  accuracy endpoints, because a description that changed no behavior cannot be
  evidence about behavior. Style and not length, because
  `locbench/stylecut.py` — which reproduces §19.2b — measured that a *blind*
  description finds the gold 13% of the time against a blind name's 50%, so
  paraphrases are the longest queries and the worst, and a description that
  raised mean length by teaching questions would be a regression reported as a
  win.
  `locbench/campaign.sh` takes its arms as `CONDITIONS=`/`LIMIT=` parameters
  (defaults reproduce §16.9 exactly), so a new A/B does not fork the loop.
  **Query sets live in `eval/queries/`, checked in** — `eval/data/` is
  gitignored and the sets are `claude`-generated, so nothing published was
  reproducible without them. Three rg conditions exist on purpose: `rg`
  (legacy, weak — kept for comparability), `rg-strong` (fair), and `rg-oracle`
  (a *ceiling* — it consults the answer, so no agent can run it; §13.4).
  Report all three. `levers.sh` runs the §9 lever campaign and `diff.py`
  compares any two conditions. `pytest eval/tests` covers the scorers, which
  decide every published number.
- `eval/sim/` — simulation testing: behavior over a *sequence* of steps against
  evolving cache state, which neither of the above can see. A session is
  `mutate` / `invoke` / `check` steps under one isolated `SEMGREP_CACHE_DIR`;
  `eval/sim/scenarios.py` holds the catalog with machine-readable expectations
  and `eval/sim/PREREGISTER.md` the prose, **committed before the first run** so
  a contradicted prediction is a finding rather than a rewrite.
  `eval/sim/run.py` drives it, `eval/sim/report.py --check` regenerates
  `eval/sim/results/INDEX.md`. Sessions are checked in; scratch corpora go to
  the gitignored `eval/data/sim/`. Findings and their patch sites:
  `SIMULATION.md`.
- Guards that run beside the numbers: `eval/leakage.py` (how much of the
  answer a query already contains — §12.5 made structural),
  `eval/validate_queries.py` (`run_eval` refuses to score a query set that has
  drifted from its corpus), `bench/manifest.py --check` (detects a corpus tree
  that changed), `eval/reclaim.sh --dry-run` (what the harness holds on disk
  and what rebuilds it).

## Conventions

- Build: `cargo build --release` (first build downloads ese weights; needs
  network once). Test: `cargo test`. Don't hand-count tests here — the number
  drifts; `cargo test` prints it.
- `tools/snapshot.sh --check` is the behavior tripwire: ranked output over the
  frozen `tests/corpus/` fixture, 114 cases, byte-compared. Any change to ranking
  must either leave it identical or re-record it deliberately in the same commit.
  It has caught non-determinism that no test could see.
- Two invariants worth knowing before changing anything:
  **chunk-id lockstep** (below), and **cold == warm** — a cold search and a warm
  one must return identical results, asserted by
  `cold_and_warm_return_identical_results`. Both paths therefore quantize
  identically; scoring one in f32 and the other in i8 silently broke this for a
  long time (FIXES.md #11).
- Chunk ids are assigned in walk order and must stay in lockstep between the
  chunk table, BM25 add order, and `emb.bin` row order. The pass is parallel
  (`corpus::pass`, the single implementation of that guarantee) with a serial
  in-order fold that preserves it; `store::build_at` asserts the three agree.
- The index is a cache (RESEARCH.md §8): cold ranked searches write-through
  to `~/.cache/semgrep` (override `SEMGREP_CACHE_DIR`; tests and the eval
  harness isolate it). `cache::discover` resolves local/.semgrep, ancestor
  dirs (git-style walk-up), then cache entries by longest prefix. **Entries are
  keyed by chunk parameters as well as root**, so a `--window` sweep cannot
  poison ordinary searches — it could, and did (FIXES.md #10).
  `meta.json` is written last: writing it is what publishes an index.
  Read-repair validation is throttled by `SEMGREP_CACHE_TTL_SECS`
  (default 60; 0 = always validate). `--no-index` never reads or writes.
- `bench/run.py` invokes competitors by absolute path (`/usr/bin/grep`,
  `/opt/homebrew/bin/*`) because dev shells wrap `grep`.
- Smoke tests in sibling repos: set `SEMGREP_CACHE_DIR` to a temp dir (a
  plain ranked search now writes a cache entry for that scope).
- The benchmark corpora live in `bench/corpora/` (~5 GB with the linux index;
  refetch with `bench/fetch-corpora.sh`). Seven of them: linux (C, 84k files),
  vscode (TS, 4k), wikipedia (prose, 1k), plus tokio/commons-lang/etcd/jekyll
  (rust/java/go/ruby, 166–1,500 files, ~35 MB total) which sit in the <2k-file
  band where §9.7 found engine variants actually diverge. Every clone is
  pinned to a SHA; wikipedia cannot be (Wikimedia expires dated dumps), and
  the vscode/wikipedia trees fetched before pinning carry
  `revision: unknown` — recorded honestly rather than invented. Tree digests
  in `bench/corpora/MANIFEST.json` make a corpus checkable either way.

## Known costs (measured, M-series mac, linux kernel corpus, index v2, 256 dims)

Re-measured 2026-07-29 after the dim-256 switch (RESEARCH.md §10.7); numbers
that involve embeddings all moved, BM25 and keyword did not.

- binary 39.0 MB (was 72.8 MB at 512 dims — `weights.bin` is
  `TABLE_SIZE × (8 + dims × 4)`, so halving dims halves the compiled table)
- keyword ≈ rg (same engine crates), ~12 MB RSS
- cold (unindexed): semantic ~20 s / 154 MB; bm25 ~39 s / 916 MB (postings —
  candidate for two-pass streaming rewrite); hybrid ~53 s
- index build 27–46 s → 946 MB (386 MB i8 emb.bin + 541 MB bm25.flat),
  peak RSS 0.8–1.6 GB. vscode 2.1 s / 63 MB, wikipedia 205 MB.
  The spread is real, not sloppy measurement: wall time is dominated by
  page-cache state (a corpus already resident reads far faster) and peak RSS
  by rayon batch timing. Quote the range, or re-measure with `bench/run.py`
  and compare via `report.py --against` — single samples here mislead.
- warm queries: bm25 88 ms, semantic 53 ms, hybrid 115 ms (halving dims
  halved the embedding scan; the old f32 scan was fault/IO-bound at ~3-4 s).
  `--bm25-pin` (default 5 since §32.4b) adds the bm25 scan to every ranked
  search, so a warm default-mode query now pays both — ~140 ms at kernel
  scale, negligible on ordinary corpora
- the declaration boost (`--decl-boost`, on by default since §24.3) costs
  **1.1–1.5 ms** and is flat in corpus size — it re-reads the `k*3` candidate
  chunks, so the cost is 30 file reads whether the corpus is vscode or the
  kernel. ~3% of a warm kernel query and ~9% of a small-corpus one. It buys
  +0.039 strict / +0.048 overlap on file-scoped agent queries and +0.017 bm25
  on directory-scoped ones (RESEARCH.md §24.2)
- the learned checklist (`--learned-blend`, default 0.5 since §35.6) is
  arithmetic over the ≤30 finalize candidates — cost unmeasurable against a
  file read. It buys +0.012 strict [+0.005, +0.020] on directory-scoped real
  agent queries, +0.010 on file-scoped, +0.025 file rank, with the bm25
  tripwire improving rather than holding; the const weights live in
  `search/checklist.rs`, retrained by `eval/locbench/checklist_train.py`
  from a `guessplay.py --dump-hits` harvest (the two feature lists must not
  drift — both say so)
- corpus walk (parallel since FIXES.md #24): 272 ms on the 84k-file kernel,
  19 ms on vscode, ~5 ms on tokio/jekyll. Paid by a build, by `--check-stale`,
  and — the reason it was worth parallelizing — by read-repair on every warm
  query past the TTL
- `--stats` prints per-stage provenance; `--check-stale` is separate (walks
  the corpus, ~0.3 s on 84k files)
- hnsw.bin > 1 GiB is skipped at query time (from_bytes ~20 s at kernel
  scale); HNSW is for a future persistent/server mode
