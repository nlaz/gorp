#!/usr/bin/env python3
"""Corpora to search, ways to disturb them, and ways to corrupt an index.

Three groups:

  `plain`        an ordinary code-shaped tree, deterministic in its seed, used
                 wherever a scenario needs "a corpus" and not a specific one.
  `adversarial`  the zoo — every input shape that has historically broken a
                 file walker. Built with a manifest so a scenario can say which
                 entries actually materialized on this filesystem.
  `faults`       corruption applied to a *built* index, to test the claim that
                 a cache entry is disposable.

Mutations all change file **size**, not just content. `FileMeta.mtime` is whole
seconds (`lib.rs`) and `corpus::diff` compares `(size, mtime)`, so a same-second
same-length edit is invisible to drift detection. That is a real defect — S3d
tests for it deliberately — but every *other* scenario must avoid tripping it or
it measures the wrong thing.
"""

import hashlib
import os
import random
import shutil
from pathlib import Path

LANGS = {
    "py": '''"""Module {i}: {topic}."""


def {fn}(attempt, ceiling={i}):
    """Compute the {topic} for a retry attempt."""
    base = {i} + 1
    return min(base * (2 ** attempt), ceiling * 1000)


class {cls}:
    """Tracks {topic} state across calls."""

    def __init__(self):
        self.failures = 0
        self.opened_at = None

    def record_failure(self):
        self.failures += 1
        return self.failures
''',
    "rs": '''//! Module {i}: {topic}.

/// Compute the {topic} for a retry attempt.
pub fn {fn}(attempt: u32, ceiling: u64) -> u64 {{
    let base = {i} + 1;
    std::cmp::min(base * (1 << attempt), ceiling * 1000)
}}

/// Tracks {topic} state across calls.
pub struct {cls} {{
    pub failures: u32,
    pub opened_at: Option<u64>,
}}

impl {cls} {{
    pub fn record_failure(&mut self) -> u32 {{
        self.failures += 1;
        self.failures
    }}
}}
''',
    "go": '''// Package m{i} implements {topic}.
package m{i}

// {fn} computes the {topic} for a retry attempt.
func {fn}(attempt uint, ceiling uint64) uint64 {{
	base := uint64({i} + 1)
	v := base << attempt
	if v > ceiling*1000 {{
		return ceiling * 1000
	}}
	return v
}}

// {cls} tracks {topic} state across calls.
type {cls} struct {{
	Failures uint32
	OpenedAt *int64
}}
''',
}

TOPICS = [
    "exponential backoff", "circuit breaker threshold", "connection pool sizing",
    "token bucket refill", "lease renewal interval", "cache eviction weight",
    "retry jitter", "heartbeat deadline", "shard rebalance cost",
    "write-ahead flush cadence",
]


def plain(dest, n_files=60, seed=1):
    """A deterministic code-shaped corpus. Returns the manifest."""
    dest = Path(dest)
    rng = random.Random(seed)
    made = []
    for i in range(n_files):
        lang = ["py", "rs", "go"][i % 3]
        topic = TOPICS[i % len(TOPICS)]
        sub = ["core", "net", "store", "util"][i % 4]
        d = dest / sub
        d.mkdir(parents=True, exist_ok=True)
        p = d / f"m{i}.{lang}"
        p.write_text(LANGS[lang].format(
            i=i, topic=topic,
            fn=f"compute_{topic.split()[0]}_{i}",
            cls=f"{topic.split()[0].capitalize()}Tracker{i}",
        ))
        made.append(str(p.relative_to(dest)))
    # A little prose, so bm25 and the embedder both have something non-code.
    (dest / "README.md").write_text(
        "# Retry and backoff\n\n"
        "This service retries failed calls with exponential backoff and jitter.\n"
        "After the circuit breaker threshold is exceeded, calls fail fast.\n" * 4)
    made.append("README.md")
    rng.shuffle(made)
    return {"n_files": len(made), "files": made}


