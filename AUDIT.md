# Codebase audit

Date: 2026-07-29. Scope: all of `crates/`, `eval/`, `bench/`, plus repo docs and
tooling. Method: full read of the 3,142 lines of Rust; structural read of the
2,300 lines of Python; `cargo test` (41 pass), `cargo clippy`, `cargo fmt
--check`; three latent bugs reproduced against the release binary.

The implementation plan is in `PLAN.md`. This file is the evidence.

---

## 1. Verdict

The engine is good code with a real problem: **the module boundaries stopped
tracking the design about three features ago.** Nothing here is rotten, and
almost nothing needs rewriting — but four concerns (build, load, cache, repair)
have collapsed into two files, and the two largest functions now hold the
system's most fragile invariant (the chunk-id space) as unnamed local
arithmetic instead of as a type.

| Dimension | State | Direction |
|---|---|---|
| Correctness | 41 tests green; 3 confirmed latent bugs, 2 user-visible | fix + regress |
| Organization | 8 flat files; 2 of them hold 56% of the code and 4 concerns | split by layer |
| Function size | 3 functions ≥ 130 lines; the largest is 289 | decompose |
| Duplication | 4 real pairs (tree diff, corpus pass, BM25 scoring, chunk read) | extract |
| Dead code | ~110 lines unreachable, 1 unused dependency | delete |
| Test coverage | strong on cache behavior, **zero** on the CLI, thin on invariants | up-level |
| Tooling | no CI, no rustfmt.toml, 9 clippy warnings | add gates |
| Harness (Python) | 2,300 lines, 0 tests, 7 scripts superseded by their own results | consolidate |

The single most useful sentence in this audit: **most of the long comments in
this codebase are the names of functions that don't exist yet.** The rationale
prose is genuinely good — it encodes measurements — but it is doing the job
that structure should do. The refactor below is largely a mechanical transform
of comment blocks into named units, which also happens to be exactly what makes
the code legible to an agent reading one file at a time.

---

## 2. Functional taxonomy vs. actual file layout

What the system actually does, as a pipeline:

1. **corpus** — a directory becomes an ordered list of files, then chunks
2. **text** — a chunk becomes tokens (lexical) and a vector (semantic)
3. **rank** — a query plus those representations becomes an ordered id list
4. **store** — representations are written to / read from disk
5. **cache** — deciding *which* store answers a query, and keeping it honest
6. **api** — orchestration, then turning ranked ids into displayable hits
7. **keyword** — the exact-match escape hatch, independent of 1–6

Where that lives today:

| Concern | File(s) | Lines | Problem |
|---|---|---|---|
| corpus | `corpus.rs` | 251 | ok, but batching + per-file work are mixed with walk/chunk |
| text | `tokenize.rs`, part of `semantic.rs` | 99 + ~130 | embedding and vector math share a file |
| rank | scattered: `bm25.rs`, `semantic.rs`, `search.rs` | ~700 | fusion, PRF, MaxSim, MMR are all inline in `search.rs` |
| store | `index.rs` 1–260, 628–769 | ~400 | build and load are one file with the cache |
| cache | `index.rs` 262–627 + `search.rs` 173–301 | ~460 | **split across two files in two crates' worth of concepts** |
| api | `search.rs` | 995 | holds all of the above plus orchestration |
| keyword | `keyword.rs` | 140 | clean, self-contained, no changes needed |

Two observations follow directly:

- `index.rs` is three modules wearing a trench coat: on-disk format, index
  build, and the central cache (discovery, generations, budget, eviction, GC).
  These have different failure modes — a corrupt format is a bug, a missing
  cache entry is normal — and the code already says so in comments.
- `search.rs` contains the read-repair overlay (96 lines), which is a *cache*
  concern, not a search concern. It lives there because it needs the ranking
  types. That's a signal the id-space abstraction is missing, not that the
  layering is wrong.

---

## 3. Complexity hot spots

