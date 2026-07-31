# Observed simulation testing

2026-07-31. 19 scenarios × 3 corpora (a synthetic tree, `tokio` 865 files,
`jekyll` 501 files): 660 semgrep invocations carrying 651 trace envelopes, and
240 pre-registered checks. Predictions were committed in
`eval/sim/PREREGISTER.md` **before any of it ran** (`a616b02`); the sessions are
in `eval/sim/results/` (3.8 MB, checked in) and the generated tables in
`eval/sim/results/INDEX.md`.

Numbers below are from the **post-fix** run unless marked otherwise. The
pre-fix run is what found the panic; both are described where they differ.

Why a third harness. `eval/` scores single queries, `bench/` times single
queries. The index is a cache (RESEARCH.md §8), so almost everything
structurally interesting — write-through, read-repair, the TTL gate, LRU
eviction, corrupt-entry disposal — only exists across a *sequence*, where step N
changes what step N+1 sees. Nothing measured that.

Two things had to exist first: performance provenance the binary emits itself
(`42850d9`), and a session format that records a whole trajectory rather than a
verdict. Both are described at the end.

---

## The short version

**Six real defects**, three of them things a user or agent would actually hit,
and one that permanently disables the cache. **Four of my own predictions were
wrong**, and those are the more useful half of the report, because they were
wrong about where the time goes — which is what the instrumentation was built to
answer.

| # | Finding | Severity | Site |
|---|---|---|---|
| 1 | A corrupt `bm25.flat` **panics** instead of erroring, so the entry is never evicted and every later run panics too | **P0** | `store/bm25.rs:82` |
| 2 | Concurrent first-searches of one scope **SIGBUS** (8/20 trials) | **P0** | `cache/mod.rs:153` |
| 3 | Read-repair has no size bound: past ~25–50% drift a query costs more than a full cold pass, forever | **P1** | `cache/repair.rs:99` |
| 4 | Under budget pressure a query builds an index, immediately evicts it, then streams anyway — paying twice | **P1** | `cache/mod.rs:178` |
| 5 | A filename containing a newline breaks the `path:line:text` stdout contract | **P2** | `out.rs:42` |
| 6 | A mistyped path exits 1 ("no results"), not 2 | **P2** | `cmd/search.rs` |

Plus two confirmed-as-designed behaviors worth knowing (a same-second edit is
invisible to drift detection; a narrow scope can return zero hits), and the LRU
eviction accounting bug, which is real but needs an undeletable directory to
trigger.

---

## 1. Bugs, gaps and issues

### 1.1 A corrupt `bm25.flat` bricks the cache permanently (P0)

The prediction was derived by arithmetic rather than guessed, and it held
exactly. `bm25.flat`'s header is 8 (magic) + 4 (`n_docs`) + 4 (`n_terms`) +
8 (`total_len`) + 5×8 (section offsets) = **exactly 64 bytes**, and
`FlatBm25::open` (`store/bm25.rs:85`) validates only `map.len() >= 64` and the
magic. A file truncated to 64 bytes therefore *passes validation*, and the five
section offsets it reads are real values pointing far past the end of the file.
They are then used as unchecked slice indices in `term_at`, `u32_le`,
`postings_at` and `doc_len_at`.

Observed, identically on all three corpora, for three corruptions
(`truncated_to_header`, `garbage_offsets`, `half`):

```
exit codes [101, 101, 101]     hits [0, 0, 0]
```

Three *consecutive* runs, all panicking, all returning nothing. That third run
is the finding. `search/mod.rs:173-181` makes a cache entry disposable — any
`Err` deletes it and falls through to the streaming path — but a panic is not an
`Err`. The disposal never runs, the entry is never evicted, and **every
subsequent invocation panics on the same bytes.** Recovery requires
`semgrep cache --clear`; nothing the engine does gets there on its own.

`bm25.flat` is the only artifact in the format that is not size-checked.
`emb.bin` is (`store/load.rs:97`), `chunks.bin` is via postcard, `meta.json` via
serde — and all of those degraded cleanly to a cold answer with correct results,
as predicted. In the pre-fix run, **30 invocations exited 101, and all 30 were
this bug**; it was the only crash class found anywhere.