def adversarial(dest):
    """Every input shape that has historically broken a walker.

    Returns a manifest of what actually got created: FIFOs, `chmod 000` and
    exotic filenames are all filesystem-dependent, and a scenario must not
    claim to have tested something the filesystem refused to make.
    """
    dest = Path(dest)
    dest.mkdir(parents=True, exist_ok=True)
    made, skipped = {}, {}

    def attempt(name, fn):
        try:
            fn()
            made[name] = True
        except (OSError, ValueError, UnicodeEncodeError) as e:
            skipped[name] = repr(e)

    # A normal file, so there is always something findable.
    (dest / "normal.py").write_text(
        "def compute_backoff(attempt):\n    return 2 ** attempt\n")

    attempt("binary_nul", lambda: (dest / "blob.bin").write_bytes(
        b"compute_backoff\x00\x01\x02" * 400))
    attempt("empty", lambda: (dest / "empty.py").write_text(""))
    attempt("whitespace_only", lambda: (dest / "blank.py").write_text("\n\n   \n\t\n"))
    # Over ChunkParams::default().max_file_bytes (4 MiB), so it is dropped at walk.
    attempt("oversized", lambda: (dest / "huge.py").write_text(
        "# compute_backoff padding\n" * 200_000))
    attempt("one_long_line", lambda: (dest / "oneline.py").write_text(
        "x = '" + "compute_backoff " * 60_000 + "'\n"))
    attempt("many_lines", lambda: (dest / "manylines.py").write_text(
        "# compute_backoff\n" * 200_000))
    attempt("invalid_utf8", lambda: (dest / "latin1.py").write_bytes(
        b"# caf\xe9 compute_backoff na\xefve\ndef f():\n    pass\n"))
    attempt("utf16_bom", lambda: (dest / "utf16.py").write_bytes(
        b"\xff\xfe" + "def compute_backoff(): pass\n".encode("utf-16-le")))
    attempt("emoji_name", lambda: (dest / "m\U0001f600ji.py").write_text(
        "def compute_backoff(): pass\n"))

    # The names that break `path:line:text` if anything does: a newline makes
    # one hit look like two lines of output, a colon makes the field split
    # ambiguous. They live in their own subdirectory so a scenario can scope to
    # them — `huge.py` emits 200k matching lines and would bury them, and a
    # format check that never saw the odd names passes for the wrong reason.
    names = dest / "names"
    names.mkdir(exist_ok=True)
    attempt("newline_name", lambda: (names / "we\nird.py").write_text(
        "def compute_backoff(): pass\n"))
    attempt("colon_name", lambda: (names / "od:d.py").write_text(
        "def compute_backoff(): pass\n"))
    attempt("quote_name", lambda: (names / 'qu"ote.py').write_text(
        "def compute_backoff(): pass\n"))
    attempt("space_name", lambda: (names / "with space.py").write_text(
        "def compute_backoff(): pass\n"))
    attempt("dash_name", lambda: (names / "-dash.py").write_text(
        "def compute_backoff(): pass\n"))
    attempt("plain_control", lambda: (names / "ordinary.py").write_text(
        "def compute_backoff(): pass\n"))

    def _unreadable():
        p = dest / "locked.py"
        p.write_text("def compute_backoff(): pass\n")
        os.chmod(p, 0o000)
    attempt("unreadable", _unreadable)

    def _fifo():
        os.mkfifo(dest / "pipe.py")
    attempt("fifo", _fifo)

    def _symlink_loop():
        a, b = dest / "loop_a", dest / "loop_b"
        a.symlink_to(b)
        b.symlink_to(a)
    attempt("symlink_loop", _symlink_loop)

    def _symlink_escape():
        (dest / "escape").symlink_to("/etc")
    attempt("symlink_escape", _symlink_escape)

    def _symlink_dangling():
        (dest / "dangling.py").symlink_to(dest / "does_not_exist.py")
    attempt("symlink_dangling", _symlink_dangling)

    def _deep():
        p = dest
        for i in range(40):
            p = p / f"d{i}"
        p.mkdir(parents=True, exist_ok=True)
        (p / "deep.py").write_text("def compute_backoff(): pass\n")
    attempt("deep_nesting", _deep)

    def _gitignored():
        (dest / ".gitignore").write_text("hidden/\n")
        (dest / "hidden").mkdir(exist_ok=True)
        (dest / "hidden" / "secret.py").write_text("def compute_backoff(): pass\n")
    attempt("gitignored_subtree", _gitignored)

    def _fake_index():
        # A directory named like the engine's own artifact, containing junk.
        d = dest / "nested" / ".gorp"
        d.mkdir(parents=True, exist_ok=True)
        (d / "meta.json").write_text("not json at all {{{")
    attempt("fake_semgrep_dir", _fake_index)

    return {"made": sorted(made), "skipped": skipped}


def cleanup_adversarial(dest):
    """`chmod 000` files defeat rmtree; restore permissions first."""
    dest = Path(dest)
    for p in dest.rglob("*"):
        try:
            if p.is_file() and not p.is_symlink():
                os.chmod(p, 0o644)
        except OSError:
            pass
    shutil.rmtree(dest, ignore_errors=True)


# -- mutation ---------------------------------------------------------------

def drift_files(root, fraction, seed=7, marker="DRIFTED"):
    """Rewrite `fraction` of the tree's files, changing size as well as content.

    Returns the paths touched. Size changes because `corpus::diff` compares
    `(size, mtime)` and mtime is whole seconds — a same-second same-length edit
    is invisible, which is S3d's finding and every other scenario's hazard.
    """
    root = Path(root)
    files = sorted(p for p in root.rglob("*")
                   if p.is_file() and not p.is_symlink()
                   and ".gorp" not in p.parts)
    rng = random.Random(seed)
    rng.shuffle(files)
    n = int(round(len(files) * fraction))
    touched = []
    for p in files[:n]:
        try:
            old = p.read_text(errors="replace")
        except OSError:
            continue
        p.write_text(f"# {marker} {rng.random()}\n" + old + f"\n# tail {marker}\n")
        touched.append(str(p.relative_to(root)))
    return touched


