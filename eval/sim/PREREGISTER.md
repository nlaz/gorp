# Simulation testing: pre-registered predictions

Written **before** `eval/sim/results/` held a single byte, and committed as its
own change. Precedent: `b56bc5b`, "eval: pre-register the rg-oracle prediction
before running it."

The reason is narrow and specific. Every prediction below was derived by reading
the code, and reading code is exactly how you convince yourself of something
false. If the predictions are written afterwards, a run that contradicts them
gets quietly reinterpreted, and the report becomes a description of whatever
happened. Written first, three outcomes are distinguishable:

- **pass** — predicted and observed agree.
- **fail** — the code does something the prediction says it should not. A bug, or
  a documented-but-unwanted behavior.
- **surprise** — a predicted *failure* that did not occur, or any observation
  outside the prediction's range. **This is the most valuable verdict**: it means
  the mental model of the code was wrong, which is worse than a bug because
  everything else reasoned from that model is now suspect.

`eval/sim/scenarios.py` holds these same expectations in machine-readable form;
this file is the prose. If they disagree, `scenarios.py` is what ran.

## What is being tested, and why it needs a new harness

`eval/` measures retrieval quality on single queries. `bench/` measures speed on
single queries. Neither observes **behavior over a sequence of steps against
evolving state** — and the index is a cache (RESEARCH.md §8), so almost
everything structurally interesting only appears across several invocations
where step N changes what step N+1 sees: write-through on first query,
read-repair, the TTL gate, LRU eviction, generation churn, corrupt-entry
disposal.

A **session** is an ordered sequence of steps against one corpus root under one
isolated `SEMGREP_CACHE_DIR`. A **step** is `mutate` (the world changes),
`invoke` (one semgrep process), or `assert` (an expectation below, evaluated
against everything observed so far).

## Conditions common to every scenario

- `SEMGREP_CACHE_DIR` is a per-session temp dir, and its path is printed, so a
  run is auditable and no scenario can be answered from another's entry.
- `SEMGREP_CACHE_TTL_SECS` is **pinned explicitly** in every scenario, never
  left at the 60 s default.
- Every mutation either changes file size or crosses a second boundary.
  `FileMeta.mtime` is whole seconds (`lib.rs:57`) and `corpus::diff` compares
  `(size, mtime)`, so a same-second same-size edit is invisible. That is a
  flakiness source for every other scenario and the finding of S3d.
- Tier 1 = synthetic corpora + the frozen `crates/semgrep-core/tests/corpus`
  fixture. Tier 2 = `tokio`, `etcd`, `commons-lang`, `jekyll` (166–1,500 files).
  The 84k-file kernel is **out of scope** for this run by decision; predictions
  are therefore not made about kernel-scale numbers.
- `python3 bench/manifest.py --check` runs before and after. A simulation that
  mutates a bench corpus and fails to restore it would silently invalidate every
  published benchmark, so the digest check is part of the protocol, not a nicety.

---

## S1 — Cold start / write-through

**Setup.** Fresh cache dir, one hybrid query against an unindexed root.

**Predict.**
1. `path_taken == "ColdWriteThrough"`. Note this is *not* "the cold pass is
   persisted": `search/mod.rs:156-166` builds first, then re-discovers, then
   answers *warm*. The streaming path runs only under `--no-index` or a
   still-missing discovery.
2. `Bucket::Build` ≥ 90% of `total_ms` on tier-2.
3. `discover_calls == 3` for a search that misses.
4. The build's internal split (walk / chunk+tokenize / embed / write) is
   reported for the first time — `store::build` has no `Trace` today and
   `BuildStats` carries counts only.

## S2 — Warm-session amortization

**Setup.** 20 sequential queries from `eval/queries/tokio.jsonl`, TTL=3600.

**Predict.**
1. Queries 2–20 fall within 2× of each other (no per-query growth).
2. `repair == TtlFresh` for 19 of 20.
3. **`load:*` is re-paid in full on every query** — predicted > 60% of warm
   query time on tier-2. This is the process-per-query architecture's core cost
   and the strongest argument for the persistent-server item on the roadmap.

## S3 — Drift, read-repair, and the TTL gate

**S3a — TTL fresh, files edited.** TTL=60, edit files, query immediately.
*Predict:* `repair == TtlFresh`, `stale_files == 0`, and **stale text is
served**. Correct by design (the throttle exists because a staleness walk costs
~1 s on 84k files against a ~135 ms query), but the trajectory should show a
user being shown a function that no longer exists.

**S3b — TTL=0, no drift.** *Predict:* `repair == NoDrift`, `repair:walk > 0`,
`repair:delta == 0`.

**S3c — TTL=0, 3 files edited.** *Predict:* `repair == Repaired{modified: 3}`,
hits reflect the new text, and **the entry on disk is unchanged** — repair never
writes back, so the same work is redone on the next query past the TTL.