It is worse for a repo-local `.semgrep/`, which is meant to surface errors
rather than degrade: it should exit 2, and exited 101 instead. A panic does not
respect that distinction either.

**Fixed** (see §6), and the same scenario against the fixed binary:

```
before   exit codes [101, 101, 101]   hits [0, 0, 0]
after    exit codes [  0,   0,   0]   hits [5, 5, 5]
```

Crash count across the whole post-fix run: **none**.

### 1.2 Concurrent first-searches SIGBUS (P0)

FIXES.md #3 has been open, narrowed but not closed, described as producing "an
occasional zero-hit run". It is sharper than that. 20 trials × 8 parallel
first-searches of one fresh scope:

| corpus | bad trials | bad processes | failure |
|---|---|---|---|
| synthetic (61 files) | **5–8 / 20** | 12–14 / 160 | exit −10 = **SIGBUS** |
| tokio (865 files) | 0–1 / 20 | 0–7 / 160 | SIGBUS when it fires |
| jekyll (501 files) | 0 / 20 | 0 / 160 | — |

(Ranges across four repetitions of the scenario. It is a race, so the rate
itself varies run to run; the synthetic figure was 5/20, 7/20 and 8/20 on
different passes.)

Not a wrong answer — a memory fault. SIGBUS on a mapped file is the signature of
an `mmap` whose backing file was truncated or replaced underneath it, which is
precisely what `store/build.rs` does: it rewrites `emb.bin` in place while
another process may already have it mapped. `unpublish()` removes `meta.json`
first to stop a *new* reader from starting, but it cannot recall a mapping that
already exists.

The rate is a first measurement — nobody had one. It is strongly size-dependent:
on the smallest corpus builds are fast enough that many processes reach the read
phase while another is still writing, and 25–40% of trials produce at least one
SIGBUS. tokio produced one bad trial out of twenty on one pass and none on the
others, which is not reassurance — it means the window is narrow, not absent.

The documented fix (build into a staging directory, then rename) closes it, and
would also make `unpublish` unnecessary.

### 1.3 The read-repair delta cliff (P1)

RESEARCH.md §8 mechanism 2 specifies the guard: "if the delta exceeds a
threshold (say >5% of files — branch switch), treat the whole query as a miss".
It was never implemented, and `cache/repair.rs:115` iterates every stale path
unconditionally — re-reading, re-chunking, re-tokenizing and **re-embedding**
all of them.

Drift sweep on tokio, TTL=0, ten identical repeated queries at each level:

| drift | query total (median) | `repair:delta` | `repair:walk` |
|---:|---:|---:|---:|
| 0% | 8.7 ms | 0.0 | 6.7 |
| 1% | 14.1 ms | 4.6 | 7.5 |
| 5% | 35.1 ms | 25.6 | 7.4 |
| 10% | 41.7 ms | 31.8 | 7.4 |
| 25% | 90.6 ms | 80.8 | 6.9 |
| 50% | **131.1 ms** | 121.6 | 7.1 |
| 100% | **196.9 ms** | 187.0 | 6.8 |

A full cold streaming pass over the same corpus is **126.6 ms**. So past roughly
50% drift, repairing a cached answer costs *more than throwing the cache away
and reading the corpus from scratch* — and at 100% drift, 1.56× more.

Two details that make it worse than the table suggests:

- **The walk is not the cost.** `repair:walk` is flat at ~7 ms regardless of
  drift. Every millisecond of growth is re-embedding, which the §8 threshold
  would have skipped entirely.
- **It never amortizes.** The 10th identical query costs essentially what the
  1st did (`delta_first` 205.2 ms → `delta_median` 187.0 ms at 100% drift). The
  overlay is discarded and rebuilt on every query past the TTL, because repair
  never writes back and coverage never grows. After a branch switch this is the
  steady state, not a transient.

The crossover landing between 25% and 50% rather than the pre-registered 5–25%
is recorded as a miss in §2.

