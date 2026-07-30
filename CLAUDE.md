# semgrep (repo dir: semgrep/)

Semantic grep for agents built on the Bog stack: `../ese` (static embeddings,
512-dim, compiled-in weights) + `../anny` (HNSW). See DESIGN.md for the full
design; README.md for usage.

## Layout

- `crates/semgrep-core` — engine library (corpus walk/chunk, tokenizer, bm25,
  semantic, keyword via ripgrep crates, `.semgrep/` index format, search API)
- `crates/semgrep` — CLI (`target/release/semgrep`)
- `bench/` — perf harness vs grep/ggrep/rg/ugrep/ack (`fetch-corpora.sh`,
  `run.py`, `report.py`, `queries.json`); corpora + results are gitignored
- `eval/` — retrieval-quality harness. `run_eval.py` scores recall@k/MRR with
  paired bootstrap CIs + sign tests (`--baseline`, `--compare-modes`);
  `generate.py --anchor symbol` makes chunking-neutral query sets via
  `symbols.py`; `fetch-cosqa.sh` pulls 9k real human queries (the only set we
  didn't write — prefer it for quality claims, RESEARCH.md §12);
  `locbench/replay.py` replays real agent queries offline. Two rg baselines
  exist on purpose: `rg` (legacy, weak — kept for comparability) and
  `rg-strong` (fair). Report both.

## Conventions

- Build: `cargo build --release` (first build downloads ese weights; needs
  network once). Test: `cargo test` (48 tests + 11 doctests: 25 lib, 20 e2e incl.
  cache-transparency / stale-cache-eviction / budget, 3 model+token probes).
- Chunk ids are assigned in walk order and must stay in lockstep between the
  chunk table, BM25 add order, and `emb.bin` row order. The pass is
  parallel (`corpus::process_file` on rayon workers) with a serial in-order
  fold that preserves this.
- The index is a cache (RESEARCH.md §8): cold ranked searches write-through
  to `~/.cache/semgrep` (override `SEMGREP_CACHE_DIR`; tests and the eval
  harness isolate it). `index::discover` resolves local/.semgrep, ancestor
  dirs (git-style walk-up), then cache entries by longest prefix.
  Read-repair validation is throttled by `SEMGREP_CACHE_TTL_SECS`
  (default 60; 0 = always validate). `--no-index` never reads or writes.
- `bench/run.py` invokes competitors by absolute path (`/usr/bin/grep`,
  `/opt/homebrew/bin/*`) because dev shells wrap `grep`.
- Smoke tests in sibling repos: set `SEMGREP_CACHE_DIR` to a temp dir (a
  plain ranked search now writes a cache entry for that scope).
- The benchmark corpora live in `bench/corpora/` (~5 GB with the linux index;
  refetch with `bench/fetch-corpora.sh`, vscode needs GIT_LFS_SKIP_SMUDGE=1).

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
- `--stats` prints per-stage provenance; `--check-stale` is separate (walks
  the corpus, ~1 s on 84k files)
- hnsw.bin > 1 GiB is skipped at query time (from_bytes ~20 s at kernel
  scale); HNSW is for a future persistent/server mode
