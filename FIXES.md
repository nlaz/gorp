# Reorganization ledger

Every defect found during the reorganization (`AUDIT.md`, `PLAN.md`), how it was
found, and what it cost to fix. Kept because the *how it was found* column turned
out to be the useful one: nine of these thirteen were invisible to reading and
surfaced only when something mechanical was pointed at them.

Also records the wrong turns. Four diagnoses in here were mine and were wrong,
and two of them I had already written into a comment as fact.

---

## Bugs fixed

### 1. BM25 scores were not reproducible — P0
`Bm25Index::query` accumulated per-document scores in `HashMap` iteration order,
and `tokenize_doc` drained a `HashMap` to assign term ids. Rust seeds hash maps
per process, and f32 addition is not associative, so the same query scored the
same document differently run to run. The last bits are cosmetic; the consequence
was that any two near-tied chunks could swap rank between runs.

**Found by** the snapshot tripwire failing to match output it had recorded ninety
seconds earlier. Not visible to any test that existed, and not visible to reading.
**Fixed by** accumulating in term order and building the term table sorted.
**Guarded by** `scores_do_not_depend_on_term_order`,
`identical_corpora_serialize_identically`.

### 2. The e2e suite was racy — P0, hardened P3
Every test shares one process-wide cache directory, and the budget test set
`SEMGREP_CACHE_MAX_BYTES=0` then evicted every other test's entries mid-run.
Failures surfaced as `repair should report the drift` — a test-isolation bug
wearing a repair bug's clothes, which would have been blamed on the refactor.

**Found by** running the suite eight times instead of once.
**Fixed by** serializing the cache tests, and by `enforce_budget_with_cap`, which
takes its thresholds as arguments so a test never mutates the environment.
**Not fully fixed:** see *Open* below.

### 3. Concurrent cache builds race — P0 (narrowed P2, still open)
Parallel processes building an entry for the same scope produced an occasional
zero-hit run.

**Found by** the new CLI tests, which spawn real processes in parallel.
**Narrowed by** the publication ordering in #6. **Not closed:** see *Open*.

### 4. Interrupted builds leaked unreclaimable cache entries — P2 (audit B2)
`write_cache_entry` created the directory, built into it, and wrote `root.txt`
last. Every enumerator skips a directory without `root.txt`, so an entry orphaned
by Ctrl-C during a first ranked search was invisible to `cache --status` and
untouchable by `cache --prune` — forever.

**Found by** reading, then reproduced: a 5 MB orphan reported as "1 entries,
6.5 KB", with `--prune` reclaiming 0 B.
**Fixed by** writing `root.txt` first, which makes an entry countable without
making it discoverable, plus a reclamation rule for entries registered but never
published (told from builds in flight by age).
**Guarded by** `interrupted_build_leaves_a_reclaimable_entry`,
`a_build_in_flight_is_not_reclaimed`.

### 5. The budget enforcer could not see the entry that triggered it — P2 (audit B3)
Reclamation ran before `root.txt` existed, so the entry just built was invisible
to it. A corpus larger than the whole budget evicted every *other* entry and then
sat over the cap.

**Found by** reading the ordering in #4. **Fixed by** reclaiming after
registration.

### 6. Index publication was not atomic — P2 (audit B4)
`meta.json` was written before `chunks.bin` and `bm25.flat`, but discovery keys on
`meta.json` alone. A concurrent reader could find the entry, fail on artifacts
that did not exist yet, and — a cache load failure being a miss —
`remove_dir_all` the directory the builder was still writing into.

**Found by** reading. **Fixed by** writing `meta.json` last, so writing it is what
publishes an index, and removing it before a rebuild begins.
**Guarded by** `an_index_is_invisible_until_it_is_complete`.

### 7. Searching wrote into a repo-local `.semgrep/` — P2 (audit B5)
Read-repair touched `last_check` inside the index directory on every validation,
including a committed `.semgrep/`. A read-only query dirtied a tracked directory.

**Found by** reading. **Fixed by** putting the marker under the cache for
repo-local indexes; cache entries keep theirs, where it doubles as the LRU access
time. **Guarded by** `searching_does_not_write_into_a_repo_local_index`.