| Function | File:line | Lines | Distinct jobs it does |
|---|---|---|---|
| `search_indexed` | `search.rs:307` | **289** | load policy, HNSW size heuristic, repair, id resolution, BM25, PRF, vector search, MaxSim rerank, fusion, scope filter, vector lookup, timing |
| `build_at` | `index.rs:95` | **166** | walk, SIF pre-pass, common-component estimation, batched parallel pass, embed/normalize/quantize/write, HNSW insert, 6 manifest writes, size accounting |
| `search_streaming` | `search.rs:602` | **128** | walk, batched pass (duplicate of the above), streaming top-k, BM25, fusion, candidate build, re-embed, timing |
| `repair_scope` | `search.rs:206` | 96 | TTL gate, walk, tree diff, delta chunking, delta BM25, delta embedding |
| `run_search` (CLI) | `main.rs:306` | 65 | option assembly, printing, miss fallback, footer, stats, exit code |
| `cache_entry_dir` | `index.rs:563` | 43 | hash, label derivation, truncation, collision probe |
| `Cli` struct | `main.rs:38` | 99 | 20 flags, 8 of them hidden harness knobs |

`search_indexed` is the one that matters. Its `resolve`, `tombstoned`,
`bm25_rank` closures and the bare `n_base` comparisons (`if id < n_base`,
`id - n_base`, appearing at lines 346, 353, 358, 372, 455, 459, 558, 560)
implement a **union id space**: base rows from the warm index, delta rows from
the in-memory repair overlay, concatenated. That union is the most fragile
invariant in the system and it is currently expressed as arithmetic in six
places inside a 289-line function. It has no name and no test.

Everything else in that function is a pipeline stage that could be a line.

---

## 4. Duplication

Four pairs, all real, all extractable to a pure function:

1. **Tree diff.** `LoadedIndex::stale_files` (`index.rs:752`) and
   `repair_scope` (`search.rs:232-262`) both diff a live walk against
   `meta.files` on `(size, mtime)`. Two implementations of one predicate; the
   repair one additionally handles a scope prefix. Neither is unit-testable
   because both are welded to a filesystem walk.

2. **The batched parallel pass.** `index.rs:201-221` and `search.rs:639-669`
   are the same loop — `pass_batches` → `par_iter().map(process_file)` → serial
   in-order fold — differing only in what the fold does with each `FileWork`.
   The comment in `search.rs` even says "see index::build for the rationale".
   This is the loop that guarantees chunk-id lockstep, duplicated.

3. **BM25 scoring.** `Bm25Index::query` (`bm25.rs:93`) and `FlatBm25::query`
   (`bm25.rs:294`) are the same formula over two storage layouts, ~30 lines
   each. Their agreement is asserted empirically by one fixture test
   (`flat_matches_in_memory`); it should be structural.

4. **Chunk text read.** `chunk_text_rel` (`corpus.rs:186`) and `chunk_text`
   (`corpus.rs:203`) have identical bodies; one takes `&str`, one takes
   `&FileMeta`, one returns `Option`, one returns `Result`.

Plus a softer one: default values for `window`, `overlap`, `sem_weight`,
`mmr_lambda`, and `k` are each written in two or three places
(`ChunkParams::default`, `SearchOptions::default`, and one or two `clap`
`default_value_t` attributes). Nothing keeps them in sync.

---

## 5. Dead weight

### Rust — unreachable in production

- **The f32 embedding path (~80 lines).** `FORMAT_VERSION` is 2; `load_dir`
  rejects anything else; `build_at` unconditionally writes
  `normalized: true, quantized: true`. Therefore `IndexMeta.normalized`,
  `IndexMeta.quantized`, `LoadedIndex::emb_matrix()` (`index.rs:733`),
  `semantic::dot_distance` (`semantic.rs:167`), and the `normalized` branch of
  `brute_force_top_k` are all unreachable outside their own unit tests. The
  format version already carries this information.
