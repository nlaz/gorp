# Reorganization ledger

Every defect found during the 2026-07 reorganization, how it was found, and
what it cost to fix. Kept because the *how it was found* column turned
out to be the useful one: nine of the original thirteen were invisible to
reading and surfaced only when something mechanical was pointed at them. The
ledger has kept growing since; it now runs to #26.

Also records the wrong turns. Four diagnoses in here were mine and were wrong,
and two of them I had already written into a comment as fact.

---

## The single-file scope: a search that answered nothing and said it succeeded

**Symptom.** `gorp "query" <path/to/file.py>` returned zero results and
exit 1, in every ranked mode, cold and warm. Exact mode returned hits but
printed them as `:9:text` — no filename.

**Cause.** `corpus::walk` computes a file's corpus-relative path as
`entry.path().strip_prefix(root)`. When `root` IS the file, that strips to
the empty string. Four independent sites then did `root.join(rel)` and looked
for `<file>/<file>`: `abs_path` (chunking read nothing), `chunk::lines`
(MaxSim/PRF re-read), `hit::materialize` (hits silently dropped), and the
keyword walk (empty display path).

**Why it survived.** Every test used a directory as the corpus root — the
natural thing when you write the tests. No test, and neither adversarial
review, used the scope an *agent* uses for a follow-up query. The failure was
also silent: exit 1 is indistinguishable from "no matches", so an agent (and
a benchmark) reads it as a legitimate empty result.

**Blast radius.** 47% of the semantic arm's searches in the §16.10 campaign
(1,610 of 3,434), 61% of instances. The result survived it — instances that
hit the bug show semantic +0.012, those that did not show −0.014, both noise
in opposite directions (RESEARCH.md §16.11) — but it plausibly explains part
of that campaign's 27% cost premium.

**Fix.** One `corpus::resolve(root, rel)` helper owns the join and returns
`root` when root is a file; all four sites call it. Keyword path fixed
alongside. Regression test: `a_single_file_scope_returns_hits_in_every_mode`.

**Prevention.** `../gorp-bench/harness/locbench/preflight.py` replays real agent invocation
shapes (harvested from the logs, including file scopes) against a fixture and
fails the campaign before it spends money. Verified against the pre-fix
binary: it flags all four failure modes, including "12 of 25 real invocation
shapes returned nothing".

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
`GORP_CACHE_MAX_BYTES=0` then evicted every other test's entries mid-run.
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

### 7. Searching wrote into a repo-local `.gorp/` — P2 (audit B5)
Read-repair touched `last_check` inside the index directory on every validation,
including a committed `.gorp/`. A read-only query dirtied a tracked directory.

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
$ gorp --window 8 --overlap 2 "retry backoff" .
{"start_line":79,"end_line":84,...}
$ gorp "retry backoff" .            # default window is 32
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

### 15. A corrupt `bm25.flat` panicked instead of erroring, permanently — P0
`FlatBm25::open` validated `map.len() >= 64` and an 8-byte magic, then read five
section offsets out of the header and used them as *unchecked* slice indices in
`term_at`, `u32_le`, `postings_at` and `doc_len_at`.

64 is exactly the header's own length (8 magic + 4 + 4 + 8 + 40 offset slots), so
a file truncated to precisely its header **passed validation** and handed out
five offsets pointing past the end of the file.

The panic is the bug, not the corruption. `search::search` treats any failure to
read a cache entry as a miss — it deletes the entry and answers from the
streaming path — but only on `Err`. A panic bypasses that, so the entry was never
evicted and *every subsequent invocation panicked on the same bytes*. A cache
that only `gorp cache --clear` could recover.

Found by simulation testing (SIMULATION.md §1.1), which was pre-registered to
predict exactly this and measured it as three consecutive runs at exit 101 with
zero hits, on all three corpora, for three different corruptions. It was the only
crash class in 660 invocations, and accounted for all 30 of them.

Fixed by validating in `open` that every section named by the header lies inside
the file, returning `Err`. Deliberately O(1): the three fixed-size tables are
bounds-checked directly and the two byte blobs against the last entry of their own
table. Checking that every offset *within* those tables is monotonic would mean
reading tens of megabytes at kernel scale and undo the reason the flat layout
exists — so the accessors are bounds-safe instead, and an in-bounds but
internally inconsistent table yields empty results rather than a crash.

