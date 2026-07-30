# eval/queries — the query sets, checked in

Every retrieval number in `RESEARCH.md` and `RESULTS.md` was produced from one
of these files. They used to live in `eval/data/`, which is gitignored, and
they are `claude`-generated — so they could not be regenerated identically and
nothing in the published results was reproducible from the repo alone. They are
400 KB. They belong in git.

`MANIFEST.json` records, per set: sha256, the `queries_fp` `run_eval.py`
stamps into every result row, the corpus it is ground truth for, how it was
anchored, and its leakage profile. A result file whose `queries_fp` is not in
the manifest was scored against a set that no longer exists.

## What each set is, and what it is good for

| set | n | corpus | queries written by | use it for |
|---|---|---|---|---|
| `linux.jsonl` | 398 | linux | `claude`, from the gold chunk | C, identifier-heavy |
| `vscode.jsonl` | 400 | vscode | `claude`, from the gold chunk | TS/JS |
| `wikipedia.jsonl` | 400 | wikipedia | `claude`, from the gold chunk | prose control |
| `cosqa-1200.jsonl` | 1200 | cosqa | **real humans** (Bing logs) | **quality claims** |
| `linux-150.jsonl` | 150 | linux | subset of `linux.jsonl` | quick iteration |
| `vscode-pilot.jsonl` | 20 | vscode | pilot subset | smoke tests |

**Prefer `cosqa-1200.jsonl` for anything published.** It is the only set here
nobody on this project wrote, and RESEARCH.md §12.5 made it the first-class
one for that reason.

## Two known biases, both measured

**Window anchoring (§11.4).** The three `claude`-generated sets define ground
truth as a fixed 30-line window, and queries were written to be answerable by
that window — which often spans two or three functions that no single function
chunk contains. **The eval's ground truth is one of the strategies under
test**, so these sets cannot referee a chunking change. `generate.py --anchor
symbol` produces chunking-neutral sets; no set here uses it yet.

**Leakage (§12.3, and the path column added 2026-07-30).** Run
`python3 eval/leakage.py eval/queries/<set>.jsonl bench/corpora/<corpus>` for
the live table, or read it out of `MANIFEST.json`. Summary:

| set | kind | ident% | med words | stem% | dirseg% | pathseg-not-in-gold% |
|---|---|---|---|---|---|---|
| linux | direct | 66.3% | 10 | 32.7% | 48.2% | 16.1% |
| linux | paraphrase | 2.0% | 17 | 0.0% | 25.1% | 17.1% |
| vscode | direct | 70.5% | 10 | 22.5% | 46.0% | 12.0% |
| vscode | paraphrase | 1.5% | 18 | 0.0% | 26.5% | 12.0% |
| wikipedia | both | ≤1% | 11-15 | 0.0% | 0.0% | 0.0% |
| cosqa-1200 | real | 0.0% | 6 | — | — | — |

`direct` hands the tool the answer's name; `paraphrase` strips vocabulary a
user would actually type and runs 17 words where real users type 6. Neither is
where users are.

The path columns are the leak §12 did not measure: `generate.py` passed the
file path into the generator prompt while semgrep's tokenizer does path
augmentation. Note that `pathseg-not-in-gold` is **as high for `paraphrase` as
for `direct`** — the generator was told to avoid the chunk's identifiers, and
nothing told it to avoid the path. So `paraphrase` is not the clean pole
§12.3 took it for.

Caveat on `stem%`: in C a file stem and an identifier prefix are often the same
token (`blkg-rwstat.c` ↔ `blkg_rwstat_add`), so that column partly re-measures
identifier leakage. `pathseg-not-in-gold` is the column that isolates it.

## Regenerating

Don't, unless you mean to — a regenerated set is a *different* set, and
`queries_fp` will refuse to compare it against any existing baseline. That
refusal is the feature. To make a new set:

    python3 eval/generate.py bench/corpora/<corpus> --anchor symbol \
        --n 200 --out eval/queries/<name>.jsonl

then re-run the manifest generator and commit both.
