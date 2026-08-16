#!/usr/bin/env python3
"""Run the simulation scenarios and write one session file per scenario.

    python3 eval/sim/run.py                       # every tier-1 scenario
    python3 eval/sim/run.py --tier 2              # include the slow ones
    python3 eval/sim/run.py --only s4,s8          # by name prefix
    python3 eval/sim/run.py --corpus bench/corpora/tokio

Sessions land in `eval/sim/results/<run_id>/<scenario>/session.jsonl` and are
checked in; the scratch corpora and cache directories they build are not.

Nothing here touches the developer's real cache: every session gets its own
`GORP_CACHE_DIR` under its own output directory, and the path is printed.
"""

import argparse
import json
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
sys.path.insert(0, str(HERE))

import corpora                                                   # noqa: E402
import scenarios                                                 # noqa: E402
from harness import GORP, Session                             # noqa: E402


def build_context(work, corpus_arg):
    """The world the scenarios search. Built once and shared.

    A scenario that mutates its corpus gets a private copy (see `run_one`);
    this is the pristine one.
    """
    ctx = {"bin": GORP}
    if corpus_arg:
        ctx["source_root"] = Path(corpus_arg).resolve()
        ctx["source_kind"] = "external"
    else:
        src = work / "corpus"
        manifest = corpora.plain(src, n_files=60, seed=1)
        ctx["source_root"] = src
        ctx["source_kind"] = "synthetic"
        ctx["manifest"] = manifest

    adv = work / "adversarial"
    ctx["adversarial_manifest"] = corpora.adversarial(adv)
    ctx["adversarial"] = adv

    # Four small distinct corpora, for budget/LRU scenarios that need several
    # entries at once.
    multi = []
    for i in range(4):
        d = work / f"multi{i}"
        corpora.plain(d, n_files=20, seed=100 + i)
        multi.append(d)
    ctx["multi_roots"] = multi
    # A root no scenario indexes, kept aside so a scenario that needs to force
    # the *write* path (and therefore the budget enforcer, which only runs
    # inside `write_cache_entry`) has something guaranteed cold.
    fresh = work / "fresh"
    corpora.plain(fresh, n_files=20, seed=999)
    ctx["fresh_root"] = fresh
    return ctx


def run_one(spec, out_root, ctx, work, timeout_note):
    name = spec["name"]
    out = out_root / name
    if out.exists():
        shutil.rmtree(out)
    out.mkdir(parents=True)

    # A private copy of the corpus per scenario: several of these mutate the
    # tree, and a scenario that inherits another's drift is not the scenario
    # that was written down.
    root = work / "scratch" / name
    root.parent.mkdir(parents=True, exist_ok=True)
    if root.exists():
        shutil.rmtree(root)
    # `.gorp` is excluded deliberately. A bench corpus usually has one left
    # over from a previous run, and copying it makes every "first search of this
    # scope" resolve warm against a repo-local index — which silently turned the
    # cold-start and fault-injection scenarios into no-ops that still reported
    # numbers. `.git` is excluded because it is large and never searched.
    shutil.copytree(ctx["source_root"], root,
                    ignore=shutil.ignore_patterns(".gorp", ".git"),
                    symlinks=True)

    local = dict(ctx)
    local["root"] = root

    sess = Session(name, out, spec["expect"], root, tier=spec["tier"],
                   notes=spec["notes"])
    t0 = time.monotonic()
    error = None
    try:
        spec["fn"](sess, local)
    except Exception as e:                                       # noqa: BLE001
        import traceback
        error = traceback.format_exc()
        sess.mutate("scenario-error", fn=lambda: {"traceback": error})
    wall = time.monotonic() - t0
    n_fail = sess.close()
    shutil.rmtree(root, ignore_errors=True)

    status = "ERROR" if error else ("FAIL" if n_fail else "ok")
    print(f"  {name:34s} {status:5s} {n_fail} failed check(s)  {wall:6.1f}s")
    if error:
        print("    " + error.strip().splitlines()[-1])
    return {"scenario": name, "tier": spec["tier"], "wall_s": round(wall, 2),
            "n_checks": len(sess.checks), "n_failed": n_fail,
            "error": error is not None}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--tier", type=int, default=1,
                    help="run scenarios at this tier and below")
    ap.add_argument("--only", default="", help="comma-separated name prefixes")
    ap.add_argument("--corpus", default="",
                    help="search this tree instead of a synthetic one")
    ap.add_argument("--out", default=str(HERE / "results"))
    ap.add_argument("--run-id", default="")
    args = ap.parse_args()

    if not GORP.exists():
        raise SystemExit(f"no binary at {GORP} — run `cargo build --release`")

    run_id = args.run_id or time.strftime("%Y%m%d-%H%M%S")
    out_root = Path(args.out) / run_id
    out_root.mkdir(parents=True, exist_ok=True)

    # Scratch lives under eval/data/, which is gitignored — corpora and caches
    # are large and reproducible; the sessions are small and are the evidence.
    work = ROOT / "eval" / "data" / "sim" / run_id
    if work.exists():
        shutil.rmtree(work)
    work.mkdir(parents=True)

    picked = [s for s in scenarios.REGISTRY if s["tier"] <= args.tier]
    if args.only:
        prefixes = tuple(p.strip() for p in args.only.split(",") if p.strip())
        picked = [s for s in picked if s["name"].startswith(prefixes)]
    if not picked:
        raise SystemExit("no scenarios selected")

    print(f"run {run_id}: {len(picked)} scenario(s), tier<={args.tier}")
    print(f"  binary   {GORP}")
    print(f"  scratch  {work}   (gitignored)")
    print(f"  sessions {out_root}")
    ctx = build_context(work, args.corpus)
    print(f"  corpus   {ctx['source_kind']} at {ctx['source_root']}")
    adv = ctx["adversarial_manifest"]
    print(f"  adversarial entries made: {len(adv['made'])}, "
          f"refused by the filesystem: {sorted(adv['skipped'])}")
    print()

    summary = [run_one(s, out_root, ctx, work, None) for s in picked]

    index = {
        "run_id": run_id,
        "tier": args.tier,
        "corpus": str(ctx["source_root"]),
        "corpus_kind": ctx["source_kind"],
        "adversarial": adv,
        "scenarios": summary,
    }
    (out_root / "index.json").write_text(json.dumps(index, indent=2) + "\n")

    n_fail = sum(s["n_failed"] for s in summary)
    n_err = sum(1 for s in summary if s["error"])
    print(f"\n{len(summary)} scenarios, {n_fail} failed checks, {n_err} harness errors")
    print(f"wrote {out_root}/index.json")
    # Scratch is not removed automatically: a failed scenario's corpus is
    # often what you need to look at. `eval/reclaim.sh` knows where it is.
    print(f"scratch left at {work}")


if __name__ == "__main__":
    main()
