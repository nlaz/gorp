#!/usr/bin/env python3
"""Regenerate eval/queries/MANIFEST.json from the sets on disk.

The manifest existed before its generator did — README.md said "re-run the
manifest generator" and no such script was in the repo, which is how every
recorded `gold_token_overlap` ended up null: the one-off that wrote it never
passed a corpus. This is that generator, kept honest by two rules:

- `queries_fp` must reproduce run_eval.py's formula byte-for-byte
  (sha256(raw + "\\0" + where + "\\0" + stratify)[:16], unfiltered), so a
  manifest row can be joined to any result row ever written.
- `anchor` and `provenance` are curation notes, not measurements: they are
  carried forward from the existing manifest (or supplied by hand for a new
  set), never invented here.

    python3 eval/make_manifest.py            # rewrite MANIFEST.json
    python3 eval/make_manifest.py --check    # fail if it is out of date
"""

import argparse
import hashlib
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
import leakage  # noqa: E402

HERE = Path(__file__).parent
QUERIES = HERE / "queries"
ROOT = HERE.parent

# Corpus name -> tree, mirroring preproc.sh's corpus_dir. A name without a
# tree (replay-agent's worktrees) gets no leakage block rather than a fake one.
CORPUS_DIRS = {
    "cosqa": ROOT / "eval/data/cosqa/corpus",
    "linux": ROOT / "bench/corpora/linux",
    "vscode": ROOT / "bench/corpora/vscode",
    "wikipedia": ROOT / "bench/corpora/wikipedia",
    "tokio": ROOT / "bench/corpora/tokio",
    "etcd": ROOT / "bench/corpora/etcd",
    "jekyll": ROOT / "bench/corpora/jekyll",
    "commons-lang": ROOT / "bench/corpora/commons-lang",
}


def fingerprint(raw):
    return hashlib.sha256((raw + "\0" + "" + "\0" + "").encode()).hexdigest()[:16]


def entry(path, prior):
    raw = path.read_text()
    rows = [json.loads(l) for l in raw.splitlines() if l.strip()]
    fp = fingerprint(raw)
    kinds = {}
    for r in rows:
        kinds[r.get("kind", "?")] = kinds.get(r.get("kind", "?"), 0) + 1
    out = {
        "sha256": fp,
        "queries_fp": fp,
        "n": len(rows),
        "kinds": dict(sorted(kinds.items())),
        "corpus": prior.get("corpus", "?"),
        "anchor": prior.get("anchor", "?"),
        "provenance": prior.get("provenance", "?"),
    }
    tree = CORPUS_DIRS.get(out["corpus"])
    if tree and tree.is_dir():
        out["leakage"] = leakage.summarize(rows, tree)
    elif "leakage" in prior:
        out["leakage"] = prior["leakage"]
    return out


def build():
    prior = json.loads((QUERIES / "MANIFEST.json").read_text())
    manifest = {}
    for path in sorted(QUERIES.glob("*.jsonl")):
        manifest[path.name] = entry(path, prior.get(path.name, {}))
    return manifest


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--check", action="store_true",
                    help="exit non-zero if MANIFEST.json is out of date")
    args = ap.parse_args()

    manifest = build()
    target = QUERIES / "MANIFEST.json"
    rendered = json.dumps(manifest, indent=1) + "\n"
    if args.check:
        if target.read_text() != rendered:
            print("MANIFEST.json is out of date — run `python3 eval/make_manifest.py`")
            sys.exit(1)
        print(f"MANIFEST.json up to date ({len(manifest)} sets)")
        return
    # The fp is the join key to result rows, so a change is worth a line —
    # but the authority is run_eval's formula, not the previous manifest.
    # (Verified 2026-08-01: the original manifest's fps matched NO result row;
    # results.py showed every joined run as "unknown (fp …)". The values this
    # writes are the ones result rows actually carry.)
    old_all = json.loads(target.read_text())
    for name, e in manifest.items():
        old = old_all.get(name, {})
        if old and old.get("queries_fp") != e["queries_fp"]:
            print(f"  fp {name}: {old.get('queries_fp')} -> {e['queries_fp']}")
    target.write_text(rendered)
    print(f"wrote {target}: {len(manifest)} sets")


if __name__ == "__main__":
    main()
