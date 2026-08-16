#!/usr/bin/env python3
"""The scenario catalog: what to run, and what was predicted before running it.

Each scenario is a function `(sess, ctx) -> None` plus an `EXPECT` block copied
into the session header. The prose version, with the reasoning behind each
prediction, is `eval/sim/PREREGISTER.md`, committed before any of this ran.

Every scenario pins `GORP_CACHE_TTL_SECS` explicitly. Left at the 60s
default, half of these would be measuring the clock.
"""

import json
import os
import time
from pathlib import Path

import corpora
import harness

REGISTRY = []


def scenario(name, tier=1, expect=None, notes=""):
    def deco(fn):
        REGISTRY.append({"name": name, "fn": fn, "tier": tier,
                         "expect": expect or {}, "notes": notes})
        return fn
    return deco


Q = "exponential backoff retry policy"


def pick_file(root):
    """A mid-sized text file to edit, whatever corpus this is.

    Scenarios used to name `core/m0.py`, which exists only in the synthetic
    tree; against a real corpus they raised and reported nothing. Deterministic
    (sorted, then the median by size) so a rerun edits the same file.
    """
    cands = []
    for p in sorted(Path(root).rglob("*")):
        if not p.is_file() or p.is_symlink() or ".gorp" in p.parts:
            continue
        try:
            n = p.stat().st_size
        except OSError:
            continue
        if 200 <= n <= 20_000 and p.suffix in (".py", ".rs", ".go", ".java",
                                               ".rb", ".ts", ".js", ".md", ".c"):
            cands.append(p)
    if not cands:
        raise RuntimeError(f"no editable file found under {root}")
    return cands[len(cands) // 2]


def subdirs(root, limit=4):
    """Directories that actually hold files, for scope-narrowing scenarios."""
    out = []
    for p in sorted(Path(root).iterdir()):
        if p.is_dir() and not p.is_symlink() and p.name not in (".gorp", ".git"):
            if any(q.is_file() for q in p.rglob("*")):
                out.append(p)
    return out[:limit]


# ---------------------------------------------------------------------------
# S1 — cold start / write-through
# ---------------------------------------------------------------------------

@scenario("s1-cold-start", tier=1, expect={
    "path_taken": "cold_write_through",
    "build_share_of_total": ">= 0.90",
    "discover_calls": 2,
    "build_stages_visible": True,
}, notes="The first ranked search of a scope builds an index inside search(). "
         "Was 3 resolutions until the CLI stopped resolving on its own to decide "
         "whether to print the cold-start notice (FIXES.md #12); the remaining two "
         "are the engine's own miss and its post-build re-discovery.")
def s1_cold_start(sess, ctx):
    step = sess.run(["--json", "-k", "10", Q], env={"GORP_CACHE_TTL_SECS": "0"})
    t = step.trace or {}
    res = t.get("resolution", {})
    timing = t.get("timing", {})
    total = timing.get("total_ms", 0) or 1.0
    build = timing.get("buckets", {}).get("build_ms", 0.0)

    sess.check("path is cold write-through", "cold_write_through", res.get("path_taken"))
    sess.check("build dominates the first query", True, round(build / total, 3),
               ok=build / total >= 0.90,
               note=f"build={build:.1f}ms total={total:.1f}ms share={build/total:.3f}")
    sess.check("index resolved twice", 2, res.get("discover_calls"),
               note="was 3 before FIXES.md #12; the engine's miss plus its "
                    "post-build re-discovery, both unavoidable today")
    sess.check("the build's internal split is reported", True,
               [s["stage"] for s in timing.get("stages", []) if s["stage"].startswith("build:")
                and s["ms"] > 0],
               ok=any(s["stage"] == "build:embed" and s["ms"] > 0
                      for s in timing.get("stages", [])))


# ---------------------------------------------------------------------------
# S2 — warm-session amortization
# ---------------------------------------------------------------------------

@scenario("s2-warm-amortization", tier=1, expect={
    "queries_2_to_n_within": "2x of each other",
    "repair": "ttl_fresh for all but the first",
    "load_share_of_warm_query": "> 0.60",
}, notes="Every query re-pays the index load; nothing is resident between processes.")
def s2_warm(sess, ctx):
    queries = [
        Q, "circuit breaker threshold", "connection pool sizing",
        "token bucket refill rate", "lease renewal interval",
        "cache eviction weight", "heartbeat deadline", "shard rebalance",
        "write ahead flush", "retry jitter computation",
    ]
    sess.run(["--json", "-k", "10", Q], env={"GORP_CACHE_TTL_SECS": "3600"},
             label="prime")
    warm = []
    for i, q in enumerate(queries):
        s = sess.run(["--json", "-k", "10", q],
                     env={"GORP_CACHE_TTL_SECS": "3600"}, label=f"warm{i}")
        warm.append(s)

    totals = sorted(s.trace["timing"]["total_ms"] for s in warm if s.trace)
    # p90/p50, not max/min: a single query that lands while the page cache is
    # cold is not "the session got slower", and a max/min ratio lets that one
    # sample decide the verdict.
    med = totals[len(totals) // 2]
    p90 = totals[min(len(totals) - 1, int(0.9 * (len(totals) - 1)))]
    spread = p90 / max(med, 1e-9)
    sess.check("warm query time is stable across a session", True, round(spread, 2),
               ok=spread <= 2.0,
               note=f"p90/p50; totals={['%.1f' % t for t in totals]}")

    outcomes = [s.trace["repair"]["outcome"] for s in warm if s.trace]
    sess.check("the TTL throttles validation", True, outcomes,
               ok=all(o == "ttl_fresh" for o in outcomes))

    shares = [s.bucket_ms("load") / max(s.trace["timing"]["total_ms"], 1e-9)
              for s in warm if s.trace]
    mean = sum(shares) / len(shares)
    sess.check("index load dominates a warm query", True, round(mean, 3),
               ok=mean > 0.60,
               note="every process re-reads the index; nothing is resident between them")


# ---------------------------------------------------------------------------
# S3 — drift, read-repair, and the TTL gate
# ---------------------------------------------------------------------------

@scenario("s3a-ttl-fresh-serves-stale", tier=1, expect={
    "repair": "ttl_fresh", "stale_files": 0, "stale_text_served": True,
}, notes="Correct by design, but a user sees a function that no longer exists.")
def s3a(sess, ctx):
    sess.run(["--json", "-k", "10", "unique_marker_alpha"],
             env={"GORP_CACHE_TTL_SECS": "3600"}, label="prime")
    target = pick_file(ctx["root"])
    sess.mutate("insert-unique-symbol", fn=lambda: {
        "wrote": str(target),
        "bytes": target.write_text(
            target.read_text() + "\n\ndef unique_marker_alpha():\n    return 1\n"),
    })
    s = sess.run(["--json", "-k", "10", "unique_marker_alpha"],
                 env={"GORP_CACHE_TTL_SECS": "3600"}, label="after-edit")
    sess.check("repair was throttled", "ttl_fresh", s.trace["repair"]["outcome"])
    sess.check("no staleness is reported", 0,
               s.trace["results"]["stale_files"])
    found = any(target.name in h["path"] for h in s.hits)
    sess.check("the new symbol is NOT found (stale answer served)", False, found,
               note="the throttle is deliberate; this records what it costs")


@scenario("s3b-no-drift", tier=1, expect={
    "repair": "no_drift", "repair_walk_ms": "> 0", "repair_delta_ms": 0,
})
def s3b(sess, ctx):
    sess.run(["--json", "-k", "10", Q], env={"GORP_CACHE_TTL_SECS": "0"},
             label="prime")
    s = sess.run(["--json", "-k", "10", Q], env={"GORP_CACHE_TTL_SECS": "0"})
    sess.check("clean tree reports no drift", "no_drift", s.trace["repair"]["outcome"])
    sess.check("the drift walk ran", True, s.stage_ms("repair:walk"),
               ok=s.stage_ms("repair:walk") > 0)
    sess.check("no overlay was built", 0.0, s.stage_ms("repair:delta"))


@scenario("s3c-repair-never-writes-back", tier=1, expect={
    "repair": "repaired", "new_text_served": True,
    "entry_digest_unchanged": True,
}, notes="The overlay is rebuilt from scratch on every query past the TTL.")
def s3c(sess, ctx):
    sess.run(["--json", "-k", "10", Q], env={"GORP_CACHE_TTL_SECS": "0"},
             label="prime")
    entries = corpora.index_dirs(sess.cache)
    before = corpora.digest(entries[0]) if entries else None

    target = pick_file(ctx["root"])
    sess.mutate("add-unique-symbol", fn=lambda: {
        "bytes": target.write_text(
            target.read_text() + "\n\ndef unique_marker_beta():\n    return 2\n")})

    s1 = sess.run(["--json", "-k", "10", "unique_marker_beta"],
                  env={"GORP_CACHE_TTL_SECS": "0"}, label="repair-1")
    sess.check("drift is repaired", "repaired", s1.trace["repair"]["outcome"])
    sess.check("the new text is served", True,
               any(target.name in h["path"] for h in s1.hits))

    after = corpora.digest(entries[0]) if entries else None
    sess.check("the entry on disk is unchanged", before, after,
               note="repair never writes back, so the work is redone every query")

    s2 = sess.run(["--json", "-k", "10", "unique_marker_beta"],
                  env={"GORP_CACHE_TTL_SECS": "0"}, label="repair-2")
    d1, d2 = s1.stage_ms("repair:delta"), s2.stage_ms("repair:delta")
    sess.check("the second query pays the same repair cost", True,
               {"first_ms": round(d1, 2), "second_ms": round(d2, 2)},
               ok=d2 > 0.5 * d1,
               note="no amortization between queries")


@scenario("s3d-same-second-same-size-edit", tier=1, expect={
    "repair": "no_drift (PREDICTED FAILURE)",
    "stale_text_served": True,
}, notes="(size, mtime) cannot see a length-preserving edit inside one second.")
def s3d(sess, ctx):
    target = pick_file(ctx["root"])
    target.write_text("def marker_aaa():\n    return 'aaa'\n")
    time.sleep(1.1)          # let the index's mtime settle before building
    sess.run(["--json", "-k", "10", "marker_aaa"],
             env={"GORP_CACHE_TTL_SECS": "0"}, label="prime")

    mut = sess.mutate("same-size-same-mtime-edit",
                      fn=lambda: corpora.same_second_same_size_edit(target))
    # The scenario is only meaningful if the condition was actually built.
    # An earlier version slept and hoped, landed in a different second every
    # time, and reported "drift was detected" — testing nothing at all.
    constructed = mut["detail"].get("size_and_mtime_identical")
    sess.check("the (size, mtime) pair is genuinely unchanged", True, constructed,
               note="if this fails, the scenario below proves nothing")

    s = sess.run(["--json", "-k", "10", "marker_aaa"],
                 env={"GORP_CACHE_TTL_SECS": "0"}, label="after-invisible-edit")
    sess.check("the edit is invisible to drift detection", "no_drift",
               s.trace["repair"]["outcome"],
               note="PREDICTED FAILURE: a length-preserving edit sharing a second "
                    "with the index is not seen by (size, mtime)")
    sess.check("but the file really did change on disk", True,
               "marker_aaa" not in target.read_text(),
               note="so a search is now serving text that is not there")


# ---------------------------------------------------------------------------
# S4 — the branch-switch delta cliff
# ---------------------------------------------------------------------------

@scenario("s4-delta-cliff", tier=2, expect={
    "repair_delta_grows_linearly_with_drift": "below the threshold only",
    "query_10_costs_the_same_as_query_1": "below the threshold only",
    "drift_above_threshold_rebuilds": True,
    "threshold_implemented": True,
}, notes="RESEARCH.md §8 mechanism 2 specifies a delta-size threshold. The original "
         "run measured what its absence cost — 131ms/query at 50% drift against a "
         "127ms cold pass, forever — and those two predictions HELD (SIMULATION.md "
         "§1.3). The threshold is now implemented (FIXES.md #7), so the two "
         "unqualified predictions above are deliberately retired rather than "
         "relaxed: they still describe drift below the bound, and above it the "
         "entry is rebuilt instead, which is a different claim and is checked as one.")
def s4_cliff(sess, ctx):
    root = ctx["root"]
    fractions = [0.0, 0.01, 0.05, 0.10, 0.25, 0.50, 1.00]
    measured = []

    sess.run(["--json", "-k", "10", Q], env={"GORP_CACHE_TTL_SECS": "0"},
             label="build-entry")
    # A clean rebuild of the same corpus, for the crossover comparison.
    rb = sess.run(["--json", "-k", "10", Q, "--no-index"],
                  env={"GORP_CACHE_TTL_SECS": "0"}, label="cold-reference")
    cold_ms = rb.trace["timing"]["total_ms"] if rb.trace else 0.0

    cumulative = 0.0
    for frac in fractions:
        step = frac - cumulative
        if step > 0:
            touched = corpora.drift_files(root, step, seed=int(frac * 1000),
                                          marker=f"D{int(frac*100)}")
            sess.mutate(f"drift-to-{int(frac*100)}pct",
                        fn=lambda t=touched: {"n_touched": len(t)})
            cumulative = frac
            time.sleep(1.05)   # cross a second boundary so mtime moves

        reps = []
        for i in range(10):
            s = sess.run(["--json", "-k", "10", Q],
                         env={"GORP_CACHE_TTL_SECS": "0"},
                         label=f"drift{int(frac*100)}-q{i}")
            reps.append(s)
        totals = [r.trace["timing"]["total_ms"] for r in reps if r.trace]
        deltas = [r.stage_ms("repair:delta") for r in reps]
        walks = [r.stage_ms("repair:walk") for r in reps]
        measured.append({
            "fraction": frac,
            "total_first": totals[0], "total_last": totals[-1],
            "total_median": sorted(totals)[len(totals) // 2],
            "delta_first": deltas[0], "delta_last": deltas[-1],
            "delta_median": sorted(deltas)[len(deltas) // 2],
            "walk_median": sorted(walks)[len(walks) // 2],
            # Both ends, and the distinction is the whole point once a threshold
            # exists: the *first* query after drifting is the one that decides
            # repair-or-rebuild, and if it rebuilds then every query behind it
            # sees a clean entry and reports `no_drift`. Recording only the last
            # one made a working rebuild look like nothing had drifted at all.
            "outcome_first": reps[0].trace["repair"]["outcome"] if reps[0].trace else None,
            "outcome": reps[-1].trace["repair"]["outcome"] if reps[-1].trace else None,
        })

    sess.mutate("record-sweep", fn=lambda: {"sweep": measured,
                                            "cold_reference_ms": cold_ms})

    # Levels the overlay actually handled, versus levels that rebuilt instead.
    repaired = [m for m in measured if m["delta_first"] > 0.5]
    rebuilt = [m for m in measured if m["outcome_first"] == "drift_too_large"]

    # Below the bound the overlay still does not amortize: it is discarded and
    # rebuilt on every query past the TTL, because repair never writes back.
    # That remains true and remains unfixed — it is simply no longer unbounded.
    ratios = [m["delta_last"] / max(m["delta_first"], 1e-9) for m in repaired]
    typical = sorted(ratios)[len(ratios) // 2] if ratios else 0.0
    sess.check("a repaired query still does not amortize", True,
               {"ratios": [round(r, 2) for r in ratios], "median": round(typical, 2)},
               ok=bool(ratios) and typical > 0.4,
               note="below the threshold the overlay is still rebuilt every query")

    grows = all(repaired[i]["delta_median"] <= repaired[i + 1]["delta_median"] + 1.0
                for i in range(len(repaired) - 1))
    sess.check("repair cost grows with drift, below the threshold", True, grows,
               note=[f"{m['fraction']}:{m['delta_median']:.1f}ms" for m in repaired])

    # The bound itself. Past it the entry is replaced, not patched.
    sess.check("heavy drift is rebuilt rather than repaired", True, bool(rebuilt),
               note=[f"{m['fraction']}:{m['outcome_first']}" for m in measured])

    # And that is what buys the amortization the old behavior never had: the
    # rebuild lands on the first query, and the nine after it are warm. This is
    # the claim the whole threshold rests on, so it is checked directly.
    amortized = [
        {"fraction": m["fraction"],
         "first_ms": round(m["total_first"], 1),
         "median_ms": round(m["total_median"], 1),
         "cold_ms": round(cold_ms, 1)}
        for m in rebuilt
    ]
    sess.check("after a rebuild the following queries are warm", True, amortized,
               ok=bool(amortized) and all(a["median_ms"] < a["first_ms"] for a in amortized),
               note="repairing charged full price on every query; rebuilding charges once")

    # The regression this exists to prevent: a query that costs more than simply
    # throwing the cache away. At 100% drift the old path was 1.56x a cold pass.
    worst = max((m["total_median"] for m in measured), default=0.0)
    sess.check("no drift level costs more than a cold pass", True,
               {"worst_median_ms": round(worst, 1), "cold_ms": round(cold_ms, 1)},
               ok=cold_ms > 0 and worst <= cold_ms,
               note="SIMULATION.md §1.3 measured 197ms against a 127ms cold pass at 100% drift")

    # The original finding, inverted. It measured 196.9ms against a 126.6ms cold
    # pass at 100% drift — a warm query costing 1.56x what throwing the cache
    # away would have — and predicted it would keep costing that forever. Both
    # held. The threshold is what stops it, so the check that recorded the
    # pathology now guards against its return.
    steady = measured[-1]["total_median"]
    sess.check("at 100% drift the steady-state query is cheaper than a cold pass", True,
               {"steady_ms": round(steady, 1), "cold_ms": round(cold_ms, 1),
                "was": "196.9ms vs 126.6ms cold, on every query"},
               ok=cold_ms > 0 and steady < cold_ms,
               note="one rebuild, then warm — rather than paying the delta on every query")


# ---------------------------------------------------------------------------
# S5 — budget and LRU
# ---------------------------------------------------------------------------

@scenario("s5a-budget-thrash", tier=1, expect={
    "every_query_cold": True,
}, notes="A budget below two entries turns a cache into a rebuild loop.")
def s5a(sess, ctx):
    roots = ctx["multi_roots"]
    # Build one entry to learn its size, then cap the budget just above it.
    first = sess.run(["--json", "-k", "5", Q], path=roots[0],
                     env={"GORP_CACHE_TTL_SECS": "0"}, label="size-probe")
    entries = corpora.index_dirs(sess.cache)
    size = sum(p.stat().st_size for p in entries[0].rglob("*") if p.is_file()) if entries else 0
    cap = int(size * 1.5) or 1_000_000
    sess.mutate("set-budget", fn=lambda: {"entry_bytes": size, "cap_bytes": cap})

    paths = []
    for rnd in range(2):
        for i, r in enumerate(roots):
            s = sess.run(["--json", "-k", "5", Q], path=r,
                         env={"GORP_CACHE_TTL_SECS": "0",
                              "GORP_CACHE_MAX_BYTES": str(cap)},
                         label=f"r{rnd}-c{i}")
            paths.append(s.trace["resolution"]["path_taken"] if s.trace else None)
    sess.mutate("record-paths", fn=lambda: {"paths": paths})
    n_cold = sum(1 for p in paths if p == "cold_write_through")
    sess.check("a too-small budget rebuilds on nearly every query", True,
               {"n_cold": n_cold, "n_total": len(paths), "paths": paths},
               ok=n_cold >= len(paths) - len(roots))


@scenario("s5b-eviction-failure", tier=1, expect={
    "healthy_entries_destroyed": False,
    "cache_stays_over_budget": True,
    "error_reported": False,
}, notes="Pre-registered as a cascade and measured as one: budget.rs popped the "
         "victim before attempting the delete and decremented only on success, so "
         "one chmod 0500 entry took every healthy entry behind it — four in, one "
         "out, exit 0, silent. The loop stops on a failed delete now and reports it "
         "(FIXES.md #19), so the prediction is retired rather than relaxed.")
def s5b(sess, ctx):
    roots = ctx["multi_roots"]
    for i, r in enumerate(roots):
        sess.run(["--json", "-k", "5", Q], path=r,
                 env={"GORP_CACHE_TTL_SECS": "0"}, label=f"build{i}")
    entries = corpora.index_dirs(sess.cache)
    sess.mutate("built-entries", fn=lambda: {"n": len(entries),
                                             "dirs": [e.name for e in entries]})
    if len(entries) < 3:
        sess.check("enough entries to test eviction", ">=3", len(entries), ok=False)
        return

    victim = entries[0]
    sess.mutate("chmod-0500-an-entry", fn=lambda: (
        os.chmod(victim, 0o500) or {"victim": victim.name}))

    # The enforcer runs inside `write_cache_entry`, so it only fires on a cold
    # miss. Querying an already-indexed root answers warm and never reaches it —
    # which is how the first version of this scenario "found" nothing. A root
    # that has never been indexed is what forces the write path.
    fresh = ctx["fresh_root"]
    r = sess.run(["--json", "-k", "5", Q], path=fresh,
                 env={"GORP_CACHE_TTL_SECS": "0",
                      "GORP_CACHE_MAX_BYTES": "1"},
                 label="force-eviction-via-cold-write")
    os.chmod(victim, 0o755)

    after = corpora.index_dirs(sess.cache)
    sess.mutate("after-eviction", fn=lambda: {
        "n_before": len(entries), "n_after": len(after),
        "victim_survived": victim.exists(),
        "names_after": [e.name for e in after]})

    sess.check("the undeletable entry survives", True, victim.exists(),
               note="its directory could not be removed, so it stays")
    # The cap here is 1 byte, so every entry is over budget and some eviction is
    # correct. The bug was not that anything was evicted — it was that the loop
    # could not tell a failed delete from a successful one, so it ran past the
    # undeletable entry and consumed everything behind it, leaving that entry as
    # the *sole* survivor. It stops at the first failure now.
    survivors_besides_victim = [e for e in after if e != victim]
    sess.check("an undeletable entry does not take every healthy one with it", True,
               {"before": len(entries), "after": len(after),
                "survivors_besides_victim": [e.name for e in survivors_besides_victim]},
               ok=len(survivors_besides_victim) >= 1,
               note="was 4 in, 1 out, and the survivor was the undeletable one")
    sess.check("no error is surfaced to the caller", 0, r["exit"],
               note="the cache is left over budget silently")


@scenario("s5c-dir-bytes-is-not-recursive", tier=1, expect={
    "subdirectory_bytes_counted": True,
}, notes="Pre-registered as False: dir_bytes summed one level, correct only because "
         "entries happen to be flat. It recurses now (FIXES.md #19). This scenario "
         "ALSO never ran: it invoked `gorp cache <path>`, which is a usage error, "
         "and then checked that a size was absent from the empty stdout it got back "
         "— green, and measuring nothing, for its whole life. The harness now "
         "distinguishes 'no path given' from 'this verb takes no path'.")
def s5c(sess, ctx):
    sess.run(["--json", "-k", "5", Q], env={"GORP_CACHE_TTL_SECS": "0"},
             label="build")
    entries = corpora.index_dirs(sess.cache)
    if not entries:
        sess.check("an entry was built", True, False, ok=False)
        return
    e = entries[0]

    before = sess.run(["cache"], path=harness.Session.NO_PATH, label="cache-status-before")
    size_before = _reported_bytes(before["stdout"])

    def plant():
        sub = e / "planted"
        sub.mkdir(exist_ok=True)
        (sub / "big.bin").write_bytes(b"\0" * 5_000_000)
        return {"planted_bytes": 5_000_000}
    sess.mutate("plant-5MB-subdirectory", fn=plant)

    st = sess.run(["cache"], path=harness.Session.NO_PATH, label="cache-status")
    reported = st["stdout"]
    sess.mutate("cache-status-output", fn=lambda: {"stdout": reported[:2000],
                                                   "exit": st["exit"]})
    # The precondition, asserted rather than assumed — this check spent its whole
    # life reading an empty string because the invocation was a usage error.
    sess.check("cache status actually ran", 0, st["exit"],
               note="`gorp cache` takes no path argument")
    size_after = _reported_bytes(reported)
    # Growth, not a literal "5.0 MB": the reported figure is the whole entry, so
    # on a real corpus it is the index plus the planted bytes and matching a
    # string would only ever work on a corpus that indexed to nothing.
    grew = size_after - size_before
    sess.check("the planted 5MB is counted in the entry's size", True,
               {"before_bytes": size_before, "after_bytes": size_after, "grew_by": grew},
               ok=grew >= 4_000_000,
               note="dir_bytes recurses now, so a nested artifact counts against the budget")


# ---------------------------------------------------------------------------
# S7 — adversarial corpus
# ---------------------------------------------------------------------------

@scenario("s7-adversarial-corpus", tier=1, expect={
    "no_panic": True, "no_hang": True,
    "path_line_text_contract_broken_by_newline_filename": False,
    "json_survives": True,
}, notes="Pre-registered and measured as broken: six files produced seven stdout "
         "lines and `od:d.py` mis-parsed silently. `out::hits` quotes ambiguous "
         "paths now (FIXES.md #20), so the prediction is retired — the contract "
         "holds, and this scenario is what keeps it holding.")
def s7(sess, ctx):
    root = ctx["adversarial"]
    modes = ["hybrid", "bm25", "semantic", "keyword"]
    crashes, timeouts = [], []
    for m in modes:
        args = (["-e", "compute_backoff"] if m == "keyword"
                else ["--mode", m, "-k", "10", "compute_backoff"])
        s = sess.run([*args, "--json"], path=root, timeout=120, label=f"json-{m}")
        if s.crashed:
            crashes.append((m, s["exit"]))
        if s["timed_out"]:
            timeouts.append(m)

    sess.check("no mode panics on the adversarial tree", [], crashes)
    sess.check("no mode hangs", [], timeouts)

    # The plain-text contract, which is what "stdout is data" rests on.
    #
    # Scoped to `names/`, which holds one file per hostile filename and nothing
    # else. Run against the whole tree this check passes for the wrong reason:
    # `huge.py` emits 200k matching lines, the capture cap cuts long before the
    # odd names, and a format assertion that never saw them proves nothing.
    plain = sess.run(["-e", "compute_backoff", "--all"], path=root / "names",
                     timeout=120, label="plaintext-exact-names")
    lines = plain.stdout_lines()
    n_files = len([p for p in (root / "names").iterdir() if p.is_file()])
    bad = [ln for ln in lines if ln and not _looks_like_hit_line(ln)]
    sess.mutate("name-scope", fn=lambda: {
        "n_files_on_disk": n_files, "n_stdout_lines": len(lines),
        "truncated": plain["stdout_truncated"], "lines": lines[:20]})
    sess.check("one stdout line per matching file", n_files, len(lines),
               note="a newline in a filename splits one hit across two lines")
    sess.check("every stdout line parses as path:line:text", [], bad[:8],
               ok=not bad,
               note="ambiguous paths are C-quoted (FIXES.md #20); ordinary ones are "
                    "byte-identical, so a plain split still works on them")

    js = sess.run(["-e", "compute_backoff", "--all", "--json"],
                  path=root / "names", timeout=120, label="json-exact-names")
    jlines = js.stdout_lines()
    unparsed = [ln for ln in jlines if not _is_json(ln)]
    sess.check("every --json line is valid JSON", [], unparsed[:5],
               note="serde escapes what println! does not")
    sess.check("--json emits one object per matching file", n_files, len(jlines),
               note="if --json survives what plain text does not, the fix is escaping")


def _reported_bytes(status_stdout):
    """Total bytes `gorp cache` reports, from its `(N entries, X of Y budget)`
    header. Parsed rather than string-matched so the check works on any corpus."""
    import re
    m = re.search(r"\(\d+ entries, ([\d.]+) (B|KB|MB|GB|TB) of ", status_stdout)
    if not m:
        return 0
    scale = {"B": 1, "KB": 1e3, "MB": 1e6, "GB": 1e9, "TB": 1e12}[m.group(2)]
    return int(float(m.group(1)) * scale)


def _looks_like_hit_line(line):
    """`path:line:text` — the field before the second colon must be an int.

    A path is C-quoted when it contains a control character, a quote, or a colon
    (FIXES.md #20), so the split has to skip a quoted path rather than cut inside
    it. A consumer has to do exactly this, which is the argument for quoting only
    the names that are genuinely ambiguous: every ordinary line still parses with
    a plain `split(":", 2)`.
    """
    if line.startswith('"'):
        i, escaped = 1, False
        while i < len(line):
            if line[i] == '"' and not escaped:
                break
            escaped = line[i] == "\\" and not escaped
            i += 1
        else:
            return False                     # unterminated quote
        rest = line[i + 1:]
    else:
        rest = line[line.find(":"):] if ":" in line else ""
    parts = rest.split(":", 2)
    return len(parts) == 3 and parts[0] == "" and parts[1].isdigit()


def _is_json(line):
    try:
        json.loads(line)
        return True
    except json.JSONDecodeError:
        return False


# ---------------------------------------------------------------------------
# S8 — fault injection on a cache entry
# ---------------------------------------------------------------------------

@scenario("s8-fault-injection", tier=1, expect={
    "bm25_truncated_to_header": "PANIC, entry never evicted, all 3 runs panic",
    "every_other_fault": "clean Err -> evict -> cold fallback -> correct results",
    "semantic_survives_bm25_corruption": True,
}, notes="A panic bypasses search/mod.rs's disposability contract entirely.")
def s8(sess, ctx):
    for fault_name, apply in corpora.FAULTS.items():
        # Each fault gets a fresh entry: a bricked cache must not contaminate
        # the next fault's measurement.
        sess.mutate(f"clear-cache-before-{fault_name}",
                    fn=lambda: _clear(sess.cache))
        sess.run(["--json", "-k", "5", Q], env={"GORP_CACHE_TTL_SECS": "0"},
                 label=f"{fault_name}:build")
        entries = corpora.index_dirs(sess.cache)
        if not entries:
            sess.check(f"{fault_name}: an entry exists to corrupt", True, False, ok=False)
            continue
        d = entries[0]
        try:
            detail = apply(d)
        except (OSError, ValueError) as e:
            sess.mutate(f"apply-{fault_name}", fn=lambda e=e: {"error": repr(e)})
            continue
        sess.mutate(f"apply-{fault_name}", fn=lambda detail=detail: detail)

        # Three consecutive runs: the third is the point. If the damage is
        # self-clearing, run 2 already recovered.
        runs = []
        for i in range(3):
            s = sess.run(["--json", "-k", "5", "--mode", "hybrid", Q],
                         env={"GORP_CACHE_TTL_SECS": "0"}, timeout=120,
                         label=f"{fault_name}:hybrid-{i}")
            runs.append(s)
        codes = [r["exit"] for r in runs]
        crashed = [r.crashed for r in runs]
        hits = [r["n_hits"] for r in runs]

        sess.check(f"{fault_name}: does not crash the process", [False, False, False],
                   crashed, note=f"exit codes {codes}")
        sess.check(f"{fault_name}: recovers by the third run", True,
                   {"codes": codes, "hits": hits},
                   ok=hits[-1] > 0,
                   note="a disposable entry should degrade to a miss and still answer")

        # Semantic mode does not load bm25, so a bm25-only fault should not
        # reach it. This separates "the artifact is broken" from "the reader is".
        sem = sess.run(["--json", "-k", "5", "--mode", "semantic", Q],
                       env={"GORP_CACHE_TTL_SECS": "0"}, timeout=120,
                       label=f"{fault_name}:semantic")
        sess.check(f"{fault_name}: semantic mode answers", True,
                   {"exit": sem["exit"], "hits": sem["n_hits"]},
                   ok=not sem.crashed)


def _clear(cache):
    import shutil
    for p in Path(cache).iterdir():
        shutil.rmtree(p, ignore_errors=True) if p.is_dir() else p.unlink(missing_ok=True)
    return {"cleared": True}


@scenario("s8h-repo-local-index-faults", tier=1, expect={
    "errors_propagate_exit_2": True,
    "except_the_panic_which_does_not_care": True,
}, notes="A repo-local .gorp is an explicit artifact; its failures should surface.")
def s8h(sess, ctx):
    root = ctx["root"]
    for fault_name in ["bm25_truncated_to_header", "emb_deleted", "chunks_truncated"]:
        idx = root / ".gorp"
        sess.mutate(f"rebuild-local-index-for-{fault_name}", fn=lambda: (
            __import__("shutil").rmtree(idx, ignore_errors=True) or {"removed": True}))
        sess.run(["index"], label=f"{fault_name}:index")
        if not (idx / "meta.json").exists():
            sess.check(f"{fault_name}: local index built", True, False, ok=False)
            continue
        detail = corpora.FAULTS[fault_name](idx)
        sess.mutate(f"corrupt-local-{fault_name}", fn=lambda d=detail: d)
        s = sess.run(["--json", "-k", "5", "--mode", "hybrid", Q], timeout=120,
                     label=f"{fault_name}:query")
        sess.check(f"{fault_name}: a repo-local failure is reported, not swallowed",
                   2, s["exit"],
                   note="exit 2 = 'something went wrong', which is the contract")
    __import__("shutil").rmtree(root / ".gorp", ignore_errors=True)


# ---------------------------------------------------------------------------
# S9 — concurrency
# ---------------------------------------------------------------------------

@scenario("s9-concurrent-first-search", tier=1, expect={
    "zero_hit_or_error_rate": "> 0 (FIXES.md #3 is open)",
}, notes="Parallel processes building one scope; the rate has never been measured.")
def s9(sess, ctx):
    import subprocess
    trials, parallel = 20, 8
    results = []
    for t in range(trials):
        _clear(sess.cache)
        env = dict(os.environ)
        env["GORP_CACHE_DIR"] = str(sess.cache)
        env["GORP_CACHE_TTL_SECS"] = "0"
        procs = [subprocess.Popen(
            [str(ctx["bin"]), "--json", "-k", "5", Q, str(ctx["root"])],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env, text=True,
            errors="replace") for _ in range(parallel)]
        outs = []
        for p in procs:
            try:
                o, e = p.communicate(timeout=180)
                outs.append({"code": p.returncode,
                             "hits": sum(1 for ln in o.splitlines() if ln.startswith("{")),
                             "stderr": e[-300:]})
            except subprocess.TimeoutExpired:
                p.kill()
                outs.append({"code": None, "hits": 0, "stderr": "TIMEOUT"})
        results.append(outs)

    flat = [o for trial in results for o in trial]
    bad = [o for o in flat if o["code"] not in (0, 1) or o["hits"] == 0]
    bad_trials = sum(1 for trial in results
                     if any(o["code"] not in (0, 1) or o["hits"] == 0 for o in trial))
    sess.mutate("concurrency-results", fn=lambda: {
        "trials": trials, "parallel": parallel,
        "n_processes": len(flat), "n_bad": len(bad),
        "bad_trials": bad_trials,
        "samples": bad[:10]})
    sess.check("concurrent first-searches all return results", 0, len(bad),
               note=f"{bad_trials}/{trials} trials had at least one bad process; "
                    f"{len(bad)}/{len(flat)} processes affected")


# ---------------------------------------------------------------------------
# S10 — cold == warm parity across the flag matrix
# ---------------------------------------------------------------------------

@scenario("s10-cold-warm-parity", tier=1, expect={
    "parity_holds": "everywhere except --prf",
    "prf_breaks": True,
    "k500_disables_hnsw_silently": True,
}, notes="expand_query exists only in indexed.rs; stream.rs has no counterpart.")
def s10(sess, ctx):
    sess.run(["--json", "-k", "10", Q], env={"GORP_CACHE_TTL_SECS": "0"},
             label="prime")
    variants = [
        ("plain", []),
        ("no-diversify", ["--no-diversify"]),
        ("maxsim", ["--maxsim"]),
        ("prf8", ["--prf", "8"]),
        ("k1", ["-k", "1"]),
        ("k50", ["-k", "50"]),
    ]
    mismatches = []
    for mode in ["bm25", "semantic", "hybrid"]:
        for label, extra in variants:
            base = ["--mode", mode, "--json", *extra]
            if "-k" not in extra:
                base += ["-k", "10"]
            warm = sess.run([*base, Q], env={"GORP_CACHE_TTL_SECS": "0"},
                            label=f"warm-{mode}-{label}")
            cold = sess.run([*base, "--no-index", Q],
                            env={"GORP_CACHE_TTL_SECS": "0"},
                            label=f"cold-{mode}-{label}")
            w = [(h["path"], h["line"]) for h in warm.hits]
            c = [(h["path"], h["line"]) for h in cold.hits]
            if w != c:
                mismatches.append({"mode": mode, "variant": label,
                                   "warm": w[:5], "cold": c[:5],
                                   "n_warm": len(w), "n_cold": len(c)})
    sess.mutate("parity-mismatches", fn=lambda: {"mismatches": mismatches})
    non_prf = [m for m in mismatches if m["variant"] != "prf8"]
    prf = [m for m in mismatches if m["variant"] == "prf8"]
    sess.check("cold and warm agree except under --prf", [], non_prf,
               note="the invariant cold_and_warm_return_identical_results")
    sess.check("--prf breaks parity (predicted)", True, len(prf),
               ok=len(prf) > 0,
               note="PRF is implemented only on the warm path")

    big = sess.run(["--mode", "semantic", "--json", "-k", "500", Q],
                   env={"GORP_CACHE_TTL_SECS": "0"}, label="k500")
    sess.check("-k 500 silently disables HNSW", False,
               big.trace["resolution"]["used_hnsw"] if big.trace else None,
               note="pool > 128 forces brute force even when --hnsw was asked for")


# ---------------------------------------------------------------------------
# S11 — narrow scope
# ---------------------------------------------------------------------------

@scenario("s11-narrow-scope", tier=1, expect={
    "fewer_than_k_hits_for_narrow_scopes": False,
}, notes="Pre-registered and measured as starvation: candidates() filtered to the "
         "subtree *after* truncating a 256-row fused list, so a scope holding none "
         "of the corpus-wide head returned nothing — tokio's docs/ gave 0 hits from "
         "8,042 chunks. Ranking is scope-aware now (FIXES.md #23), so a scope "
         "returns everything it has. Note what 'everything it has' means: a scope "
         "with fewer than k chunks returns fewer than k hits and always did, which "
         "is not starvation and is why the check compares against what is in scope "
         "rather than against k.")
def s11(sess, ctx):
    root = ctx["root"]
    sess.run(["--json", "-k", "10", Q], env={"GORP_CACHE_TTL_SECS": "0"},
             label="prime-whole-corpus")
    results = []
    for p in subdirs(root):
        sub = p.name
        s = sess.run(["--json", "-k", "10", Q], path=p,
                     env={"GORP_CACHE_TTL_SECS": "0"}, label=f"scope-{sub}")
        results.append({"scope": sub, "n_hits": s["n_hits"],
                        "n_considered": s.trace["results"]["n_chunks_considered"]
                        if s.trace else None})
    sess.mutate("scope-results", fn=lambda: {"results": results})
    # The finding, stated as the thing that was actually wrong: a scope with
    # indexed content returned none of it.
    empty = [r for r in results if r["n_considered"] and r["n_hits"] == 0]
    sess.check("no scope with indexed content returns nothing", [], empty,
               note="the original finding: two tokio scopes returned 0 of 8,042 chunks")

    # And a scope with room to spare fills k. Restricted to scopes holding at
    # least `candidate_width(k)` = 3k chunks, because below that the count is
    # decided by span dedupe and MMR rather than by the scope filter — jekyll's
    # `.devcontainer` holds three chunks and returns two, which is diversity
    # working, not starvation, and a check that cannot tell those apart would
    # fail forever on any corpus with a small directory in it.
    roomy = [r for r in results if (r["n_considered"] or 0) >= 30]
    short = [r for r in roomy if r["n_hits"] < 10]
    sess.check("a scope with room to spare returns k hits", [], short,
               note=f"checked {len(roomy)} of {len(results)} scopes; the rest are "
                    f"smaller than the candidate pool")


# ---------------------------------------------------------------------------
# S12 — the exact-miss double search
# ---------------------------------------------------------------------------

@scenario("s12-exact-miss-double-search", tier=1, expect={
    "n_envelopes": 2,
    "phases": ["primary", "suggest"],
    "discover_calls": 2,
}, notes="Pre-registered as 3 resolutions; the CLI's cold-start pre-check never ran "
         "in keyword mode, so it was always 2. Unchanged by FIXES.md #12 for the same "
         "reason — this pair is the suggestion path's own.")
def s12(sess, ctx):
    sess.run(["--json", "-k", "10", Q], env={"GORP_CACHE_TTL_SECS": "0"},
             label="prime")
    s = sess.run(["-e", "zzz_no_such_symbol_anywhere"],
                 env={"GORP_CACHE_TTL_SECS": "0"}, label="exact-miss")
    phases = [t.get("phase") for t in s.traces]
    sess.check("one command, two engine invocations", 2, len(s.traces))
    sess.check("phases are primary then suggest", ["primary", "suggest"], phases)
    if len(s.traces) == 2:
        sess.check("both belong to one command", True,
                   s.traces[0]["query_id"] == s.traces[1]["query_id"])
        sess.check("index resolutions for one failed -e", 2,
                   s.traces[1]["resolution"]["discover_calls"],
                   note="PREREGISTERED AS 3 — the cold-start pre-check skipped keyword mode")
        kw = s.traces[0]["timing"]["total_ms"]
        hy = s.traces[1]["timing"]["total_ms"]
        sess.mutate("double-search-cost", fn=lambda: {
            "keyword_ms": kw, "suggestion_ms": hy,
            "suggestion_share": round(hy / max(kw + hy, 1e-9), 3)})


# ---------------------------------------------------------------------------
# S13 — keyword at scale
# ---------------------------------------------------------------------------

@scenario("s13-keyword-at-scale", tier=1, expect={
    "keyword_reports_a_schedule": True,
    "cost_scales_with_total_matches_not_the_250_cap": True,
})
def s13(sess, ctx):
    root = ctx["root"]
    capped = sess.run(["-e", "def|func|pub fn|return"], path=root, timeout=180,
                      label="capped")
    allhits = sess.run(["-e", "def|func|pub fn|return", "--all"], path=root,
                       timeout=180, label="all")
    sess.check("keyword mode reports a stage schedule", True,
               [s["stage"] for s in (capped.trace or {}).get("timing", {}).get("stages", [])],
               ok=bool((capped.trace or {}).get("timing", {}).get("stages")))
    sess.check("the 250 cap is print-only, so both cost the same", True,
               {"capped_hits": capped.trace["results"]["n_hits"] if capped.trace else None,
                "all_hits": allhits.trace["results"]["n_hits"] if allhits.trace else None},
               ok=(capped.trace and allhits.trace
                   and capped.trace["results"]["n_hits"]
                   == allhits.trace["results"]["n_hits"]),
               note="the full hit vector is materialized regardless of what prints")


# ---------------------------------------------------------------------------
# S14 — a mistyped path
# ---------------------------------------------------------------------------

@scenario("s14-nonexistent-path", tier=1, expect={
    "exit_code": 2,
}, notes="Found while smoke-testing: a typo'd path answers 'no results', not an error.")
def s14(sess, ctx):
    s = sess.run(["--json", "-k", "10", Q], path=ctx["root"] / "no_such_subdir",
                 label="ranked-bad-path")
    sess.check("a nonexistent path is an error, not an empty answer", 2, s["exit"],
               note="'no results' tells an agent the code is absent; the path was wrong")
    e = sess.run(["-e", "compute_backoff"], path=ctx["root"] / "no_such_subdir",
                 label="exact-bad-path")
    sess.check("exact mode on a nonexistent path is an error too", 2, e["exit"])
