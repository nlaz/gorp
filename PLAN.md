# Reorganization plan

Companion to `AUDIT.md`. Structured as the five passes: taxonomy → folders →
file-by-file → where functionality lands → how it ships. Then the testing pass.

**Governing constraint: this refactor changes no ranked output.** Every phase
ends with a byte-identical `--json` snapshot over a fixed corpus and query set.
Behavior changes (the bug fixes) are separate, labelled commits with their own
regression tests.

---

## Pass 1 — Taxonomy

Seven layers, each a verb the system performs, ordered by dependency. A layer
may only call downward.

```
keyword   exact match, grep semantics                    (independent of all below)

api       orchestrate a query; turn ranked ids into hits
  ↓
cache     choose the store that answers; keep it honest
  ↓
store     write and read representations on disk
  ↓
rank      query + representations → ordered ids
  ↓
text      text → tokens, text → vectors
  ↓
corpus    directory → files → chunks
```

The rule an agent can follow without reading anything else: **`rank` never
touches the filesystem, `store` never ranks, `cache` never scores, `api` never
computes.** It is checkable — see `tests/arch.rs` in the testing pass.

Two concepts get promoted to named types because they are currently invariants
held in local arithmetic:

- **`Rows`** — the union id space. Base rows (warm index) and delta rows
  (repair overlay) concatenated into one addressable list. Owns `n_base`, and
  is the *only* place `id < n_base` may appear.
- **`Trace`** — per-stage timing provenance. Replaces the ad-hoc
  `Vec<(String, f64)>` pushed from 14 sites.

---

## Pass 2 — Folder structure

```
crates/semgrep-core/src/
  lib.rs            types + re-exports only          (~60)
  corpus/
    mod.rs          walk, read_text, abs_path        (~90)
    chunk.rs        chunk_lines, doc_text, lines     (~90)
    pass.rs         pass_batches, process_file, pass (~110)
    diff.rs         live-vs-indexed tree diff        (~70)
  text/
    mod.rs
    token.rs        (was tokenize.rs)                (~100)
    embed.rs        embed_query, embed_docs          (~50)
    sif.rs          SifStats, embed_sif, token_vecs  (~110)
  rank/
    mod.rs          Mode, fuse                       (~90)
    bm25.rs         scoring core + in-memory store   (~180)
    vec.rs          distance, quantize, i8 kernels   (~110)
    topk.rs         TopK heap + brute-force scans    (~110)
    mmr.rs          diversity reranking              (~80)
    prf.rs          pseudo-relevance feedback        (~70)
    maxsim.rs       late-interaction reranking       (~90)
  store/
    mod.rs          IndexMeta, LoadedIndex, Needs    (~180)
    build.rs        build, build_at, manifest write  (~180)
    bm25.rs         FlatBm25 reader + flat writer    (~200)
  cache/
    mod.rs          discover, Discovered, fill       (~150)
    gen.rs          fingerprint, generation, gc      (~110)
    budget.rs       status, trim, clear              (~120)
    repair.rs       TTL gate, overlay construction   (~120)
  search/
    mod.rs          search(), Options, Hit, Report   (~150)
    rows.rs         Rows: the union id space         (~90)
    indexed.rs      warm path                        (~110)
    stream.rs       cold path                        (~100)
    hit.rs          dedupe, finalize, materialize    (~120)
    trace.rs        Trace                            (~40)
  keyword.rs        unchanged                        (140)

crates/semgrep/src/
  main.rs           parse + dispatch                 (~60)
  cli.rs            clap definitions                 (~130)
  cmd/
    search.rs       ranked + exact search command    (~110)
    index.rs        build / status                   (~70)
    cache.rs        status / prune / clear           (~70)
  out.rs            hit printing, footer, stats      (~140)
```

Twenty-eight files where there are ten, averaging ~110 lines. No file over
~200. Every name is one word. Directory + file name together carry the meaning
(`rank/bm25.rs`, `store/bm25.rs`, `cache/budget.rs`) so the leaf names stay
short — which also removes the `index::cache_clear()` / `index::cache_status()`
stutter.

---

## Pass 3 — File by file

