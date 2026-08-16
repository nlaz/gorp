#!/usr/bin/env python3
"""Turn sessions into the five answers, derived rather than written.

    python3 eval/sim/report.py                        # every run under results/
    python3 eval/sim/report.py --runs synthetic,tokio
    python3 eval/sim/report.py --check                # fail if INDEX.md is stale

Sections, in the order the questions were asked:

  1. bugs, gaps and issues        every failed check, plus a crash table
  2. expectations                 pass / fail / SURPRISE against the pre-registration
  3. bottlenecks                  per-stage percentiles with share of total
  4. inefficiencies               derived counters
  5. hardening                    ranked, each with a patch site

`unattributed_ms` is printed beside every timing table. It is the honest "what
we still cannot see" number, and a figure that only appears when it is bad
teaches nobody what normal looks like.
"""

import argparse
import json
import statistics
import sys
from collections import defaultdict
from pathlib import Path

HERE = Path(__file__).resolve().parent
RESULTS = HERE / "results"


def load(run_dirs):
    runs = []
    for d in run_dirs:
        sessions = []
        for sf in sorted(d.glob("*/session.jsonl")):
            recs = []
            for line in sf.read_text(errors="replace").splitlines():
                try:
                    recs.append(json.loads(line))
                except json.JSONDecodeError:
                    continue          # torn last line from a hard kill
            if recs:
                sessions.append({"file": sf, "header": recs[0], "recs": recs[1:]})
        if sessions:
            runs.append({"dir": d, "name": d.name, "sessions": sessions})
    return runs


def invokes(run):
    for s in run["sessions"]:
        for r in s["recs"]:
            if r.get("action") == "invoke":
                yield s, r


def traces(run):
    for s, r in invokes(run):
        for t in r.get("traces", []):
            if "timing" in t:
                yield s, r, t


def pct(xs, p):
    if not xs:
        return 0.0
    xs = sorted(xs)
    i = min(len(xs) - 1, int(round((p / 100) * (len(xs) - 1))))
    return xs[i]


# -- 1. bugs ----------------------------------------------------------------

def section_bugs(runs, out):
    out.append("## 1. Bugs, gaps and issues\n")
    fails = defaultdict(list)
    for run in runs:
        for s in run["sessions"]:
            for r in s["recs"]:
                if r.get("action") == "check" and r["verdict"] == "fail":
                    fails[(s["header"]["scenario"], r["name"])].append(
                        (run["name"], r))
    if not fails:
        out.append("No failed checks.\n")
    else:
        out.append(f"{len(fails)} distinct failed checks across "
                   f"{len(runs)} corpora.\n")
        out.append("| scenario | check | corpora | expected | observed |")
        out.append("|---|---|---|---|---|")
        for (scen, name), hits in sorted(fails.items()):
            corpora = ", ".join(sorted({c for c, _ in hits}))
            r = hits[0][1]
            out.append(f"| `{scen}` | {name} | {corpora} | "
                       f"`{_short(r['expected'])}` | `{_short(r['observed'])}` |")
        out.append("")

    out.append("### Crashes\n")
    rows = []
    for run in runs:
        for s, r in invokes(run):
            code = r.get("exit")
            if r.get("timed_out"):
                rows.append((run["name"], s["header"]["scenario"], "TIMEOUT",
                             " ".join(map(str, r["argv"][:6]))))
            elif code is not None and (code < 0 or code not in (0, 1, 2)):
                rows.append((run["name"], s["header"]["scenario"], str(code),
                             " ".join(map(str, r["argv"][:6]))))
    if not rows:
        out.append("None.\n")
    else:
        by = defaultdict(int)
        for c, scen, code, argv in rows:
            by[(scen, code)] += 1
        out.append(f"**{len(rows)} invocations exited outside the documented "
                   f"0/1/2 contract.**\n")
        out.append("| scenario | exit | count | reproducer |")
        out.append("|---|---|---|---|")
        seen = set()
        for c, scen, code, argv in rows:
            if (scen, code) in seen:
                continue
            seen.add((scen, code))
            out.append(f"| `{scen}` | {code} | {by[(scen, code)]} | `{argv} ...` |")
        out.append("")


def _short(v, n=60):
    s = json.dumps(v) if not isinstance(v, str) else v
    return (s[:n] + "…") if len(s) > n else s


# -- 2. expectations --------------------------------------------------------