- `semantic::embed_batch` (`semantic.rs:18`) — no callers anywhere.
- `TopK::threshold` (`semantic.rs:326`) — no callers anywhere.
- `grep-matcher` — declared in the workspace manifest *and* in
  `semgrep-core/Cargo.toml`, never imported. (`grep-regex` and `grep-searcher`
  are used; `grep-matcher` is a transitive dep that doesn't need declaring.)
- `LoadNeeds::all()` — test-only; harmless, but it belongs in a test helper.

### Rust — fragile leftovers

- `SifStats::merge` merges `freqs` and `total` but silently drops `a` and
  `mean`. It works today only because `build_at` sets `a` after merging. A
  future caller that merges post-configuration gets wrong weights with no
  error.
- `Bm25Index::add_doc` is used only by the repair path (`search.rs:280`) and
  tests, while the build path uses `add_tokenized`. Two entry points for the
  same operation, and the repair path is the one that skips the parallel
  tokenizer.

### Python / harness

- Four one-off experiment drivers whose results are already published in
  `RESEARCH.md §9`: `run-levers.sh`, `run-levers2.sh`, `run-levers3.sh`,
  `run-pool-sweep.sh` (~150 lines, three of them near-copies with a shared
  `run_if_missing` helper duplicated verbatim in each).
- Three near-identical result-diff scripts: `lever-report.py` (53),
  `tune-report.py` (57), `model-ab-report.py` (56). All three load
  `eval/data/<tag>-<corpus>.json`, align on (mode, kind), print deltas.
- Three overlapping locbench aggregators over the same JSONL:
  `report.py`, `compare.py`, `table-ab.py`, `stratify.py`.
- `eval/data/` is 1.3 GB (gitignored — hygiene is fine), almost all of it
  locbench git mirrors. There is no documented way to reclaim it.

### Docs

- `CLAUDE.md` claims "38 tests"; there are 41. A hand-maintained count will
  always drift.
- Measured performance numbers appear in `CLAUDE.md`, `README.md`,
  `RESULTS.md`, and `bench/results/report.md`. Only the last is generated.
- Source comments cite `RESEARCH.md §8`, `§9.1`–`§9.6`, `§10.7`, `§11.4`,
  `§11.5`. Nothing verifies those sections still exist under those numbers.

---

## 6. Latent bugs

Three confirmed by reproduction against `target/release/semgrep`, plus four
found by reading.

### B1 — Chunk params are not part of the cache identity *(confirmed, user-visible)*

`cache_entry_dir` keys entries on the canonical root path only; `compat_key`
covers format version, dims, and the embedding-table fingerprint — but not
`window`/`overlap`. `search_indexed` never compares `opts.params` against
`idx.meta.params`. So a single search with a non-default window silently
poisons every later search of that scope:

```
$ semgrep --window 8 --overlap 2 "retry backoff" .     # writes an 8-line-window entry
{"path":"src/a.py","start_line":79,"end_line":84,...}
$ semgrep "retry backoff" .                            # default window is 32
{"path":"src/a.py","start_line":79,"end_line":84,...}  # still 8-line spans
$ cat .cache/*/*/meta.json | jq .params
{"window": 8, "overlap": 2, "max_file_bytes": 4194304}
```

The hidden flags exist for the eval harness, and the harness runs against the
same cache directory as ordinary use unless `SEMGREP_CACHE_DIR` is set — which
is exactly how a tuning sweep silently contaminates a subsequent measurement.

### B2 — Interrupted cold searches leak invisible cache entries *(confirmed)*

`write_cache_entry` creates the entry directory and runs `build_at` (which
immediately creates `emb.bin`) **before** writing `root.txt`. Every enumerator —
`cache_entries`, `cache_status` — skips directories without `root.txt`, and
`gc_old_generations` only inspects generation directories, never entries inside
the current one. So an entry orphaned by a Ctrl-C during a first ranked search
of a large repo is permanently invisible and permanently unreclaimable:

```
$ ls .cache/v2-d256-0d2d/
orphan-halfbuilt   scratchpad-probe-975595b2
$ semgrep cache --prune
pruned 0 entries, reclaimed 0 B
.../probe/.cache  (1 entries, 6.5 KB of 2.1 GB budget)   # 5 MB orphan uncounted
$ du -sh .cache
5.0M  .cache
```

Ctrl-C during the first big index is precisely when users interrupt.

### B3 — `enforce_budget` runs before the new entry is registered *(confirmed by reading, same root cause as B2)*

In `write_cache_entry` (`index.rs:615-620`) the order is: `build_at` →
`gc_old_generations` → `enforce_budget` → write `root.txt`. The entry that was
just built is therefore not visible to the budget enforcer that runs
immediately after building it. Indexing a corpus larger than the 2 GiB budget
evicts every *other* entry and then leaves the oversized one in place until
some future write.

### B4 — Index builds are not atomic

`build_at` writes `emb.bin` incrementally, then `meta.json`, then `chunks.bin`,
`bm25.flat`, `sif.bin`, `hnsw.bin`. Discovery requires only `meta.json`. A
second process searching the same scope in the window between the `meta.json`
write and the `chunks.bin` write will discover the entry, fail to load it, and
— because `from_cache` is true — `remove_dir_all` the directory the first
process is still writing into (`search.rs:160-163`). Narrow, but the recovery
path actively destroys another process's work. Build to a temp dir and rename.

### B5 — Read-repair writes into repo-local `.semgrep/`

`repair_scope` touches `<index_dir>/last_check` on every validation
(`search.rs:223`), including when the index is a repo-local `.semgrep/`
committed as an explicit artifact. A read-only query mutates a tracked
directory. Cache entries are the right place for this marker; repo-local
indexes should use a per-user location or skip the marker.

### B6 — Fusion truncation differs between the two paths

Indexed fuses to `pool * 2` (256) then scope-filters then takes `k * 3`;
streaming fuses directly to `k * 3` (`search.rs:537` vs `690`). For a whole-root
query the visible top-k agrees, which is why the parity test passes — but the
two paths are not the same function of their inputs, and the parity test only
checks `hits[0]`.

### B7 — Delta BM25 statistics are merged across incompatible IDFs

`repair_scope`'s delta index computes IDF over the delta corpus alone (a handful
of files), then its scores are merged and sorted against base scores computed
over the whole corpus (`search.rs:370-377`). The comment acknowledges this
("close enough to merge for a small overlay"). It is a documented approximation,
not a bug — but there is no test bounding how wrong it gets as the delta grows,
and the delta grows without limit between rebuilds.