### 8. Cold and warm BM25 numbered terms differently — P3a
`Bm25Index` assigned term ids by first appearance, `FlatBm25` by position in its
sorted table. Term-id order drives the score accumulation, so the cold path and
the warm path could return a near-tied pair in different orders.

**Found by** the store-parity property test, on its first run. The fixture test it
replaced had a 1e-5 tolerance that was hiding exactly this.
**Fixed by** renumbering terms into sorted order in `finalize`, making the two
stores structurally identical rather than approximately equal.
**Measured:** BM25 cold/warm disagreements 8/18 → 2/18.

### 9. One unreadable file NaN'd an entire reranked head — P5a
MaxSim scores a candidate whose file cannot be read as `NEG_INFINITY`. That value
went into a min-max normalization, making `lo` infinite, `span` infinite, and every
normalized value `(x + inf)/inf` — NaN. A single missing file destroyed the
ordering of the whole reranked head.

**Found by** writing a test for the unreadable-candidate case while extracting
`blend_head`. **Confirmed** against the pre-refactor arithmetic with a standalone
probe before fixing, to be sure it was not an artifact of the extraction.
**Fixed by** excluding non-finite values from the range and appending unscorable
rows behind everything that could be scored.
**Reachable only via `--maxsim`**, which is why no eval run caught it.
**Guarded by** `an_unreadable_candidate_sinks_without_poisoning_the_head`.

### 10. Chunk parameters were not part of cache identity — P6 (audit B1)
Entries were keyed by canonical root alone, and nothing compared a query's params
against the entry serving it. One search with a non-default `--window` wrote an
entry that every later search of that scope was served from, silently returning
spans of the wrong size.

**Found by** reading, then reproduced against the binary before and after:

```
$ semgrep --window 8 --overlap 2 "retry backoff" .
{"start_line":79,"end_line":84,...}
$ semgrep "retry backoff" .            # default window is 32
before: {"start_line":79,"end_line":84,...}   <- still 8-line spans
after:  {"start_line":73,"end_line":84,...}   <- its own entry
```

**Widest blast radius of the audit.** The eval harness sweeps `--window` against
the same cache ordinary use has, so a tuning run contaminated whatever was
measured next — and every §9 lever number in `RESEARCH.md` was produced under it.
**Fixed by** recording params in `params.txt` and in the entry directory name,
filtering discovery on them, and making scope promotion respect them.
**Guarded by** `chunk_params_are_part_of_a_cache_entry_identity`,
`scope_promotion_spares_entries_built_with_other_params`.

### 11. Cold and warm ranked by different arithmetic — P6
The cold path scored full-precision cosine over f32 embeddings; the warm path
scores i8 dot products over the quantized matrix. Different numbers, so different
orders among near-ties. **37 of 54 query/mode pairs disagreed.** The parity test
that existed compared `hits[0]` only and could not see any of it.

**Found by** measuring cold-vs-warm agreement across the whole snapshot rather
than trusting the single-hit assertion.
**Fixed by** having the cold path quantize exactly as `store::build` does.
**Measured:** 37/54 → **0/54**. "The index is a cache" became an equality.
**Guarded by** `cold_and_warm_return_identical_results`, over the full top-k.

### 12. The repair overlay was inconsistent with its own base — P7
The overlay stored f32 embeddings and scored cosine while the base it merges into
stores i8 and scores dot products — the same mistake as #11, inside the warm path.

**Found by** the repair-vs-rebuild property test.
**Fixed by** storing delta vectors as i8, produced by the same
normalize-then-quantize the build performs, scored with the same kernel and the
same quantized query. **Measured:** hybrid 47/49 → 48/49 exact.

### 13. The overlay computed corpus statistics over itself — P7 (audit B7)
BM25 idf and average document length are corpus-wide, and the overlay computed
them over its own handful of drifted files, putting its scores on a different
scale from the base list they get merged with.

**Found by** the same property test. **Fixed by** `rank::bm25::Rest`, which lets a
store holding part of a corpus borrow the rest of it.

### 14. The eval harness scored a broken binary as 0.00 — P9 follow-up
`semgrep_search` ignored the subprocess exit code and parsed whatever was on
stdout. Handed a binary that could not answer a single query — an embedding width
that did not match the index — it produced a clean table of `0.00` across 400
queries and three modes, which reads exactly like a real and catastrophic result
rather than a broken setup.