| Today | Lines | Goes to | Notes |
|---|---|---|---|
| `lib.rs` | 55 | `lib.rs` | unchanged, plus layer doc + re-exports |
| `tokenize.rs` | 99 | `text/token.rs` | verbatim |
| `keyword.rs` | 140 | `keyword.rs` | verbatim; derive `Default` |
| `corpus.rs:10-74` | 65 | `corpus/mod.rs` | walk, read_text, abs_path |
| `corpus.rs:78-121, 186-219` | 75 | `corpus/chunk.rs` | `chunk_text`/`chunk_text_rel` collapse into one `lines()` |
| `corpus.rs:126-181` | 56 | `corpus/pass.rs` | plus the new shared `pass()` driver |
| — | new | `corpus/diff.rs` | extracted from `index.rs:752` + `search.rs:232` |
| `bm25.rs:8-126` | 119 | `rank/bm25.rs` | scoring core factored out behind a `Postings` trait |
| `bm25.rs:128-325` | 198 | `store/bm25.rs` | flat writer + `FlatBm25`, implements `Postings` |
| `semantic.rs:9-24` | 16 | `text/embed.rs` | drop `embed_batch` (dead) |
| `semantic.rs:32-144` | 113 | `text/sif.rs` | `maxsim` moves on to `rank/maxsim.rs` |
| `semantic.rs:146-214` | 69 | `rank/vec.rs` | drop `dot_distance` + f32 path (dead) |
| `semantic.rs:216-337` | 122 | `rank/topk.rs` | one `scan()`, i8 only |
| `index.rs:23-82, 628-769` | 200 | `store/mod.rs` | meta, `Needs`, `LoadedIndex`; `stale_files` delegates to `corpus::diff` |
| `index.rs:84-260` | 177 | `store/build.rs` | SIF pre-pass and embed-writer extracted |
| `index.rs:262-339` | 78 | `cache/mod.rs` | `discover` takes a boundary predicate |
| `index.rs:341-445, 563-627` | 170 | `cache/gen.rs` + `cache/mod.rs` | fingerprint/generation/gc; `fill` (was `write_cache_entry`) |
| `index.rs:447-559` | 113 | `cache/budget.rs` | `status`, `trim`, `clear` |
| `search.rs:14-113` | 100 | `search/mod.rs` | Mode moves to `rank/mod.rs` |
| `search.rs:115-171` | 57 | `search/mod.rs` | the orchestrator, largely unchanged |
| `search.rs:173-301` | 129 | `cache/repair.rs` | + `corpus/diff.rs` |
| `search.rs:307-595` | 289 | `search/indexed.rs` + `rank/{prf,maxsim}.rs` + `search/rows.rs` | see Pass 4 |
| `search.rs:602-729` | 128 | `search/stream.rs` | pass loop replaced by `corpus::pass` |
| `search.rs:739-765` | 27 | `rank/mod.rs` | `fuse` |
| `search.rs:771-919` | 149 | `search/hit.rs` | + `rank/mmr.rs` |
| `main.rs:29-173` | 145 | `cli.rs` | defaults sourced from core, not literals |
| `main.rs:187-234` | 48 | `cmd/cache.rs` + `out.rs` | `human()` to `out.rs` |
| `main.rs:236-291` | 56 | `cmd/index.rs` | |
| `main.rs:293-399` | 107 | `cmd/search.rs` | |
| `main.rs:401-510` | 110 | `out.rs` | footer, stats, context, peak RSS |

---

## Pass 4 — Where the functionality lands

### 4.1 `search_indexed`: 289 lines → ~45

```rust
pub fn run(d: &cache::Discovered, query: &str, opts: &Options) -> Result<SearchResult> {
    let mut trace = Trace::new();
    let idx = store::load(d, needs_for(opts, d), &mut trace)?;
    let rows = Rows::new(&idx, cache::repair::scope(d, &idx, &mut trace));

    let lexical = rank::bm25::top_k(&rows, query, opts, &mut trace);
    let lexical = rank::prf::expand(&rows, query, lexical, opts, &mut trace);
    let vector  = rank::vec_search(&rows, query, opts, &mut trace);
    let vector  = rank::maxsim::rerank(&rows, query, vector, opts, &mut trace);

    let fused = rank::fuse(opts.mode, lexical, vector, opts);
    let cands = rows.candidates(fused, &d.prefix, opts.k * 3);
    let hits  = hit::finalize(&d.root, query, cands, opts, &d.prefix, |c| rows.vector(c));
    Ok(SearchResult { hits, report: rows.report(trace, opts) })
}
```

The gain is not the line count. It is that `Rows` now owns the union id space:

```rust
pub struct Rows<'a> {
    idx: &'a LoadedIndex,
    repair: Option<Repair>,
    n_base: u32,
}

impl Rows<'_> {
    pub fn chunk(&self, id: u32) -> (Chunk, &str);   // the only `id < n_base` in the tree
    pub fn live(&self, id: u32) -> bool;             // was `tombstoned`, inverted
    pub fn vector(&self, id: u32) -> Option<Vec<f32>>;
    pub fn len(&self) -> usize;
}
```