---

## 7. Test coverage audit

41 tests: 22 unit (in-file), 16 e2e, 3 research probes. All pass in 0.08 s.

### What is genuinely well covered

Cache semantics — and it's the best part of the suite. Write-through, ancestor
discovery, scope promotion, read-repair, compat generations, unreadable
entries, corrupt entries, budget/LRU eviction. Each test states its invariant
in a doc comment. This is the model the rest of the suite should follow.

### Gaps, by layer

| Layer | Covered | Missing |
|---|---|---|
| corpus | chunk_lines (3 unit tests) | walk: gitignore, hidden files, binary sniff, size cap, `.semgrep` exclusion, sort determinism; `pass_batches` boundary math; `process_file` |
| text | tokenizer (4 unit tests) | SIF weighting, `embed_sif` vs `embed_query` space equivalence, `token_vectors`, `maxsim` |
| rank | TopK, brute force vs naive, RRF weighting, MMR | PRF term selection (**zero tests** for 33 lines of live code), MaxSim rerank path (**zero**), quantized-vs-f32 ranking agreement, fusion determinism under tie shuffling |
| store | flat-vs-memory BM25 (1 fixture), staleness count | format guards (version/dims/size mismatch produce the right error), chunk-id lockstep invariant, HNSW skip threshold, atomicity, `to_flat_bytes` round-trip on adversarial input (empty terms, 65k+ tf saturation) |
| cache | strong (see above) | discovery boundary rules (`.git` stop, `$HOME` stop) — untestable today because `discover` reads the real environment; orphan entries; concurrent build; params mismatch (B1) |
| repair | 1 test, 3 assertions | deletion, rename, file→binary, empty file, TTL honored/expired, scope-limited repair, delta-vs-rebuild equivalence |
| CLI | **nothing — `crates/semgrep` has 0 tests** | exit codes (0/1/2), `-e` grep semantics, `--json` schema stability, `-C` context format, the 250-cap and `--all`, stderr footer contract, `index --status`, `cache` (bare = status) `/--prune/--clear`, miss-suggestion fallback |
| harness | **nothing — `eval/` and `bench/` have 0 tests** | `scoring.py:score_instance/func_match/parse_gold`, `symbols.py:extract`, `run_eval.py:correct` + bootstrap/sign-test, `bench/report.py:gate` |