**Found by** running the eval, seeing all zeros, and not believing them.
**Fixed by** raising on any exit code other than 0 (found) or 1 (no match), with
the failing command and stderr attached. The first query now stops the run and
says why.

The same shape as the engine defects above: a fallback that swallows an error and
returns something plausible. It is worth noting that this one would have
mattered most in exactly the situation it arose in — comparing two binaries,
where one silently scoring zero looks like the other one winning.

---

## Dead code removed — P1

The whole f32 embedding path had been unreachable since format v2 began writing
quantized rows, and `load_dir` rejects any other version:
`IndexMeta::{normalized, quantized}`, `LoadedIndex::emb_matrix`,
`rank::dot_distance` (a hand-unrolled SIMD kernel), and the f32
`brute_force_top_k`. Plus `semantic::embed_batch` and `TopK::threshold`, which had
no callers at all, and `grep-matcher`, declared in two manifests and imported
nowhere. About 110 lines.

Two better tests replaced the one that covered the deleted f32 scan:
`block_parallel_scan_matches_naive` (the i8 scan that actually runs) and
`quantized_ranking_tracks_exact_cosine`, which checks the assumption the entire v2
format rests on and that nothing had verified.

---

## Corrections to my own claims

Recorded because three of these were already written down as fact, and the only
reason they did not survive is that something mechanical contradicted them.

1. **"Both BM25 stores agree bit-for-bit."** Written as a comment in P3a while
   unifying the scorer. The property test failed on its first run: the two stores
   numbered terms differently. Corrected in the same commit — the comment now
   explains why they agree, which required making it true first.

2. **"The residual cold/warm divergence is MMR's fault."** Written in the P3a
   commit message. Wrong: MMR contributed, but the ranking arithmetic was the
   cause (#11). Corrected in P6.

3. **First attempt at #11 fixed the wrong layer.** I quantized only the vectors
   MMR sees (`as_stored`) and measured: 37/54 before, 37/54 after. No effect. The
   fix had to be in the ranking, not the reranking. `as_stored` was kept because
   it is still needed for consistency, but it was not the fix.

4. **First attempt at #12 fixed the wrong thing first.** I hypothesized the
   overlay's idf (#13), applied it, and measured: still 3 divergences. The primary
   cause was the f32/i8 representation mismatch. Both were real defects; my
   ordering of them was wrong, and only measuring after each change showed it.

5. **`cache::gen` would not compile.** `gen` is a reserved keyword in Rust 2024.
   Renamed to `cache::compat`.

The pattern in 2–4: I twice diagnosed a numeric divergence from the top of the
stack down, and was wrong both times. Measuring after each individual change —
rather than after a batch — is what caught it.

---

## Open, and deliberately not fixed

1. **Tombstone residual in corpus statistics.** Two of 147 repair-vs-rebuild
   comparisons return the same chunks in a different order. A tombstoned chunk
   stops being served but stays in the base's term table and document count, so
   idf differs slightly from a rebuild's. Subtracting it means walking every
   posting list in a memory-mapped index on every repaired query. The two cases are
   pinned by name in `repair.rs`, so a new divergence fails the test and an
   improvement forces the list to shrink.

2. **Concurrent cache builds (#3) are narrowed, not closed.** Publication is
   atomic now, but two processes can still interleave. Closing it needs a staging
   directory and a rename.

3. **Cache tests are serialized, not isolated.** `cache_base()` latches
   `SEMGREP_CACHE_DIR` in a `OnceLock`, so one process gets one cache and per-test
   isolation is impossible. The real fix is making the cache root an explicit
   parameter, as `enforce_budget_with_cap` did for the budget.

4. **CI cannot run.** It needs read access to the sibling `flowercomputers/ese`
   and `flowercomputers/anny` checkouts — a `BOG_TOKEN` secret, or those repos made
   public.

5. **P6 changed retrieval behavior and is not eval-validated.** 237 cold-path hit
   lines moved. It should be strictly better, since cold now matches warm, but
   `eval/run_eval.py` has not been run across the change. Given that #10 means the
   existing lever numbers were measured through a contaminated cache, the eval
   deserves a fresh run regardless.
