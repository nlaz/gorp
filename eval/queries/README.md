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

| set | n | corpus | anchor | queries written by | use it for |
|---|---|---|---|---|---|
| `cosqa-1200.jsonl` | 1200 | cosqa | whole file | **real humans** (Bing logs) | **quality claims** |
| `replay-agent.jsonl` | 497 | locbench trees | n/a | **real agents** (shim logs) | **what actually gets typed** |
| `tokio.jsonl` | 400 | tokio | symbol | `claude`, no path shown | rust |
| `commons-lang.jsonl` | 398 | commons-lang | symbol | `claude`, no path shown | java |
| `etcd.jsonl` | 400 | etcd | symbol | `claude`, no path shown | go |
| `jekyll.jsonl` | 176 | jekyll | symbol | `claude`, no path shown | ruby |
| `linux.jsonl` | 398 | linux | window | `claude`, path shown | C, identifier-heavy |
| `vscode.jsonl` | 400 | vscode | window | `claude`, path shown | TS/JS |
| `wikipedia.jsonl` | 400 | wikipedia | window | `claude`, path shown | prose control |
| `linux-150.jsonl` | 150 | linux | window | subset of `linux.jsonl` | quick iteration |
| `vscode-pilot.jsonl` | 20 | vscode | window | pilot subset | smoke tests |

**Prefer the two at the top for anything published.** They are the only sets
here nobody on this project wrote — `cosqa-1200` is where human users live
(§12.5 made it first-class for that reason) and `replay-agent` is where this
product's actual input lives (§13.2). Everything else we wrote, and §12.5's
standing suspicion of self-written queries applies.

The four symbol-anchored sets are the best of the ones we did write: ground
truth is a function span, so they can referee a chunking change, and the
generator was never shown the file path.

## Two known biases, both measured

**Window anchoring (§11.4).** The linux/vscode/wikipedia sets define ground
truth as a fixed 30-line window, and queries were written to be answerable by
that window — which often spans two or three functions that no single function
chunk contains. **The eval's ground truth is one of the strategies under
test**, so these sets cannot referee a chunking change. The four language sets
use `--anchor symbol` and do not have this problem.

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
| **tokio** | direct | 30.0% | 12 | 15.5% | 10.0% | **1.0%** |
| **tokio** | paraphrase | 0.5% | 14 | 5.0% | 5.0% | **2.5%** |
| **commons-lang** | direct | 39.2% | 11 | 3.5% | 33.7% | **6.5%** |
| **commons-lang** | paraphrase | 1.0% | 15 | 0.0% | 22.1% | **12.1%** |
| **etcd** | direct | 9.5% | 12 | 14.5% | 16.5% | **2.5%** |
| **etcd** | paraphrase | 0.0% | 15 | 2.0% | 10.0% | **5.0%** |
| **jekyll** | direct | 5.7% | 12 | 14.8% | 6.8% | **3.4%** |
| **jekyll** | paraphrase | 0.0% | 15 | 3.4% | 6.8% | **4.5%** |
| cosqa-1200 | real | 0.0% | 6 | — | — | — |
| replay-agent | real | 47% | 4 | — | — | — |

`direct` hands the tool the answer's name; `paraphrase` strips vocabulary a
user would actually type and runs 17 words where real users type 6, and agents
type 4. Neither pole is where anyone is.

The path columns are the leak §12 did not measure: `generate.py` passed the
file path into the generator prompt while semgrep's tokenizer does path
augmentation. Two things to read off the table:

- **It was as bad for `paraphrase` as for `direct`** on the old sets (17.1% vs
  16.1% on linux). The generator was told to avoid the chunk's identifiers, and
  nothing told it to avoid the path, so it reached for the one piece of the
  answer it could still see. `paraphrase` is not the clean pole §12.3 took it
  for.
- **Removing `{path}` from the prompt fixed it**: 12–17% → 1–6.5% on the four
  new sets.

commons-lang's `paraphrase` still reads 12.1%, and that one is mostly the
metric's fault: paths like `src/main/java/org/apache/commons/lang3/…` contribute
segments — "java", "test", "main" — that appear in ordinary English questions
about Java code. Read it as a ceiling on that corpus, not as 12% of real
leakage.

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