### 1.4 Build, evict, then stream anyway (P1)

Not predicted; found because the envelope needed a name for a state I did not
think was reachable. With `SEMGREP_CACHE_MAX_BYTES` set to 1.5× one entry and
four corpora queried round-robin, **5 of 8 queries** came back as:

```
wrote_cache = true    used_index = false    build_ms = 2.3    total_ms = 5.2
```

The engine builds a complete index entry; `write_cache_entry` then calls
`enforce_budget()` *before returning* (`cache/mod.rs:178-179`); the budget is too
small so the entry it just wrote is evicted immediately; the re-discovery misses;
and the query falls through to a full streaming pass. **Each such query pays a
complete index build and a complete cold search, and keeps neither.** A warm
query on the same corpus is 1.1 ms; these are 5.0–5.5 ms.

FIXES.md #5 moved reclamation to *after* registration so the enforcer could see
the entry that triggered it. It can now see it — and evicts it. The missing piece
is that a corpus which cannot fit the budget should not be written at all, rather
than written and immediately reclaimed.

### 1.5 A newline in a filename breaks the stdout contract (P2)

`out::hits` (`crates/semgrep/src/out.rs:42`) writes a bare
`println!("{}:{}:{}", path, line, text)` with no escaping or quoting. Six files,
one per hostile filename, in their own directory:

```
6 files on disk  →  7 stdout lines

  -dash.py:1:def compute_backoff(): pass
  od:d.py:1:def compute_backoff(): pass          ← splits as path="od", line="d.py"
  ordinary.py:1:def compute_backoff(): pass
  qu"ote.py:1:def compute_backoff(): pass
  we                                             ← one hit, split
  ird.py:1:def compute_backoff(): pass           ←   across two lines
  with space.py:1:def compute_backoff(): pass
```

`--json` is unaffected: exactly 6 valid objects, serde escapes what `println!`
does not. Reproduced identically on all three corpora.

This matters more than the input looks. "stdout is data, stderr is commentary"
is the CLI's central promise and a tested invariant, and `path:line:text` is what
makes semgrep drop-in for grep. A consumer splitting on `:` mis-parses `od:d.py`
silently, and a newline turns one result into two.

**The first version of this check passed for the wrong reason** — run against
the whole adversarial tree, `huge.py`'s 200k matching lines overflowed the
harness's 64 KB capture cap long before the odd names appeared, and it also made
the harness report "semgrep emits invalid JSON" when what it had actually found
was its own truncation. Both are noted in `harness.py`; the scenario now scopes
to a directory holding only the hostile names, and records how many files were
on disk so a vacuous pass is visible as one.

### 1.6 A mistyped path answers "no results" (P2)

```
$ semgrep "exponential backoff retry policy" ./no_such_subdir
semgrep: first ranked search of this scope — caching it (later searches are fast)
semgrep: no results · try broader phrasing or a nearby concept
$ echo $?
1
```

Exit 1 means "nothing found", and that is what an agent reads: *the code is not
there*. The path was simply wrong. `-e` behaves the same way. Worse, ranked mode
announces it is caching a scope that does not exist. Exit 2 exists for exactly
this and is not used.

### 1.7 Confirmed as designed, worth knowing