Six scattered `n_base` comparisons collapse to one method. The invariant becomes
directly assertable (see T4 in the testing pass), which it is not today.

Also extracted here: `hnsw_worth_loading()` (replacing the 5-line comment at
`search.rs:313`) and `needs_for()` (the `LoadNeeds` decision).

### 4.2 `build_at`: 166 lines → ~50

Three extractions:

- `sif::count(root, &files, opts)` — the pre-pass and common-component
  estimation, currently a 50-line closure (`index.rs:107-156`).
- `EmbedWriter` — owns the pending batch, normalize → quantize → write →
  optional HNSW insert (`index.rs:169-194`).
- `corpus::pass(root, &files, params, |work| ...)` — the shared batched
  parallel driver, replacing the loop duplicated in `index.rs:201` and
  `search.rs:639`. One implementation of the chunk-id lockstep guarantee.

`build_at` becomes: walk → optional SIF → `pass` folding into
(`chunks`, `bm25`, `EmbedWriter`) → `write_manifest`.

### 4.3 `repair_scope`: 96 → ~45, and it moves

Splits into `cache/repair.rs` (TTL gate, overlay construction) and
`corpus/diff.rs`:

```rust
pub struct Diff { pub added: Vec<String>, pub modified: Vec<(u32, String)>, pub deleted: Vec<u32> }
pub fn diff(indexed: &[FileMeta], live: &[FileMeta], prefix: &str) -> Diff;
```

Pure, table-testable, and shared with `LoadedIndex::stale_files`, which becomes
`diff(...).len()`. This kills duplication #1 and makes B7's blast radius
measurable.

### 4.4 BM25: one scorer, two stores

```rust
pub trait Postings {
    fn n_docs(&self) -> usize;
    fn total_len(&self) -> u64;
    fn term(&self, s: &str) -> Option<TermId>;
    fn postings(&self, t: TermId) -> impl Iterator<Item = (u32, u16)>;
    fn doc_len(&self, id: u32) -> u32;
}
pub fn top_k<P: Postings>(store: &P, query: &str, k: usize) -> Vec<(u32, f32)>;
```

`Bm25Index` (in-memory, `rank/bm25.rs`) and `FlatBm25` (mmap, `store/bm25.rs`)
each implement it. The parity test stops being a fixture assertion and becomes
a property over generated corpora.

### 4.5 CLI

`Cli` keeps its 20 flags but sources defaults from core rather than repeating
them: `#[arg(default_value_t = ChunkParams::default().window)]`. The hidden
harness knobs (`prf`, `maxsim*`, `sif*`, `sem_weight`, `mmr_lambda`, `window`,
`overlap`) group into one `#[command(flatten)] Tuning` struct — one place to
look, and the day they graduate or die it's one deletion.

`run_search` splits into `cmd/search.rs` (assemble options, dispatch, exit
code) and `out.rs` (every `println!`). That separation is what makes CLI
output testable without spawning a process for the formatting cases.

### 4.6 Naming

Short, module-qualified, no stutter:

| Today | Becomes |
|---|---|
| `index::cache_clear` / `cache_status` / `cache_base` / `cache_entries` | `cache::clear` / `status` / `base` / `entries` |
| `index::gc_old_generations` | `cache::gc` |
| `index::enforce_budget` | `cache::trim` |
| `index::write_cache_entry` | `cache::fill` |
| `index::compat_key` / `table_fingerprint` | `cache::generation_key` / `fingerprint` |
| `semantic::brute_force_top_k_i8` | `rank::topk::scan` |
| `corpus::chunk_text` + `chunk_text_rel` | `corpus::lines` |
| `search::finalize_hits` | `hit::finalize` |
| `search_indexed` / `search_streaming` | `indexed::run` / `stream::run` |
| `SearchReport.n_chunks_considered` | `.considered` |
| `index::SEMGREP_DIR` | `store::DIR` |

### 4.7 Comments

The rule: a comment block of 4+ lines inside a function body becomes either a
named function or a one-line `RESEARCH.md` pointer. Keep every comment that
records a *measurement* or a *decision with a rejected alternative* — that's
the irreplaceable content. Delete comments that restate the code. Expected net:
~150 comment lines removed, ~15 new function names, no lost rationale.

Add `// SAFETY:` to the six `unsafe` blocks (currently zero of them have one)
and rename `bytemuck_cast_u32`/`_u64` — they are named after a crate this
project does not depend on.

---

## Pass 5 — How it ships

Each phase is independently mergeable and ends green. `snapshot.sh` (built in
P0) is the tripwire.