def section_expectations(runs, out):
    out.append("## 2. Did the results match expectations?\n")
    out.append("Predictions were committed in `eval/sim/PREREGISTER.md` before "
               "any of this ran. `SURPRISE` marks a predicted *failure* that "
               "did not occur, or an observation outside the predicted range — "
               "the most valuable verdict, because it means the mental model "
               "was wrong.\n")
    out.append("| scenario | corpus | checks | passed | failed |")
    out.append("|---|---|---|---|---|")
    for run in runs:
        for s in run["sessions"]:
            checks = [r for r in s["recs"] if r.get("action") == "check"]
            n_ok = sum(1 for r in checks if r["verdict"] == "pass")
            out.append(f"| `{s['header']['scenario']}` | {run['name']} | "
                       f"{len(checks)} | {n_ok} | {len(checks) - n_ok} |")
    out.append("")


# -- 3. bottlenecks ---------------------------------------------------------

def section_bottlenecks(runs, out):
    out.append("## 3. Bottlenecks\n")
    for run in runs:
        # Warm queries only: mixing a cold write-through into the same table
        # makes every percentile a statement about how many builds happened.
        warm, cold = [], []
        for _, _, t in traces(run):
            if t.get("kind") != "search":
                continue
            (warm if t["resolution"]["path_taken"] == "warm" else cold).append(t)
        if not warm and not cold:
            continue
        out.append(f"### {run['name']}\n")
        for label, group in (("warm", warm), ("cold write-through", cold)):
            if not group:
                continue
            totals = [t["timing"]["total_ms"] for t in group]
            unattr = [t["timing"]["unattributed_ms"] for t in group]
            out.append(f"**{label}** — n={len(group)}, "
                       f"total p50 {pct(totals,50):.1f}ms / p90 {pct(totals,90):.1f}ms "
                       f"/ p99 {pct(totals,99):.1f}ms · "
                       f"unattributed p50 {pct(unattr,50):.2f}ms "
                       f"({100*pct(unattr,50)/max(pct(totals,50),1e-9):.1f}%)\n")
            per = defaultdict(list)
            for t in group:
                for st in t["timing"]["stages"]:
                    per[st["stage"]].append(st["ms"])
            out.append("| stage | p50 ms | p90 ms | share of p50 total |")
            out.append("|---|---|---|---|")
            ranked = sorted(per.items(), key=lambda kv: -pct(kv[1], 50))
            for stage, xs in ranked:
                if pct(xs, 50) <= 0.0 and pct(xs, 90) <= 0.0:
                    continue
                out.append(f"| `{stage}` | {pct(xs,50):.2f} | {pct(xs,90):.2f} | "
                           f"{100*pct(xs,50)/max(pct(totals,50),1e-9):.1f}% |")
            out.append("")


# -- 4. inefficiencies ------------------------------------------------------

def section_inefficiencies(runs, out):
    out.append("## 4. Inefficiencies\n")
    out.append("| measure | value | where |")
    out.append("|---|---|---|")

    dc = defaultdict(list)
    for run in runs:
        for _, _, t in traces(run):
            if t.get("kind") != "search":
                continue
            dc[t["resolution"]["path_taken"]].append(
                t["resolution"]["discover_calls"])
    for path, xs in sorted(dc.items()):
        if xs:
            out.append(f"| index resolutions per `{path}` command | "
                       f"median {statistics.median(xs):.0f} (max {max(xs)}) | "
                       f"`cache::discover` |")

    # How much of a warm query is spent re-reading an index the previous
    # process already read.
    shares = []
    for run in runs:
        for _, _, t in traces(run):
            if t.get("kind") == "search" and t["resolution"]["path_taken"] == "warm":
                tot = t["timing"]["total_ms"]
                if tot > 0:
                    shares.append(t["timing"]["buckets"]["load_ms"] / tot)
    if shares:
        out.append(f"| warm query time spent loading the index | "
                   f"median {100*statistics.median(shares):.0f}% | "
                   f"`store::load` — nothing is resident between processes |")

    # Repair that is redone every query because the overlay is never written.
    redone = 0
    for run in runs:
        for _, _, t in traces(run):
            if t.get("repair", {}).get("outcome") == "repaired":
                redone += 1
    out.append(f"| queries that rebuilt a read-repair overlay from scratch | "
               f"{redone} | `cache::repair` never writes back |")

    # The suggestion search nobody asked for.
    sug = [t for run in runs for _, _, t in traces(run)
           if t.get("phase") == "suggest"]
    if sug:
        ms = [t["timing"]["total_ms"] for t in sug]
        out.append(f"| extra full searches run by failed `-e` | {len(sug)} "
                   f"(median {statistics.median(ms):.1f}ms each) | "
                   f"`cmd/search.rs::suggest_ranked_alternatives` |")
    out.append("")