**S3d — same-second, same-size edit.** Write a file, immediately overwrite one
character in place (preserving length), query with TTL=0.
*Predict:* **FAIL** — `repair == NoDrift`, stale text served, `stale_files == 0`,
because `corpus::diff` sees identical `(size, mtime)`. A real hazard for an
agent that edits and immediately re-searches.

## S4 — The branch-switch delta cliff ⭐ *(predicted headline finding)*

**Setup.** Build an entry, then drift 0 / 1 / 5 / 10 / 25 / 50 / 100% of files,
TTL=0, and run **10 identical repeated queries at each drift level**.

**Predict.**
1. `repair:delta` grows linearly in drift fraction — nothing in
   `cache/repair.rs` bounds `drift.len()`; the loop at `:115` iterates every
   stale path unconditionally, re-reading, re-chunking, re-tokenizing and
   **re-embedding** each one.
2. **Query 10 costs the same as query 1 at every drift level.** The overlay is
   never written back and coverage never grows, so there is no amortization —
   ever — between rebuilds.
3. At ≥50% drift on tier-2, per-query cost **exceeds a full rebuild**. The
   crossover — where repairing is more expensive than rebuilding — is predicted
   to lie between 5% and 25% drift.

RESEARCH.md §8 mechanism 2 specifies exactly the missing guard ("if the delta
exceeds a threshold, say >5% of files — branch switch — treat the whole query as
a miss"). It was never implemented. This is predicted to be the largest
performance finding of the exercise.

## S5 — Cache budget and LRU

**S5a — thrash.** `SEMGREP_CACHE_MAX_BYTES` = 1.5× one entry; query 4 corpora
round-robin ×3. *Predict:* every query cold, 12 full builds, evictions on nearly
every write.

**S5b — eviction failure ⭐.** `chmod 0500` an entry directory so
`remove_dir_all` fails, then trigger the enforcer.
*Predict:* verified by reading `cache/budget.rs:115-122` — the victim is
`pop()`ed from `entries` *before* the delete is attempted, and `total` is only
decremented **inside** the `is_ok()` branch. So a failed delete does not stop the
loop; it keeps popping and **destroys healthy entries** while the undeletable one
survives, and the loop exits with the cache still over budget and no error
anywhere. Two defects in five lines: over-eviction of good entries, and a silent
budget violation.

Also probe `dir_bytes` (`budget.rs:40-46`), which is non-recursive — plant a
subdirectory inside an entry and *predict* its bytes are counted as zero.
Correct only by accident today, since entries are flat.

## S6 — Generation churn

**Setup.** Plant a sibling generation directory with a fake entry; query
warm-only; then force a cold write.
*Predict:* the sibling survives every warm query (`gc_old_generations` is never
called on a read path, `cache/compat.rs:92-94`) and is reclaimed only on a cold
write or `cache --prune`. Correct, but a user who only ever queries warm
accumulates dead generations indefinitely.

## S7 — Adversarial corpus

**Setup** (synthetic, built by `eval/sim/corpus.py`): binary file with NULs; a
file over `max_file_bytes`; an empty file; a `chmod 000` file; invalid UTF-8;
UTF-16 with BOM; a symlink loop `a → b → a`; a symlink escaping the root;
filenames containing a **newline**, a `:`, a quote, and emoji; a 200k-line
single-line file; a FIFO; a `.gitignore`d subtree; a directory named `.semgrep`.

**Predict.** No panic and no hang anywhere — the `ignore` crate handles symlink
loops, `corpus::read_text` NUL-sniffs the first 8 KiB, and oversized files are
dropped at walk (`corpus/mod.rs:39-41`).

**One predicted FAIL:** `out::hits` (`crates/semgrep/src/out.rs:42`) writes a
bare `println!("{}:{}:{}", path, line, text)` with no escaping or quoting. A hit
in a file whose name contains a newline or a `:` therefore **breaks the
`path:line:text` stdout contract** — the documented, grep-compatible format that
the whole "stdout is data" rule rests on. `--json` survives, because serde
escapes.

Secondary: a `chmod 000` file is silently skipped by `corpus::read_text`'s
`.ok()?` and appears in no report, no warning, and no count.

## S8 — Fault injection on a cache entry ⭐⭐ *(sharpest prediction)*

Build an entry, corrupt one artifact, then query **three times consecutively**.
The third query is the point: it tests whether the damage is self-clearing.

**S8a — `bm25.flat` truncated to exactly 64 bytes.**
The header is 8 (magic) + 4 (`n_docs`) + 4 (`n_terms`) + 8 (`total_len`) + 5×8
(section offsets) = **exactly 64 bytes**. So `FlatBm25::open`
(`store/bm25.rs:85`) validates `map.len() >= 64 && magic` and **passes**, then
populates `off[]` with the real — now wildly out-of-range — offsets, which are
used as *unchecked* slice indices in `term_at` (`:113`), `u32_le` (`:104`),
`postings_at` (`:136`) and `doc_len_at` (`:146`).

*Predict:*
1. **PANIC** — exit 101, index-out-of-bounds — for `--mode bm25` and
   `--mode hybrid`.
2. `--mode semantic` **survives**, because `load_needs` (`indexed.rs:84`) does
   not request bm25.
3. Because it is a panic and not an `Err`, the disposability contract at
   `search/mod.rs:173-181` — which deletes a bad entry and falls back to
   streaming — **never fires**. The entry is never evicted, so **all three
   consecutive runs panic identically**: a permanently bricked cache, recoverable
   only by `semgrep cache --clear`.

This is the one artifact in the format that is not size-checked. `emb.bin` is
(`store/load.rs:97`), `chunks.bin` is via postcard, `meta.json` via serde.

**S8b — garbage offsets.** Write `0xFFFF_FFFF_FFFF_FFFF` into the five slots at
bytes 24..64. *Predict:* the same panic, possibly SIGSEGV from a wild mmap index.

**S8c–g — every other artifact.** `emb.bin` deleted; `emb.bin` truncated by one
byte; zero-length `meta.json`; truncated `chunks.bin`; garbage `hnsw.bin`.
*Predict:* clean `Err` → entry evicted → cold fallback → **correct results**, in
every case. Note `meta.json` stays *discoverable* (`cache/mod.rs:94` checks only
`is_file()`), so predict one wasted discovery per query until eviction lands.

**S8h — the same corruptions in a repo-local `.semgrep/`.** *Predict:* the error
propagates to the user (exit 2) rather than degrading, by design — a repo-local
index is an explicit artifact, not a disposable cache. Except a/b, which panic
identically because a panic does not care about that distinction.

## S9 — Concurrency

**Setup.** 20 trials × 8 parallel first-searches of one fresh scope, `--json`.

**Predict.** FIXES #3 is open and narrowed, not closed. *Predict* ≥1 trial in 20
produces a zero-hit or nonzero-exit run. **The rate has never been measured** —
reporting it is the point, since it decides whether the staging-dir + rename fix
is urgent or merely correct.

Secondary: `cache_entry_dir`'s 64-slot collision probe (`cache/mod.rs:141-147`)
is a TOCTOU — two processes probing simultaneously can pick the same free slot.

## S10 — Flag/mode matrix and the cold==warm invariant

**Setup.** `{bm25, semantic, hybrid} × {cold, warm} × {diversify, hnsw, prf 0/8,
maxsim} × k ∈ {1, 10, 50, 500}`, hit lists compared field-wise.

**Predict.** Parity holds everywhere **except `--prf 8`**: `expand_query` exists
only in `indexed.rs:127` and has no counterpart in `stream.rs`, so a PRF query
answers differently depending on whether the scope happens to be cached. This is
the same class of defect as the MaxSim cold-path break fixed in `4356f9b` —
which is *excluded* from this prediction because it is already fixed.

Secondary: `-k 500` forces `pool > 128`, which sets `used_hnsw = false`
(`indexed.rs:89`) even when `--hnsw` was requested. Correct, but silent.

## S11 — Narrow scope

**Setup.** Index a whole corpus, then query a deep subdirectory holding <1% of
chunks. Sweep scope narrowness.
*Predict:* fewer than `k` hits are returned for narrow scopes, because
`candidates()` (`indexed.rs:261-278`) filters to the subtree *before*
truncating, but the fused list it filters is only `FUSION_POOL * 2 = 256` rows
wide and may contain zero in-scope rows. Find the narrowness at which it breaks.

## S12 — The exact-miss double search

**Setup.** `-e zzz_no_such_symbol` against an indexed scope.
*Predict:* **two** trace envelopes sharing one `query_id` (`phase: "primary"`
and `phase: "suggest"`), `discover_calls == 3`, and process wall ≈ a keyword
scan **plus a full hybrid query** — `cmd/search.rs:106` runs an entire second
`search()` to produce the suggestion. Invisible in `--stats` today.

## S13 — Keyword at scale

**Setup.** A high-frequency pattern, with and without `--all`, `--json` and not.
*Predict:* keyword mode reports a stage schedule for the first time (it returns
`SearchReport::default()` today). The 250-hit cap is *print*-only
(`cmd/search.rs:23`) while the full hit vector is materialized, so predict
`finalize` cost and RSS proportional to **total** matches, not to 250.

---

## Scope of what a passing run would and would not show

These scenarios exercise cache and I/O behavior under sequences. They say
nothing about retrieval quality — `eval/` owns that — and nothing about
kernel-scale performance, which is deliberately out of this run. A scenario that
passes shows the predicted behavior occurred on tier-1/tier-2 corpora on one
machine; it does not show the behavior is correct at other scales.