`tests.rs` now truncates a real index at every byte offset from 0 to its length
and asserts each prefix either errors or answers, never panics. A spot check
would have missed this: the bug lived at one specific length.

### 16. Concurrent first-searches SIGBUS'd — P0
`store::build_at` wrote `emb.bin` straight into the live entry, truncating a file
another process had already `mmap`ed. A mapping whose backing file is truncated
faults on access, and a fault is a signal, not an error anyone can catch.
`unpublish()` removed `meta.json` first to stop a *new* reader starting, but it
could not recall a mapping that already existed.

A build now assembles itself in a `.building-<pid>` sibling and is published by
two renames — old entry aside, staging into place — so **no published file is
ever mutated**. A reader mid-query keeps the inodes it opened and finishes
against a complete, consistent index; only the directory entry changes.
`unpublish` is gone, being exactly the thing the swap makes unnecessary.

`cache_entries` skips the transient directories (a finished-but-unswapped staging
dir holds a valid `meta.json` and would otherwise be discoverable) while
`cache_status` keeps counting them, so an interrupted build is still reclaimable —
the property #4 established.

Measured with the harness that found it (`eval/sim`, s9): **4 of 20 trials and 11
of 160 processes at exit −10 before, 0 of 20 and 0 of 160 after**, holding across
four repetitions. Closes the open item that had been narrowed but not shut since
#3.

### 17. Read-repair had no delta bound — P1
`repair.rs` iterated every stale path unconditionally, re-reading, re-chunking and
**re-embedding** all of them, and never wrote back — so past ~50% drift a warm
query cost more than throwing the cache away, on every query, forever.