# -- 5. hardening -----------------------------------------------------------

HARDENING = [
    ("P0", "`FlatBm25::open` accepts any file ≥64 bytes with the right magic, "
           "then uses five header offsets as unchecked slice indices.",
     "`crates/gorp-core/src/store/bm25.rs:82`",
     "Panic, not `Err` — so the disposability path in `search/mod.rs:173` never "
     "runs, the entry is never evicted, and every later invocation panics too. "
     "Validate the offsets against `map.len()` and return `Err`."),
    ("P1", "Concurrent first-searches of one scope race.",
     "`crates/gorp-core/src/cache/mod.rs:153` (FIXES.md #3)",
     "Build into a staging directory and rename, so a reader never sees a "
     "half-written entry."),
    ("P1", "Read-repair has no delta-size bound.",
     "`crates/gorp-core/src/cache/repair.rs:99`",
     "RESEARCH.md §8 mechanism 2 specifies treating drift above ~5% of files as "
     "a full miss. Unimplemented, so a branch switch makes every query past the "
     "TTL re-embed the whole delta, forever."),
    ("P2", "LRU eviction pops a victim before deleting it and only decrements "
           "the running total on success.",
     "`crates/gorp-core/src/cache/budget.rs:115-122`",
     "An undeletable entry survives while healthy entries are destroyed and the "
     "cache stays over budget, silently. Decrement only what was freed, and "
     "stop when no progress is possible."),
    ("P2", "`out::hits` writes `println!(\"{}:{}:{}\")` with no escaping.",
     "`crates/gorp/src/out.rs:42`",
     "A filename containing a newline splits one hit across two stdout lines, "
     "breaking the documented `path:line:text` contract that 'stdout is data' "
     "rests on. `--json` is unaffected."),
    ("P2", "A nonexistent search path exits 1 (\"no results\") rather than 2.",
     "`crates/gorp-core/src/corpus/mod.rs` walk / `cmd/search.rs`",
     "An agent reads \"no results\" as \"the code is not there\" and stops "
     "looking, when the path was simply wrong."),
    ("P3", "A same-second, length-preserving edit is invisible to drift "
           "detection.",
     "`crates/gorp-core/src/corpus/diff.rs:53`",
     "`(size, mtime)` with whole-second mtime cannot see it. Documented as a "
     "known limit; worth a content hash for files whose mtime equals the "
     "index's own."),
    ("P3", "`budget::dir_bytes` is not recursive.",
     "`crates/gorp-core/src/cache/budget.rs:40`",
     "Correct only because entries happen to be flat. A nested artifact would "
     "be sized as zero and never reclaimed."),
    ("P3", "`warn_if_first_search` resolves the index on every ranked query.",
     "`crates/gorp/src/cmd/search.rs:90`",
     "One extra canonicalize plus generation-directory scan per query, to "
     "decide whether to print one line the engine could report itself."),
]


def section_hardening(out):
    out.append("## 5. Hardening\n")
    out.append("| pri | issue | site | why / fix |")
    out.append("|---|---|---|---|")
    for pri, issue, site, why in HARDENING:
        out.append(f"| {pri} | {issue} | {site} | {why} |")
    out.append("")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--runs", default="")
    ap.add_argument("--out", default=str(RESULTS / "INDEX.md"))
    ap.add_argument("--check", action="store_true",
                    help="fail if the generated report is out of date")
    args = ap.parse_args()

    dirs = sorted(d for d in RESULTS.iterdir() if d.is_dir()) if RESULTS.exists() else []
    if args.runs:
        want = {r.strip() for r in args.runs.split(",")}
        dirs = [d for d in dirs if d.name in want]
    runs = load(dirs)
    if not runs:
        raise SystemExit("no sessions found; run eval/sim/run.py first")

    out = ["# Simulation results",
           "",
           "Generated by `eval/sim/report.py` from the session files. "
           "Never hand-edited; if this disagrees with the JSONL, the JSONL is "
           "right.",
           "",
           f"Runs: {', '.join(r['name'] for r in runs)}",
           ""]
    section_bugs(runs, out)
    section_expectations(runs, out)
    section_bottlenecks(runs, out)
    section_inefficiencies(runs, out)
    section_hardening(out)
    text = "\n".join(out) + "\n"

    dest = Path(args.out)
    if args.check:
        if not dest.exists() or dest.read_text() != text:
            raise SystemExit(f"{dest} is out of date — re-run eval/sim/report.py")
        print(f"{dest} is up to date")
        return
    dest.write_text(text)
    print(f"wrote {dest} ({len(text)} bytes)")


if __name__ == "__main__":
    main()