def same_second_same_size_edit(path):
    """The invisible edit: one byte swapped in place, `(size, mtime)` preserved.

    The mtime is restored with `os.utime` after the write. That is not a cheat,
    it is the only reliable way to construct the condition: mtime has
    whole-second resolution, so "the edit landed in the same second as the
    index" is a real and common situation (an agent edits and immediately
    re-searches) that a test cannot reach by racing the clock. Sleeping and
    hoping produced a *different* second on every attempt, which made the
    scenario report "drift was detected" and quietly test nothing.

    Restoring the timestamp reproduces exactly what `corpus::diff` would see.
    """
    path = Path(path)
    st = path.stat()
    data = path.read_bytes()
    for a, b in ((b"a", b"z"), (b"e", b"q"), (b"o", b"x"), (b"1", b"9")):
        if a in data:
            new = data.replace(a, b, 1)
            break
    else:
        return {"changed": False}
    assert len(new) == len(data)
    path.write_bytes(new)
    os.utime(path, (st.st_atime, st.st_mtime))
    after = path.stat()
    return {"changed": True,
            "size_before": st.st_size, "size_after": after.st_size,
            "mtime_before": int(st.st_mtime), "mtime_after": int(after.st_mtime),
            "size_and_mtime_identical": (st.st_size == after.st_size
                                         and int(st.st_mtime) == int(after.st_mtime))}


# -- index corruption -------------------------------------------------------

def index_dirs(cache_dir):
    """Every built entry under a cache base, across generations."""
    return sorted(p.parent for p in Path(cache_dir).rglob("meta.json"))


FAULTS = {}


def _fault(name):
    def deco(fn):
        FAULTS[name] = fn
        return fn
    return deco


@_fault("bm25_truncated_to_header")
def _bm25_header(d):
    """Truncate `bm25.flat` to exactly its header.

    8 magic + 4 n_docs + 4 n_terms + 8 total_len + 5*8 offsets = 64 bytes
    exactly, which is precisely the length `FlatBm25::open` accepts. The five
    section offsets it then reads are real values pointing past the end of a
    64-byte file, and every accessor uses them as unchecked slice indices.
    """
    p = d / "bm25.flat"
    data = p.read_bytes()
    p.write_bytes(data[:64])
    return {"file": "bm25.flat", "bytes_before": len(data), "bytes_after": 64}


@_fault("bm25_garbage_offsets")
def _bm25_offsets(d):
    """Keep the header's length and magic, poison only the offset table."""
    p = d / "bm25.flat"
    data = bytearray(p.read_bytes())
    if len(data) < 64:
        return {"skipped": "file shorter than a header"}
    for i in range(5):
        data[24 + i * 8: 32 + i * 8] = (0xFFFFFFFFFFFFFFFF).to_bytes(8, "little")
    p.write_bytes(bytes(data))
    return {"file": "bm25.flat", "offsets": "0xFFFF_FFFF_FFFF_FFFF x5"}


@_fault("bm25_half")
def _bm25_half(d):
    p = d / "bm25.flat"
    data = p.read_bytes()
    p.write_bytes(data[:len(data) // 2])
    return {"file": "bm25.flat", "bytes_after": len(data) // 2}


@_fault("emb_deleted")
def _emb_deleted(d):
    (d / "emb.bin").unlink()
    return {"file": "emb.bin", "op": "unlink"}


@_fault("emb_truncated_one_byte")
def _emb_one(d):
    p = d / "emb.bin"
    data = p.read_bytes()
    p.write_bytes(data[:-1])
    return {"file": "emb.bin", "bytes_after": len(data) - 1}


@_fault("meta_zero_length")
def _meta_zero(d):
    (d / "meta.json").write_bytes(b"")
    return {"file": "meta.json", "bytes_after": 0}


@_fault("meta_truncated_json")
def _meta_trunc(d):
    p = d / "meta.json"
    data = p.read_bytes()
    p.write_bytes(data[:len(data) // 2])
    return {"file": "meta.json", "bytes_after": len(data) // 2}


@_fault("chunks_truncated")
def _chunks(d):
    p = d / "chunks.bin"
    data = p.read_bytes()
    p.write_bytes(data[:max(1, len(data) // 3)])
    return {"file": "chunks.bin", "bytes_after": max(1, len(data) // 3)}


@_fault("hnsw_garbage")
def _hnsw(d):
    (d / "hnsw.bin").write_bytes(b"\xde\xad\xbe\xef" * 256)
    return {"file": "hnsw.bin", "op": "garbage"}


@_fault("params_removed")
def _params(d):
    p = d / "params.txt"
    if p.exists():
        p.unlink()
        return {"file": "params.txt", "op": "unlink"}
    return {"skipped": "no params.txt"}


def digest(path):
    """Content digest of a tree, for asserting an artifact did not change."""
    h = hashlib.sha256()
    for p in sorted(Path(path).rglob("*")):
        if p.is_file() and not p.is_symlink():
            h.update(str(p.relative_to(path)).encode())
            h.update(str(p.stat().st_size).encode())
            try:
                h.update(hashlib.sha256(p.read_bytes()).digest())
            except OSError:
                pass
    return h.hexdigest()[:16]
