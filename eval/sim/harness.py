#!/usr/bin/env python3
"""Running semgrep as a *session*, and recording what happened.

`eval/` scores single queries and `bench/` times single queries. Neither can see
behavior that only exists across a sequence: the index is a cache (RESEARCH.md
§8), so write-through, read-repair, the TTL gate, LRU eviction and corrupt-entry
disposal all need step N to change what step N+1 sees.

A **session** is an ordered sequence of steps against one corpus root under one
isolated `GORP_CACHE_DIR`. A **step** is one of:

  mutate   the world changes (edit files, corrupt an artifact, shrink a budget)
  invoke   one semgrep process runs
  check    a pre-registered expectation is evaluated against everything so far

Sessions are written as JSONL: line 0 is a header pinning the scenario, its
expectations, and provenance; the rest are step records. Header-pinned,
flush-per-record, torn-last-line tolerant — the discipline `run_eval.Checkpoint`
established, for the same reason (a run that dies partway should leave readable
evidence, not a file nobody can trust).

Stdlib only, like every other harness here.
"""

import json
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
sys.path.insert(0, str(ROOT / "eval"))
sys.path.insert(0, str(ROOT / "bench"))

GORP = Path(os.environ.get("GORP_BIN", ROOT / "target/release/gorp"))

# Cap on captured output per step. Sessions are checked into git, and a keyword
# scan of a large corpus can emit megabytes; the shape of the output is what
# matters here, not every byte of it: `stdout_bytes` keeps the true size and
# `--json` steps keep parsed `hits`, so the capture is evidence rather than data.
# Steps record `stdout_truncated` so a check that parses output can tell it is
# looking at a cut — see `Step.stdout_lines`.
MAX_CAPTURE = 2 * 1024

# Envelope fields that are identical on every invocation of a session and are
# already in its header. Dropping them from the step records is what keeps the
# checked-in sessions under a megabyte instead of eight; nothing derived from
# them is lost, because the header pins the binary and the environment for the
# whole session.
ENVELOPE_DROP = ("binary", "env", "argv", "cwd", "pid")