| Phase | Work | Risk | Size |
|---|---|---|---|
| **P0** | Safety net: `rustfmt.toml` codifying current style; `.github/workflows/ci.yml` (fmt, clippy `-D warnings`, test, pytest); `tools/snapshot.sh` recording `--json` for 30 queries × 4 modes over the e2e fixture and `bench/corpora/vscode`; characterization tests for the CLI contract | none | S |
| **P1** | Delete dead code (§5 of the audit): f32 path, `embed_batch`, `threshold`, `grep-matcher`, `IndexMeta.{normalized,quantized}`; fix all 9 clippy warnings | low | S |
| **P2** | Fix B2/B3/B4 (write `root.txt` first, build to temp dir + rename, `trim` after registration) and B5 (marker only for cache entries). Regression test each | low | S |
| **P3** | Extract pure functions: `corpus::diff`, `bm25::top_k` over `Postings`, `corpus::pass`, `corpus::lines`. Unit-test each as it lands | medium — touches the lockstep loop | M |
| **P4** | Move files into the layer directories. Mechanical; no logic edits. One commit per layer, `cargo test` + snapshot between | low (large diff, no semantics) | M |
| **P5** | Decompose the monsters: `Rows`, `Trace`, `search_indexed` → stages, `build_at` → pass + writer, `main.rs` → `cmd/*` + `out.rs` | **highest** — do it last, on top of the safety net | L |
| **P6** | Fix B1 (chunk params in the cache key) and B6 (unify truncation). These *change behavior*, so they ship after the snapshot has proven everything else didn't | medium | S |
| **P7** | Test up-level: everything in the testing pass below | none | L |
| **P8** | Python harness: consolidate 7 scripts → 2, add pytest for the scorers | low | M |
| **P9** | Docs: regenerate `CLAUDE.md` layout/conventions for the new tree, drop the hand-counted test number, single-source the perf numbers, add `RESEARCH.md` anchors + the link-check test | none | S |

Phase gate, every phase: `cargo test` green · `cargo clippy -D warnings` clean ·
`tools/snapshot.sh --check` byte-identical (except P6, which updates the
snapshot in the same commit that changes behavior, with the diff reviewed
line by line).

Full-refactor gate, before P6: run `eval/run_eval.py` on one corpus at HEAD and
at the refactor tip. R@5 and MRR@10 must be **identical**, not close.

---

## Testing pass

### Principle

Test structure mirrors code structure. A layer's invariants are tested at that
layer; end-to-end tests assert only what genuinely spans layers. Today the ratio
is inverted — 16 e2e tests carry behavior that ought to be 40 unit tests, which
is why a failure points at "search is wrong" instead of at a function.

No new dependencies. Property tests use the hand-rolled LCG already in
`semantic.rs`; CLI tests use `std::process::Command`.

### Files

```
crates/semgrep-core/tests/
  corpus.rs     walk, chunk, batch, diff
  rank.rs       bm25, vectors, fusion, mmr, prf, maxsim
  store.rs      format, build, load, lockstep
  cache.rs      discovery, generations, budget, corruption   (absorbs today's e2e cache tests)
  repair.rs     drift matrix, delta-vs-rebuild equivalence
  search.rs     cold/warm parity, mode behavior              (absorbs the rest of e2e)
  arch.rs       layering rule
  probe/        modelprobe.rs, tokprobe.rs (research probes, kept)
crates/semgrep/tests/
  cli.rs        process-level contracts
eval/tests/
  test_scoring.py test_symbols.py test_report.py test_gate.py
```

### Invariant tests (the ones worth writing first)

- **T1 — chunk coverage.** For random (window, overlap, file), every non-blank
  line appears in ≥1 chunk; spans are monotone; ids dense from 0.
- **T2 — store parity.** Random corpora + queries: `Bm25Index::top_k` ==
  `FlatBm25::top_k` within 1e-5. Replaces one fixture assertion.
- **T3 — quantization fidelity.** Random unit vectors: i8 top-k ⊆ f32 top-2k.
  Bounds the approximation that the whole v2 format rests on and that nothing
  currently checks.
- **T4 — id lockstep.** Build an index over a random corpus:
  `chunks.len() == emb rows == bm25.n_docs()`, and chunk *i*'s BM25 tokens equal
  `tokenize(doc_text(path_i, text_i))`. This is the invariant `CLAUDE.md` calls
  out as load-bearing and that no test asserts.
- **T5 — repair ≡ rebuild.** For a random edit sequence (add / modify / delete /
  truncate / binary-flip), top-k from (warm index + repair overlay) equals
  top-k from a full rebuild. This is the cache-transparency claim as a property
  rather than one anecdote — the highest-value test in the plan, and only
  writable once `Rows` and `corpus::diff` exist.