RESEARCH.md §8 mechanism 2 had specified the guard ("say >5% of files — branch
switch"); it is now implemented as `SearchOptions::repair_max_drift`, default
0.05. Above it, `indexed::run` raises a typed `DriftTooLarge` and `search`
**rebuilds** the entry rather than streaming around it — streaming answers one
query and keeps nothing, while the rebuild makes every query after it warm, which
is the entire argument for having a threshold.

A plain option rather than an env var behind a `OnceLock`: latching a tunable per
process is what makes `cache_base` untestable (open item 3 below), and a
threshold with no test that crosses it is not a threshold. The CLI reads
`GORP_REPAIR_MAX_DRIFT` into it so the sim can still sweep.

Measured on tokio, against SIMULATION.md §1.3's own table:

| drift | before, *every* query | after, 1st | after, steady |
|---:|---:|---:|---:|
| 5% | 35.1 ms | 163.5 ms | **8.2 ms** |
| 25% | 90.6 ms | 162.4 ms | **8.4 ms** |
| 50% | 131.1 ms | 168.1 ms | **8.2 ms** |
| 100% | 196.9 ms | 161.2 ms | **8.9 ms** |

The retry after a rebuild runs with the bound off. That is not belt-and-braces:
a scope the root walk excludes does not gain rows by rebuilding the root, so
re-raising would charge a build *and* a stream on every query. A hidden directory
is the case that found it.

### 18. A write could evict the entry it had just written — P1
`write_cache_entry` ran `enforce_budget()` before returning, and under a cap below
one entry the LRU evicted what it had just been handed; the query then missed on
re-discovery and streamed the corpus as well, paying twice and keeping nothing
(5 of 8 queries under budget pressure). `enforce_budget_protecting` spares the new
entry; if it alone exceeds the cap it survives this call and is evicted by the
next write like anything else.

### 19. LRU eviction destroyed healthy entries when a delete failed — P2
`budget.rs` popped the victim whether or not `remove_dir_all` succeeded and
decremented the running total only on success, so one undeletable directory made
the loop chew through every healthy entry behind it — four entries in, one out,
and the survivor was the undeletable one, at exit 0 with no warning. The loop now
stops on a failed delete and reports it: an entry that will not delete is a
permissions anomaly, not ordinary pressure, and pressing on trades the whole
cache for nothing.

`enforce_budget` returns a `Reclaimed { removed, freed, stuck }` rather than a
pair, because `gorp-core` does not print — every word the user sees is written
by the CLI's `out`, which is what keeps "stdout is data, stderr is commentary"
checkable in one place. `dir_bytes` also recurses now; it was correct only
because entries happen to be flat.

### 20. `out::hits` did not escape paths — P2
`println!("{}:{}:{}")` meant a filename containing a newline split one hit across
two stdout lines (six files on disk, seven lines out) and one containing `:`
mis-parsed silently. `quote_path` now C-quotes a path containing a control
character, a quote, or a colon, and leaves every other path byte-identical — so
the common case does not get noisier and `tools/snapshot.sh` does not move.
Applied at all three writers of a path: `out::hits`, `out::context`, and the
`-e`-miss suggestion lines. `--json` was already correct and is unchanged.

### 21. A nonexistent search path exited 1, not 2 — P2
"No results" is what an agent reads as *the code is not there*, when the path was
simply wrong; ranked mode additionally announced it was caching a scope that did
not exist. `cmd::search` now checks the path before anything else and bails,
which the existing error path renders as exit 2 with a reason. `exists`, not
`is_dir`, because a single file is a legitimate scope the streaming path handles.

### 22. Every warm ranked query resolved the index twice — P2
`warn_if_first_search` called `cache::discover` on *every* ranked search — a full
canonicalization and generation scan — purely to decide whether to print one
line, and then the engine resolved the same scope again. The notice now comes
from the engine via `SearchOptions::on_first_search`, fired at the point a build
becomes certain. Warm queries resolve once; a ranked cold miss is two rather than
three. The exact-miss path still resolves twice of its own, which is
`suggest_ranked_alternatives` not reusing what it already resolved — a separate,
smaller thing, and pinned as such.

### 23. A narrow scope could return zero hits — P2
`candidates()` filtered to the query's subtree correctly, but filtered a fused
list only `FUSION_POOL * 2` = 256 rows wide, so a scope holding none of the
corpus-wide top 256 got nothing. `Rows` now materializes an in-scope mask once
and exposes `serves(id) = live(id) && in_scope(id)`, threaded into the BM25
accumulation (`top_k_scoped`) and the brute-force scan
(`brute_force_top_k_i8_where`) so the filter runs **before** truncation. A scoped
query also gets faster, since rows that cannot be returned never reach the
kernel. HNSW is skipped under a scope: the graph returns its own bounded list and
cannot be told to skip rows, which is the same mistake again.

On tokio, `docs` 0 → 10 hits, `benches` 5 → 10, `examples` 3 → 10. Whole-corpus
ranking is unchanged (the mask is `None`), which `tools/snapshot.sh` confirms.

**A correction to SIMULATION.md §1.7 while fixing this.** It reported `.github`
and `docs` together as one finding. Only `docs` was. `corpus::walk` runs
`ignore::WalkBuilder` at its default `hidden(true)`, and tokio's index contains
**no dot-prefixed path at all** — there was never anything under `.github` to
starve, and no scope filter can fix that. Pinned in
`a_hidden_subtree_is_absent_from_its_parents_index_but_searchable_on_its_own`.

### 24. The walk was serial — perf
`corpus::walk` drove `ignore::WalkBuilder::build()`, one thread. It is now
`build_parallel()`, and **the terminal sort is what makes that safe**: chunk ids
are assigned in walk order, but walk order is defined by that sort rather than by
traversal order, so concurrent traversal changes nothing downstream. Removing the
sort would silently break the index format, which is why it is stated in the
docstring.

Worth doing because the walk is not only a build cost — read-repair walks the
scope on every warm query past the TTL:

| corpus | files | before | after |
|---|---:|---:|---:|
| jekyll | 806 | 7.1 ms | 5.3 ms |
| tokio | 865 | 6.0 ms | 5.2 ms |
| vscode | 4,389 | 43.2 ms | 19.3 ms |
| linux | 84,265 | **1,735 ms** | **272 ms** |

No small-corpus regression, which was the risk. Verified identical: the parallel
walk reproduces the 846-file table of a tokio index built by the pre-change
binary, in the same order, and twenty repeated walks are byte-identical.

### 25. The citation guard could not tell one document from another — P3
`tests/docs.rs` scraped every `§N` out of the source and checked it against
RESEARCH.md whatever document the comment named. So `SIMULATION.md §1.1` passed
only because RESEARCH.md happens to have a §1.1, and a correct citation of a
section RESEARCH.md lacks would have failed — §1.3 through §1.7 exist only in
SIMULATION.md. Wrong in both directions, and passing. Citations are now
attributed to the document named nearest before them, defaulting to RESEARCH.md.

### 26. Nothing bounded how wide a result line could be — P1
`EXACT_PRINT_CAP` bounded how many lines printed and nothing bounded how long
one could be, so between them they bounded nothing. Measured over the 366
searches of the first post-§16.11 campaign, **23 lines longer than 1,000
characters carried 73% of every byte the tool printed**: a `-k 5` ranked search
for `add_code` returned 659 KB because its top hit was a single-line 374 KB JSON
fixture, `bleach sanitize html` returned 121 KB of `bootstrap.min.css`, and
`-e equation` had returned 12.5 MB the same way a day earlier.

Cost was the smaller half. The caller's tool-result limit truncates what it is
handed, so one minified line **deleted the hits ranked beneath it** — the reply
was wrong, not merely expensive, and wrong in a way that looks like a thin
result rather than a truncated one. Generated files were also *winning top
slots* over real code, which is a ranking observation this fix does not
address (`../gorp-bench/harness/locbench/replay.py` can score a down-weight offline).

`out::hits` — already the single writer of `path:line:text` — now strips each
line's indentation and clips it to `MAX_COLUMNS` (200, over ripgrep's
`-M/--max-columns` spelling and its `[... omitted end of long line]` wording),
uniformly across ranked, exact and `--json` so the three cannot drift. `-M 0`
restores the old behavior. Replayed over the same 366 searches: 75% fewer bytes
and a worst case of 2.5 KB instead of 659 KB.

