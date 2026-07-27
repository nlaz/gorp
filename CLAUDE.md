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
- `eval/` — retrieval-quality harness (`generate.py` makes LLM query sets,
  `run_eval.py` scores recall@k/MRR, `agent-eval.md` protocol)

## Conventions

- Build: `cargo build --release` (first build downloads ese weights; needs
  network once). Test: `cargo test` (26 tests incl. e2e fixture corpus).
- Chunk ids are assigned in walk order and must stay in lockstep between the
  chunk table, BM25 `add_doc` order, and `emb.bin` row order.
- `bench/run.py` invokes competitors by absolute path (`/usr/bin/grep`,
  `/opt/homebrew/bin/*`) because dev shells wrap `grep`.
- Never leave a `.semgrep/` dir in sibling repos after smoke tests.
- The benchmark corpora live in `bench/corpora/` (~5 GB with the linux index;
  refetch with `bench/fetch-corpora.sh`, vscode needs GIT_LFS_SKIP_SMUDGE=1).

## Known costs (measured, M-series mac, linux kernel corpus, index v2)

- keyword ≈ rg (same engine crates), ~12 MB RSS
- cold (unindexed): semantic ~20 s / 154 MB; bm25 ~39 s / 916 MB (postings —
  candidate for two-pass streaming rewrite); hybrid ~53 s
- index build ~59 s → 1.3 GB (737 MB i8 emb.bin + 515 MB bm25.flat)
- warm queries: bm25 80 ms, semantic 80 ms, hybrid 135 ms (quantized brute
  scan; the old f32 scan was fault/IO-bound at ~3-4 s)
- `--stats` prints per-stage provenance; `--check-stale` is separate (walks
  the corpus, ~1 s on 84k files)
- hnsw.bin > 1 GiB is skipped at query time (from_bytes ~20 s at kernel
  scale); HNSW is for a future persistent/server mode