def _provenance():
    """Which binary, which machine, which commit — merged from the harnesses
    that already answer each part, rather than a fourth implementation."""
    block = {}
    try:
        import run_eval
        block.update(run_eval.binary_provenance())
    except Exception as e:                                   # noqa: BLE001
        block["binary_provenance_error"] = repr(e)
    try:
        import run as bench_run
        block["machine"] = bench_run.provenance()
    except Exception as e:                                   # noqa: BLE001
        block["machine_provenance_error"] = repr(e)
    block["at"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
    return block


class Step:
    """One recorded step. Kept as a plain dict under the hood so the session
    file and the in-memory value a scenario reasons about are the same thing."""

    def __init__(self, rec):
        self.rec = rec

    def __getitem__(self, k):
        return self.rec[k]

    def get(self, k, default=None):
        return self.rec.get(k, default)

    @property
    def hits(self):
        return self.rec.get("hits", [])

    @property
    def traces(self):
        return self.rec.get("traces", [])

    @property
    def trace(self):
        """The primary engine invocation, or None. Most checks want this one;
        `traces` is there for the exact-miss case that makes two."""
        for t in self.traces:
            if t.get("phase") == "primary":
                return t
        return self.traces[0] if self.traces else None

    def stage_ms(self, name):
        t = self.trace
        if not t:
            return 0.0
        for s in t.get("timing", {}).get("stages", []):
            if s["stage"] == name:
                return s["ms"]
        return 0.0

    def bucket_ms(self, name):
        t = self.trace
        return (t or {}).get("timing", {}).get("buckets", {}).get(f"{name}_ms", 0.0)

    def stdout_lines(self):
        """Complete lines of stdout only.

        Captured stdout is capped at `MAX_CAPTURE`, so when a step overflows it
        the final line is torn *by this harness*. A check that parses output
        must not see that: the first version of the adversarial scenario
        reported "semgrep emits invalid JSON" when what it had actually found
        was its own 64 KB cut. Any assertion about output format has to run on
        whole lines or it is measuring the recorder.
        """
        lines = self.rec.get("stdout", "").splitlines()
        if self.rec.get("stdout_truncated") and lines:
            lines = lines[:-1]
        return lines

    @property
    def crashed(self):
        """Anything that is not the documented 0/1/2 contract, or a signal.

        A negative code is a signal on POSIX (`-6` = SIGABRT, which is what a
        Rust panic reaching the process boundary looks like after abort; `101`
        is the ordinary panic exit). Exit 2 is a *reported* error and is not a
        crash — that is the engine working.
        """
        c = self.rec.get("exit")
        return c is not None and (c < 0 or c not in (0, 1, 2))


class Session:
    """One scenario run. Owns the cache dir, the trace files, and the log."""

    def __init__(self, name, out_dir, expectations, corpus_root, env=None,
                 tier=1, notes=""):
        self.name = name
        self.dir = Path(out_dir)
        self.dir.mkdir(parents=True, exist_ok=True)
        self.corpus_root = Path(corpus_root)
        # Isolated, and its path recorded: a scenario answered from another
        # scenario's entry is not the scenario you wrote.
        self.cache = self.dir / "cache"
        self.cache.mkdir(exist_ok=True)
        self.trace_dir = self.dir / "traces"
        self.trace_dir.mkdir(exist_ok=True)
        self.env = dict(env or {})
        self.n = 0
        self.checks = []

        self.path = self.dir / "session.jsonl"
        self.fh = self.path.open("w")
        self._write({
            "kind": "session",
            "scenario": name,
            "tier": tier,
            "notes": notes,
            # Copied verbatim from scenarios.py so the session file carries the
            # prediction it is being judged against. A results file that does
            # not contain its own hypothesis can be reinterpreted later.
            "expectations": expectations,
            "corpus_root": str(self.corpus_root),
            "cache_dir": str(self.cache),
            "env": self.env,
            "provenance": _provenance(),
        })

    def _write(self, rec):
        self.fh.write(json.dumps(rec, default=str) + "\n")
        # Flush per record: a scenario that hangs or is killed should still
        # leave every step it completed.
        self.fh.flush()

    # -- steps --------------------------------------------------------------

    def mutate(self, op, fn=None, **kw):
        """Record a change to the world. `fn` performs it."""
        self.n += 1
        detail = {}
        error = None
        try:
            if fn is not None:
                detail = fn() or {}
        except Exception as e:                               # noqa: BLE001
            error = repr(e)
        rec = {"kind": "step", "step": self.n, "action": "mutate", "op": op,
               "args": {k: str(v) for k, v in kw.items()}, "detail": detail,
               "error": error, "ts": time.time()}
        self._write(rec)
        return Step(rec)

    # Sentinel for "this verb takes no path at all", distinct from "no path
    # given, use the corpus root". `gorp cache` is the case: passing it a path
    # is a clap usage error, so a scenario that meant to inspect the cache got
    # exit 2 and an empty stdout — and then *passed*, because its check was
    # looking for something to be absent from that stdout. Silently testing
    # nothing is the failure mode this harness is most prone to (SIMULATION.md
    # §5), so the two meanings of "no path" are now different values.
    NO_PATH = object()

    def run(self, args, path=None, timeout=300, env=None, label=""):
        """One semgrep invocation, with its trace envelopes attached.

        `path=None` searches the session's corpus root; `path=Session.NO_PATH`
        appends no path argument at all, for verbs that do not take one.

        The trace file is per-step: the driver sets `GORP_TRACE_FILE`, the
        binary appends one object per *engine* invocation, and this reads it
        back. That is how the second full search an exact-mode miss runs shows
        up at all — no flag on the outer command can reach it.
        """
        self.n += 1
        trace_path = self.trace_dir / f"{self.n:03d}.jsonl"
        target = None if path is Session.NO_PATH else (
            Path(path) if path is not None else self.corpus_root)

        e = dict(os.environ)
        e["GORP_CACHE_DIR"] = str(self.cache)
        e["GORP_TRACE_FILE"] = str(trace_path)
        e["GORP_SESSION_ID"] = self.name
        e.update(self.env)
        e.update(env or {})

        argv = [str(GORP), *args]
        if target is not None:
            argv.append(str(target))
        t0 = time.monotonic()
        timed_out = False
        try:
            proc = subprocess.run(argv, capture_output=True, text=True,
                                  errors="replace", env=e, timeout=timeout)
            code, out, err = proc.returncode, proc.stdout, proc.stderr
        except subprocess.TimeoutExpired as ex:
            timed_out = True
            code = None
            out = (ex.stdout or b"").decode("utf-8", "replace") if isinstance(ex.stdout, bytes) else (ex.stdout or "")
            err = (ex.stderr or b"").decode("utf-8", "replace") if isinstance(ex.stderr, bytes) else (ex.stderr or "")
        wall_ms = (time.monotonic() - t0) * 1e3

        hits = []
        if "--json" in args:
            for line in out.splitlines():
                try:
                    h = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if "path" in h:
                    hits.append({"path": h["path"], "line": h.get("line"),
                                 "score": h.get("score")})

        traces = []
        if trace_path.exists():
            for line in trace_path.read_text(errors="replace").splitlines():
                try:
                    env_rec = json.loads(line)
                except json.JSONDecodeError:
                    # A torn line means the binary died mid-write, which is
                    # itself a finding — recorded, not skipped silently.
                    traces.append({"_torn": line[:200]})
                    continue
                for k in ENVELOPE_DROP:
                    env_rec.pop(k, None)
                traces.append(env_rec)

        rec = {
            "kind": "step", "step": self.n, "action": "invoke", "label": label,
            "argv": argv[1:], "exit": code, "timed_out": timed_out,
            "wall_ms": round(wall_ms, 2),
            "stdout_bytes": len(out), "stderr_bytes": len(err),
            "stdout_truncated": len(out) > MAX_CAPTURE,
            "stderr_truncated": len(err) > MAX_CAPTURE,
            "stdout": out[:MAX_CAPTURE], "stderr": err[:MAX_CAPTURE],
            "n_hits": len(hits), "hits": hits[:25],
            "traces": traces, "ts": time.time(),
        }
        self._write(rec)
        return Step(rec)

    def check(self, name, expected, observed, ok=None, note=""):
        """Evaluate one pre-registered expectation.

        `ok=None` means "compare expected to observed"; pass an explicit bool
        when the predicate is richer than equality. A check that raises while
        being evaluated is recorded as an error rather than killing the run —
        a scenario should produce evidence even when it surprises the harness.
        """
        self.n += 1
        try:
            verdict = bool(observed == expected) if ok is None else bool(ok)
            err = None
        except Exception as ex:                              # noqa: BLE001
            verdict, err = False, repr(ex)
        rec = {"kind": "step", "step": self.n, "action": "check", "name": name,
               "expected": expected, "observed": observed,
               "verdict": "pass" if verdict else "fail", "note": note,
               "error": err, "ts": time.time()}
        self._write(rec)
        self.checks.append(rec)
        return verdict

    def close(self):
        n_fail = sum(1 for c in self.checks if c["verdict"] == "fail")
        self._write({"kind": "summary", "n_steps": self.n,
                     "n_checks": len(self.checks), "n_failed": n_fail})
        self.fh.close()
        # Cache dirs are large and reproducible, and the per-step trace files
        # have already been inlined into the step records — keeping both would
        # double the size of a checked-in session for no extra evidence.
        # The session file is what gets committed.
        shutil.rmtree(self.cache, ignore_errors=True)
        shutil.rmtree(self.trace_dir, ignore_errors=True)
        return n_fail