**A same-second, length-preserving edit is invisible.** `corpus::diff` compares
`(size, mtime)` and `FileMeta.mtime` is whole seconds, so an edit that preserves
byte length and lands in the same second as the index is not drift. Verified
directly by restoring the mtime after a one-byte swap: `size_and_mtime_identical
= true` → `repair = no_drift` → stale text served, `stale_files = 0`. The
docstring in `diff.rs:52` says as much ("wrong only in the direction that matters
least"), but an agent that edits a file and immediately re-searches is the
common case, not a rare one.

**A narrow scope can return zero hits.** `candidates()` filters to the query's
subtree *before* truncating, but the fused list it filters is only
`FUSION_POOL * 2 = 256` rows wide. On tokio:

| scope | hits (k=10) | chunks considered |
|---|---:|---:|
| `.github` | **0** | 8,042 |
| `docs` | **0** | 7,958 |
| `benches` | 5 | 7,958 |
| `examples` | 3 | 7,958 |

Two scopes return nothing at all from a corpus that was fully indexed. The
comment at `indexed.rs:257` explains the filter-before-truncate ordering and is
correct as far as it goes; it does not cover the case where 256 fused rows
contain nothing from the requested subtree.

**LRU eviction destroys healthy entries when a delete fails.** `budget.rs:115-122`
pops the victim from the list *before* attempting the delete and decrements the
running total only on success, so a failure neither stops the loop nor accounts
for itself. With one entry `chmod 0500`: 4 entries before, **1 after** — and the
single survivor is the undeletable one. Exit code 0, no warning. Confirmed, but
it needs a directory that resists `remove_dir_all`, so it is P2 rather than P1.

---

## 2. Did the results match expectations?

Of 19 scenarios, most predictions held. Four did not, and they are the more
useful half — all four were wrong about *where the time goes*, which is what the
instrumentation was built to answer.

### Wrong: index load does **not** dominate a warm query

Predicted > 60%. Measured 13–14%. The actual breakdown of a warm tokio query
(median 1.77 ms):

| stage | ms | share |
|---|---:|---:|
| `rank:brute` | 0.76 | **42.9%** |
| `finalize:materialize` | 0.49 | **27.9%** |
| `load:*` (all six) | 0.23 | 13.0% |
| `discover` | 0.09 | 5.4% |
| `finalize:mmr` | 0.12 | 6.9% |
| unattributed | 0.04 | 2.0% |

The reasoning behind the prediction was that a process-per-query architecture
re-pays the index load every time. It does — but at this scale the load is a
few `mmap` calls and a small `meta.json`, and what actually costs is the
brute-force i8 scan over the embedding matrix plus **re-reading hit files from
disk to pick the best line**. Materialization being 28% of a warm query is not
something I would have guessed, and it is invisible in the old instrumentation,
where all four of those steps were one bucket called `finalize`.

This has a direct consequence for the roadmap. RESULTS.md proposes a persistent
server to amortize the index load. At tier-2 scale that would buy ~13%. (CLAUDE.md
measures `load:bm25` at 84 ms on the 84k-file kernel, where the picture is surely
different — this finding is scoped to corpora of 500–900 files and says nothing
about kernel scale, which was out of scope for this run.)

### Wrong: reading and tokenizing dominates a build, not embedding

Not pre-registered as a number, but I assumed embedding was the cost. First
visibility into the split, since `BuildStats` carried counts and no timings at
all:

| stage | tokio | jekyll |
|---|---:|---:|
| `build:read+tokenize` | 71.4 ms (**55%**) | 38.3 ms (**57%**) |
| `build:embed` | 45.5 ms (35%) | 17.3 ms (26%) |
| `build:walk` | 7.2 ms (6%) | 7.6 ms (11%) |
| `build:write` | 5.7 ms (4%) | 4.4 ms (6%) |

RESEARCH.md §8.2 lists "pipelined embed overlap" as the remaining ceiling on
build time, on the premise that read/tokenize and embed alternate at batch
barriers. That is the right shape of fix, but the larger half is the one being
overlapped *into*, not the embedding.

### Wrong: the delta-cliff crossover is at 25–50%, not 5–25%

Predicted the crossover — where repairing costs more than rebuilding — between
5% and 25% drift. Measured between 25% and 50% (90.6 ms at 25% vs a 126.6 ms
cold pass; 131.1 ms at 50%). The direction and the mechanism were right, the
magnitude was optimistic by roughly one step. The §8 threshold of 5% is
conservative by about 5–10×, which does not make it wrong — a 5% guard costs
almost nothing and the curve is steep past it.

### Wrong: an exact miss resolves the index twice, not three times

Found while writing the tests, and recorded in `42850d9` rather than quietly
amended. `warn_if_first_search` skips keyword mode entirely, so a failed `-e`
resolves twice (the suggestion path's own check, then the nested `search()`).
Three is the *ranked* cold-miss count. Both are now pinned by CLI tests.

### Near-misses worth stating precisely

- **Build share of a cold start**: predicted ≥ 90%, measured **89%** on tokio and
  **87%** on jekyll. Directionally right, and the point stands — a first query is
  a build with a query stapled to it — but reported as measured rather than
  rounded up.
- **Synthetic corpora cannot resolve the delta cliff.** At 61 tiny files every
  measurement is 4–5 ms and noise dominates; the s4 checks fail there and pass on
  tokio. The synthetic tree is fine for crash-hunting and useless for timing, and
  the report shows both so the distinction is visible rather than assumed.

### Held, as predicted

Cold==warm parity across `{bm25, semantic, hybrid} × {plain, no-diversify,
maxsim, k1, k50}` — including, notably, **`--prf 8`**, which I predicted would
break parity because `expand_query` exists only in `indexed.rs`. It did not, on
any corpus. Either the expansion is a no-op on these queries or fusion absorbs
it; the prediction is unrefuted rather than confirmed, and a query set that
actually moves PRF would be needed to settle it.

Also held: `-k 500` silently disables HNSW; the TTL throttle serves stale text;
repair never writes back; the exact-miss double search; keyword mode's 250-cap
being print-only; every non-bm25 artifact corruption degrading cleanly.

---

## 3. Bottlenecks

Full percentile tables are in `eval/sim/results/INDEX.md`. The three that matter:

**A first query is an index build.** 87–89% of it, and the query it delayed is
1–2% of it. On tokio: 145.4 ms total, 129.8 ms of build, 1.6 ms of ranking. This
was previously unattributable — the build ran inside `search()` and contributed
nothing but `total_ms`.

**A warm query is a vector scan and a file re-read.** 43% `rank:brute`, 28%
`finalize:materialize`. Both scale with corpus size and `k`; neither is cached.

**A repaired query is an embedding job.** `repair:delta` is 95% of a query's time
at 100% drift, and the walk that finds the drift is ~7 ms flat.

`unattributed_ms` — the residual no stage claims — runs **2.0% median on warm
queries** and 0.5–1.0 ms on cold starts. It is printed beside every table above
and by `--stats`, because a number that only appears when it is bad teaches
nobody what normal looks like. It was 31% before this work; the gap was
`ese::encode` of the query sitting outside every span on the cold path, which is
how the residual paid for itself within an hour of existing.

---

## 4. Inefficiencies

| measure | value | site |
|---|---|---|
| index resolutions per **warm** query | **2** | `warn_if_first_search` re-does the work the engine is about to do, to decide whether to print one line |
| index resolutions per **cold** query | **3** | one more from the post-build re-discovery |
| extra full searches run by a failed `-e` | 1 per miss | `suggest_ranked_alternatives` runs a complete hybrid search that `--stats` never showed |
| queries that rebuilt a repair overlay from scratch | 100% of drifted warm queries | the overlay is never written back |
| index builds written and then immediately evicted | 5 of 8 under budget pressure | §1.4 |
| warm query time spent loading the index | 13% | *not* the bottleneck it was assumed to be |

The first is the cheapest to fix and the most consistently paid: every ranked
query canonicalizes its path and scans the generation directory twice, and one of
those two is only there to decide whether to print `first ranked search of this
scope`. The engine already knows — it reports `wrote_cache`.

---

## 5. What the simulation itself got wrong

Recorded because a harness that only reports the system's failures is not being
measured.

- **Two scenarios silently tested nothing.** The adversarial format check ran
  against a tree where a 200k-line file overflowed the capture cap before the
  hostile filenames appeared, and the budget-eviction scenario queried an
  already-indexed root, so the enforcer — which only runs inside
  `write_cache_entry` — never fired. Both *passed*. They now record how many
  files were on disk and whether the condition was constructed, so a vacuous pass
  is visible.
- **One scenario reported a false finding.** "semgrep emits invalid JSON" was the
  harness's own 64 KB truncation cutting a line in half. `Step.stdout_lines()`
  now drops a torn final line and the record carries a `stdout_truncated` flag.
- **One scenario could not build its condition by racing the clock.** The
  same-second edit slept and hoped, landed in a different second every time, and
  reported "drift was detected". It now restores the mtime with `os.utime`, which
  reproduces exactly what `corpus::diff` sees.
- **Corpus-shape assumptions.** Three scenarios named `core/m0.py`, which exists
  only in the synthetic tree; and the first tokio run copied the corpus's
  leftover `.semgrep/` directory, so every "first search of this scope" resolved
  warm and the cold-start and fault-injection scenarios were no-ops that still
  reported numbers.
- **Two brittle thresholds that reported noise as signal.** "Warm query time is
  stable" used max/min, so one query landing on a cold page cache decided the
  verdict; it is p90/p50 now. "Repeated queries never amortize" tested every
  ratio against `> 0.5` and failed on an exact 0.5; it tests the median against
  0.4, which is the claim actually being made.
- **The synthetic corpus cannot support timing claims at all.** At 61 tiny files
  every measurement is 4–5 ms and run-to-run variance exceeds the effects being
  measured, which is why s4's cliff checks fail there and pass on both real
  corpora. It is kept because it is where the concurrency race and the fault
  injection reproduce most readily — but no timing number in this report comes
  from it.

The first four produced a *green* check. That is the failure mode to watch for
in this kind of harness — a scenario that tests nothing passes — and it is why
the scenarios now assert that their own preconditions held (s3d checks that
`(size, mtime)` really was unchanged; s7 records how many files were on disk).

---

## 6. What was changed

**Fixed (crashers only, per the agreed scope):**

- `FlatBm25::open` now validates the five section offsets against the mapped
  length and checks that each section is internally consistent, returning `Err`
  instead of indexing out of bounds. A corrupt entry becomes an ordinary miss
  that the existing disposability path already handles. `tests/bm25_flat.rs`
  covers truncation at every byte boundary and the three corruptions the
  simulation found.

**Reported, not fixed** — each with a patch site, in `FIXES.md`: the concurrency
SIGBUS (staging dir + rename), the repair delta threshold, budget eviction
accounting, path escaping in `out::hits`, the nonexistent-path exit code, the
duplicate discovery on every warm query, and the narrow-scope starvation.

**Instrumentation** (`42850d9`), which is what made any of this measurable:
`Stage` is a closed enum with a declared per-path schedule, zero-filled so the
report has a fixed shape; every stage is a leaf in exactly one bucket so
`walk`/`load`/`rank` are derived sums and `unattributed_ms` means something; and
`SEMGREP_TRACE_FILE` / `--stats-json` emit one JSON envelope per *engine*
invocation, which is how the hidden second search became visible.

**Harness**: `eval/sim/` — `harness.py` (sessions), `corpora.py` (synthetic and
adversarial trees, mutations, fault injection), `scenarios.py` (the catalog with
machine-readable expectations), `run.py`, `report.py`. Stdlib only, like the rest
of `eval/` and `bench/`.

## Reproducing

```sh
cargo build --release
python3 eval/sim/run.py --tier 2 --run-id myrun                        # synthetic
python3 eval/sim/run.py --tier 2 --run-id tokio --corpus bench/corpora/tokio
python3 eval/sim/report.py                                             # regenerate INDEX.md
```

Each session gets its own `SEMGREP_CACHE_DIR`, printed for auditability; no
scenario can be answered from another's entry. `python3 bench/manifest.py --check`
before and after — a simulation that mutated a bench corpus and failed to restore
it would silently invalidate every published benchmark. Scenarios copy their
corpus to `eval/data/sim/` (gitignored) and never touch the original.

## Scope

Three corpora of 61–865 files on one machine (M-series mac). The 84k-file kernel
was out of scope by decision, so nothing here describes kernel-scale behavior —
and §2 shows at least one finding (index load being a small share of a warm
query) that is explicitly scale-dependent. Retrieval quality is `eval/`'s
subject, not this one's: a scenario that passes shows the predicted *behavior*
occurred, not that the answers were good.