- **T6 — fusion.** `sem_weight = 0` ⇒ output order == BM25 order; output is a
  permutation of the input union; ties break deterministically under input
  shuffling.
- **T7 — MMR.** `lambda = 1.0` ⇒ identity order; no duplicates;
  `len == min(k, n)`.
- **T8 — cold/warm parity, generalized.** Today: one query, `hits[0]`. Should
  be: 20 queries × 4 modes × {streaming, fresh cache, warm cache, warm+repair},
  comparing the full top-k, not the first hit.
- **T9 — layering.** Parse `use crate::…` per module; assert no upward
  dependency (`rank` importing `store`, `store` importing `cache`, etc.). Ten
  lines, and it is what keeps the taxonomy true a year from now.
- **T10 — doc links.** Every `RESEARCH.md §N` cited in source resolves to a
  real section heading.

### Behavior tests currently missing

- **CLI (`crates/semgrep/tests/cli.rs`)** — exit 0 with hits / 1 without / 2 on
  error; `-e` grep semantics including `-i`, `-F`, the 250-cap, `--all`; stdout
  stays `path:line:text` and every advisory goes to stderr; `--json` field set
  is stable; `-C` context format; `index --status` exit codes; `cache
  --status/--prune/--clear` output shape; the miss-suggestion fallback fires
  only with a warm index.
- **Discovery boundaries** — `.git` stop, `$HOME` stop, deepest-cache-entry
  selection, prefix scoping. Requires `discover` to take a boundary predicate
  (P5) instead of reading the real environment.
- **PRF** — term selection given a known corpus; high-tf/low-df wins; query
  tokens excluded; `prf_terms = 0` is a no-op.
- **MaxSim** — `blend = 0.0` ⇒ original order preserved; `blend = 1.0` ⇒ pure
  MaxSim; head size respected; empty query tokens ⇒ no-op.
- **Format guards** — wrong version, wrong dims, truncated `emb.bin`, truncated
  `bm25.flat`, missing `chunks.bin`: each produces the specific error, and
  each degrades to a miss when `from_cache`.
- **Adversarial BM25 flat encoding** — empty term list, tf saturation at
  `u16::MAX`, a term that is a prefix of another (binary search correctness),
  zero-doc index.
- **Concurrency** — two processes searching the same cold scope simultaneously
  both return correct results and neither deletes the other's entry (B4).
- **Regressions** — B1 (params in cache key), B2 (orphan entry reclaimed),
  B3 (new entry counted by the budget).

### Python

`eval/scoring.py` and `eval/symbols.py` are pure functions that determine every
number published in `RESEARCH.md §11`, and they have no tests. Minimum:

- `score_instance` / `func_match` / `parse_gold` / `norm_path` — table-driven
  over hand-checked cases including the tolerant-match rules.
- `symbols.extract` — one fixture per supported language, including the
  `related_descriptors.py` case that motivated dropping tree-sitter.
- `run_eval.correct` — slack-window boundary conditions.
- `bench/report.gate` — regression gate fires on a known-bad row, passes on a
  known-good one.
- `locbench/report.load` — the dedupe-by-(instance, condition, model) rule.

Wire into CI alongside `cargo test`.

### Harness consolidation (P8)

- `run-levers.sh` + `run-levers2.sh` + `run-levers3.sh` + `run-pool-sweep.sh` →
  one `eval/levers.sh <condition>...` with the conditions in a table. The four
  copies of `run_if_missing` become one.
- `lever-report.py` + `tune-report.py` + `model-ab-report.py` →
  `eval/diff.py --base <tag> --cand <tag>`.
- `locbench/{compare,table-ab,stratify}.py` → subcommands of
  `locbench/report.py`.
- Add `eval/clean.sh` for the 1.3 GB of locbench mirrors, and say so in
  `eval/README.md`.

Net: 16 scripts → 9, ~250 fewer lines, and the surviving ones get tests.

---

## Non-goals

- No new dependencies (no `proptest`, no `assert_cmd`, no `bytemuck`).
- No change to the on-disk format. `FORMAT_VERSION` stays 2 — dropping the
  `normalized`/`quantized` fields is a `serde` default-compatible change, and
  existing entries stay readable.
- No change to the CLI surface, except B1's fix (chunk params join the cache
  key, so a non-default `--window` gets its own entry instead of poisoning the
  shared one).
- No performance work. The numbers in `CLAUDE.md` must hold; the snapshot gate
  and `bench/report.py --gate` enforce that they do.