Three of those deserve emphasis:

- **The CLI is the product and it is entirely untested.** Every contract in
  `main.rs` — exit 1 on no match, exit 2 on error, stdout stays grep-parseable,
  the footer goes to stderr, `--all` defeats the cap — is asserted only by the
  README.
- **PRF and MaxSim are live, flagged, and untested.** 60+ lines of ranking
  logic inside `search_indexed`, reachable via hidden flags used by the eval
  harness that produces the numbers in `RESEARCH.md`.
- **The Python scorers guard every published number and have no tests.** A
  wrong `func_match` silently changes every conclusion in `RESEARCH.md §11`.

### Structural obstacles to testing

These are *why* the gaps exist, and each is fixed by the refactor rather than
by writing more tests:

- `discover` reads `$HOME` and the real filesystem directly → boundary rules
  can't be exercised. Take the stop-predicate as a parameter.
- The tree diff is inside a function that walks the disk → the diff logic can't
  be tested with a table of (indexed, live) inputs.
- PRF/MaxSim/fusion are inline closures inside a 289-line function → nothing to
  call.
- The union id space has no type → the invariant can't be asserted, only
  observed through end-to-end results.
- `SearchReport.stages` is `Vec<(String, f64)>` built ad hoc at 14 call sites →
  no test can assert "the indexed path always reports these stages".

---

## 8. Conventions and tooling

- **No CI.** No `.github/`. Everything is enforced by remembering to run it.
- **No `rustfmt.toml`**, and `cargo fmt --check` currently wants to reformat
  the codebase — it disagrees with the hand-maintained style (single-line
  `if`/`else` bodies, import ordering). The style is deliberate and consistent;
  it should be codified, not abandoned.
- **9 clippy warnings**, all trivial: 4 unit-valued `let` bindings in `e2e.rs`
  (`let _cache = isolate_cache();` — the function returns `()`, so the binding
  implies a guard that doesn't exist), `p.join(&keep).exists() == false`
  (`index.rs:438`), 2 collapsible `if`s, 1 manual `is_multiple_of`, 1 derivable
  `Default` (`KeywordOptions`).
- **`unsafe` audit.** Six sites, all sound but none documented as such:
  three `Mmap::map`, three `from_raw_parts` transmutes of `&[u8]` to `&[f32]` /
  `&[i8]` (`index.rs:736,743`, `bm25.rs:204,207`). The f32 one assumes 4-byte
  alignment of a page-aligned mmap — true, but unstated. Each deserves a
  `// SAFETY:` line; the two `bytemuck_cast_*` helpers are named after a crate
  the project doesn't depend on, which is actively misleading.
- **Comment density.** Roughly 1 comment line per 3 code lines in `search.rs`
  and `index.rs`. The content is high-value (measured rationale), but blocks of
  5–10 lines inside function bodies are doing a function's job. Examples:
  `search.rs:313-317` (HNSW load cap, 5 lines → `fn hnsw_worth_loading`),
  `index.rs:569-575` (entry naming, 6 lines → `fn entry_label`),
  `search.rs:471-475` (pre-fusion rationale, 5 lines → a `RESEARCH.md` pointer).