Indentation is dropped because it is the one part of a line that is pure
position — already carried by the line number, and reconstructible from the file
the hit names. `-C` is the exception: its whole purpose is showing the shape of
the surrounding code, so a framed block is dedented by the indentation its lines
*share* rather than each line's own. The first attempt dedented by the hit
line's indentation, which flattened every block, because the matched line is
usually the deepest one in it — `context_dedents_the_block_without_flattening_it`
is the guard.

---

## Open, and deliberately not fixed

Numbering is stable — source comments cite `FIXES.md open #3` by number — so
items that have since been closed are struck through in place rather than
removed, with the entry that closed them. Genuinely open today: #1, #3, #4.

1. **Tombstone residual in corpus statistics.** Two of 147 repair-vs-rebuild
   comparisons return the same chunks in a different order. A tombstoned chunk
   stops being served but stays in the base's term table and document count, so
   idf differs slightly from a rebuild's. Subtracting it means walking every
   posting list in a memory-mapped index on every repaired query. The two cases are
   pinned by name in `repair.rs`, so a new divergence fails the test and an
   improvement forces the list to shrink.

2. ~~**Concurrent cache builds (#3) are narrowed, not closed.** Publication is
   atomic now, but two processes can still interleave. Closing it needs a staging
   directory and a rename.~~ **Closed by #16** — builds go to
   `store::staging_path` and publish by rename; `unpublish` is gone.

3. **Cache tests are serialized, not isolated.** `cache_base()` latches
   `GORP_CACHE_DIR` in a `OnceLock`, so one process gets one cache and per-test
   isolation is impossible. The real fix is making the cache root an explicit
   parameter, as `enforce_budget_with_cap` did for the budget.

4. **CI cannot run.** It needs read access to the sibling `flowercomputers/ese`
   and `flowercomputers/anny` checkouts — a `BOG_TOKEN` secret, or those repos made
   public.

5. ~~P6 is not eval-validated.~~ **Done.** Paired A/B of the pre-refactor commit
   (`8f0bdfb`) against the merged tip, vscode corpus, isolated caches, index
   rebuilt per binary:

   - **Warm path, 400 queries x 3 modes: identical.** All 21 metrics `+0.000`,
     every sign test 0-0. The path the product actually uses is bit-for-bit
     unchanged by the reorganization.
   - **Cold path, 60 queries x 3 modes:** BM25 and hybrid identical; semantic
     moved 3 cells, all inconclusive.
   - **Cold semantic, full 400 queries** (the only mode P6 changed): recall@1
     +0.005 (2-1), recall@5 +0.005 (2-1), recall@10 +0.000 (0-0), MRR +0.004.
     Three of 400 queries changed rank. Every CI crosses zero.

   So the quantization change is quality-neutral to within what 400 queries can
   resolve, and it buys exact cold/warm agreement. Raw output in
   `eval/results/refactor-ab-*.json`. (The pre-refactor SHA quoted above no
   longer resolves — history was rewritten when the harnesses moved to
   gorp-bench.)

   Still outstanding, and separate: #10 means the published §9 lever numbers were
   measured through a cache a `--window` sweep could contaminate. Those want a
   re-run on their own terms; this A/B does not substitute for it.

6. ~~**Concurrent first-searches SIGBUS, not merely return zero hits.**~~
   **Closed by #16**, the staging-and-rename publication this item asks for.
   (The text below credits the fix to "#2"; #2 is the racy e2e suite — the
   staging fix is #16.) #3 above
   describes the race as producing "an occasional zero-hit run". Simulation
   testing measured it: 20 trials x 8 parallel first-searches of one fresh scope
   produced **exit -10 (SIGBUS) in 5-8 of 20 trials** on a small corpus, and
   occasionally on tokio. SIGBUS on a mapped file is an `mmap` whose backing file
   was replaced underneath it, which is what `store::build` does when it rewrites
   `emb.bin` in place. `unpublish()` stops a *new* reader starting; it cannot
   recall a mapping that already exists. The staging-dir-and-rename fix in #2
   closes this and makes `unpublish` unnecessary. Rate measured in
   SIMULATION.md §1.2; nobody had one before.

7. ~~**Read-repair has no delta-size bound.**~~ **Closed by #17** —
   `repair::DEFAULT_MAX_DRIFT = 0.05`, surfaced as `RepairOutcome::DriftTooLarge`
   and `--repair-max-drift`. RESEARCH.md §8 mechanism 2 specifies
   treating drift above ~5% of files as a full miss; `repair.rs` never bounds
   `drift.len()`. Measured on tokio: at 50% drift a repaired query costs 131 ms
   against a 127 ms full cold pass, and at 100% drift 197 ms — and it never
   amortizes, because the overlay is discarded and rebuilt on every query past
   the TTL. The walk that finds the drift is flat at ~7 ms; all the growth is
   re-embedding, which the threshold would skip. After a branch switch this is
   the steady state. SIMULATION.md §1.3.

8. ~~**A corpus that cannot fit the budget is built and then immediately
   evicted.**~~ **Closed by #18** — `write_cache_entry` now calls
   `enforce_budget_protecting`, which will not evict the entry the running query
   just paid for. As found, it called `enforce_budget()` before returning, so with a
   budget below one entry the query builds a complete index, has it reclaimed,
   misses on re-discovery, and falls through to a full streaming pass — paying
   for both and keeping neither. Observed on 5 of 8 queries under budget
   pressure. #5 moved reclamation after registration so the enforcer could see
   the new entry; it can now see it, and evicts it. SIMULATION.md §1.4.

9. ~~**LRU eviction destroys healthy entries when a delete fails.**~~
   **Closed by #19** — a failed delete now records the entry as stuck and breaks
   the loop, and `dir_bytes` recurses. As found, the eviction loop
   popped the victim before attempting `remove_dir_all` and
   decrements the running total only on success, so a failure neither stops the
   loop nor accounts for itself. With one entry `chmod 0500`: four entries
   before, one after, and the survivor is the undeletable one. Exit 0, no
   warning. Also `dir_bytes` is non-recursive, correct only because entries
   happen to be flat.

10. ~~**`out::hits` does not escape paths.**~~ **Closed by #20** — paths are
    C-quoted through `out::quote_path`. As found, `println!("{}:{}:{}")` meant a
    filename containing a newline splits one hit across two stdout lines, and one
    containing `:` mis-parses. Six files on disk produced seven output lines.
    `--json` is unaffected. This breaks the `path:line:text` contract that
    "stdout is data" rests on. SIMULATION.md §1.5.

11. ~~**A nonexistent search path exits 1, not 2.**~~ **Closed by #21.**
    "No results" is what an agent
    reads as "the code is not there", when the path was simply wrong. Ranked mode
    additionally announces it is caching a scope that does not exist.

12. ~~**Every warm ranked query resolves the index twice.**~~ **Closed by
    #22** — the engine reports the first search through an `on_first_search`
    callback instead, so a warm query resolves once. As found,
    `warn_if_first_search` called `cache::discover` on every ranked search, not
    only the first, to decide whether to print one line — then the engine
    resolves the same scope again. Each resolution canonicalizes the path and
    scans the generation directory. The engine already reports `wrote_cache`.

13. ~~**A narrow scope can return zero hits.**~~ **Closed by #23** — ranking is
    scope-aware now. As found, `candidates()` filtered to the
    query's subtree before truncating, but the fused list is only
    `FUSION_POOL * 2 = 256` rows wide. On tokio, `.github` and `docs` returned 0
    hits from 8,042 indexed chunks. SIMULATION.md §1.7.
