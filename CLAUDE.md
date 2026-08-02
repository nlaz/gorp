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
  index.
- `text/` — text into representations. `token` (code-aware tokenizer), `embed`,
  `sif` (rarity-weighted pooling, §9.1).
- `rank/` — query plus representations into ordered ids. `bm25` (one scorer over
  a `Postings` trait), `vec` (kernels and quantization), `topk`, `fuse`, `mmr`,
  `prf`, `maxsim`.
- `store/` — representations on disk. `build` (+ `build/embed`, `build/sif`),
  `load`, `bm25` (the flat mmap layout).
- `cache/` — which index answers, and keeping it honest. `mod` (discovery,
  fill), `compat` (generations), `budget`, `repair` (the read-repair overlay).
- `search/` — orchestration and materialization. `indexed` (warm), `stream`
  (cold), `rows` (the union id space), `hit`.
- `keyword.rs` — the exact-match escape hatch, independent of all of it.

`crates/semgrep` is the CLI: `cli` (flags), `cmd/` (one file per verb), `out`
(every write to stdout or stderr). **stdout is data, stderr is commentary** —
`crates/semgrep/tests/cli.rs` enforces it.

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
- `eval/` — retrieval-quality harness. `run_eval.py` scores recall@k/MRR with
  paired bootstrap CIs + sign tests (`--baseline`, `--compare-modes`), cuts by
  `--stratify`/`--where`, and prints leakage above every table;
  `generate.py --anchor symbol` makes chunking-neutral query sets via
  `symbols.py`; `fetch-cosqa.sh` pulls 9k real human queries (the only set we
  didn't write — prefer it for quality claims, RESEARCH.md §12);
  `locbench/replay.py` replays real agent queries offline (§13.2).
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
  halved the embedding scan; the old f32 scan was fault/IO-bound at ~3-4 s)
- corpus walk (parallel since FIXES.md #24): 272 ms on the 84k-file kernel,
  19 ms on vscode, ~5 ms on tokio/jekyll. Paid by a build, by `--check-stale`,
  and — the reason it was worth parallelizing — by read-repair on every warm
  query past the TTL
- `--stats` prints per-stage provenance; `--check-stale` is separate (walks
  the corpus, ~0.3 s on 84k files)
- hnsw.bin > 1 GiB is skipped at query time (from_bytes ~20 s at kernel
  scale); HNSW is for a future persistent/server mode
