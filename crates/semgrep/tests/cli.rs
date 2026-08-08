//! CLI contract tests: exit codes, stream discipline, and exact-mode grep
//! semantics. These assert the promises the README makes to a caller — the
//! things an agent or a shell script depends on and that no library test can
//! see, because they are properties of the *process*.
//!
//! Every test gets its own cache directory, so nothing here touches the
//! developer's real `~/.cache/semgrep` and no two tests share cache state.

use std::path::{Path, PathBuf};
use std::process::Command;

fn bin() -> PathBuf {
    // The integration test binary lives in target/<profile>/deps/.
    let mut p = std::env::current_exe().expect("test exe path");
    p.pop();
    if p.ends_with("deps") {
        p.pop();
    }
    p.join("semgrep")
}

/// The frozen fixture tree (tests/corpus at the repo root).
fn corpus() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/corpus")
}

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Run {
    fn lines(&self) -> Vec<&str> {
        self.stdout.lines().collect()
    }
}

/// A `semgrep` invocation with its own cache.
///
/// Per-test rather than per-process on purpose: these tests run real processes
/// in parallel, and a cold ranked search writes through to the cache. One
/// shared directory meant several processes building an entry for the same
/// scope simultaneously — a race the engine only partly defends against, which
/// showed up here as an occasional zero-hit run. Isolating the tests keeps that
/// out of the signal; hardening concurrent builds is separate work.
struct Sg {
    cache: PathBuf,
}

impl Sg {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = dir.path().to_path_buf();
        // Leak: the directory must outlive the child processes.
        std::mem::forget(dir);
        Self { cache }
    }

    /// Run against the fixture corpus.
    fn run(&self, args: &[&str]) -> Run {
        self.run_in(args, &corpus())
    }

    /// Run against an arbitrary path (appended last, as the CLI expects).
    fn run_in(&self, args: &[&str], path: &Path) -> Run {
        self.run_in_env(args, path, &[])
    }

    /// As `run_in`, with extra environment. `SEMGREP_TRACE_FILE` is the reason
    /// this exists: the trace surface is deliberately reachable without an
    /// argv change, so it has to be testable without one.
    fn run_in_env(&self, args: &[&str], path: &Path, env: &[(&str, &str)]) -> Run {
        let mut cmd = Command::new(bin());
        cmd.args(args)
            .arg(path)
            .env("SEMGREP_CACHE_DIR", &self.cache)
            .env("SEMGREP_CACHE_TTL_SECS", "0");
        for (k, v) in env {
            cmd.env(k, v);
        }
        let out = cmd.output().expect("run semgrep");
        Run {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    /// Run with no path argument at all.
    fn run_bare(&self, args: &[&str]) -> Run {
        let out = Command::new(bin())
            .args(args)
            .env("SEMGREP_CACHE_DIR", &self.cache)
            .env("SEMGREP_CACHE_TTL_SECS", "0")
            .output()
            .expect("run semgrep");
        Run {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }
}

// ---------------------------------------------------------------------------
// exit codes
// ---------------------------------------------------------------------------

#[test]
fn hits_exit_zero_and_misses_exit_one() {
    let sg = Sg::new();
    let found = sg.run(&["-e", "compute_backoff_delay"]);
    assert_eq!(found.code, 0, "a match must exit 0\nstderr: {}", found.stderr);

    let missed = sg.run(&["-e", "zzz_definitely_not_present"]);
    assert_eq!(missed.code, 1, "no match must exit 1 (grep's contract)");
    assert!(missed.stdout.is_empty(), "a miss must print nothing to stdout");
}

#[test]
fn usage_and_bad_input_exit_two() {
    let sg = Sg::new();
    assert_eq!(sg.run_bare(&[]).code, 2, "missing query is a usage error");

    // A pattern that is not a valid regex.
    let bad = sg.run(&["-e", "fn ("]);
    assert_eq!(bad.code, 2, "an invalid pattern is an error, not a miss");
    assert!(
        bad.stderr.contains("semgrep:"),
        "errors are prefixed so they are attributable in a shell pipeline"
    );

    assert_eq!(sg.run(&["--mode", "nonsense", "retry"]).code, 2);
}

// ---------------------------------------------------------------------------
// stream discipline: stdout is data, stderr is commentary
// ---------------------------------------------------------------------------

#[test]
fn stdout_is_parseable_and_advice_goes_to_stderr() {
    let sg = Sg::new();
    let r = sg.run(&["-e", "fn \\w+_token"]);
    assert_eq!(r.code, 0);
    for line in r.lines() {
        let mut parts = line.splitn(3, ':');
        let path = parts.next().unwrap_or_default();
        let line_no = parts.next().unwrap_or_default();
        assert!(!path.is_empty(), "expected path:line:text, got {line:?}");
        assert!(
            line_no.parse::<u32>().is_ok(),
            "second field must be a line number, got {line_no:?} in {line:?}"
        );
    }
    // The footer teaches the next move, and must not pollute stdout.
    assert!(r.stderr.contains("semgrep:"), "expected a footer on stderr");
    assert!(!r.stdout.contains("semgrep:"), "stdout must carry only results");
}

/// The §26 default: each result is an 18-line passage, results are separated by
/// a blank line, and every non-blank line is still `path:line:text`.
///
/// Pinned because three tests in this file counted stdout lines to count
/// results, and all three broke when the default changed — which is exactly
/// what a downstream consumer doing the same thing will experience. The shape
/// is now asserted somewhere rather than only implied by whatever else fails.
#[test]
fn the_default_result_is_an_eighteen_line_passage() {
    let sg = Sg::new();
    let r = sg.run(&["how is the retry delay computed", "-k", "2"]);
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);

    let blocks: Vec<Vec<&str>> = r
        .stdout
        .split("\n\n")
        .map(|b| b.lines().filter(|l| !l.trim().is_empty()).collect::<Vec<_>>())
        .filter(|b: &Vec<&str>| !b.is_empty())
        .collect();
    assert_eq!(blocks.len(), 2, "one block per result, blank-line separated");
    for b in &blocks {
        assert!(b.len() <= 18, "a passage is at most 18 lines, got {}", b.len());
        // Line numbers inside a passage must be consecutive and real, which is
        // what `lines_from` exists to get right.
        let nums: Vec<u32> = b
            .iter()
            .map(|l| l.split(':').nth(1).unwrap_or("x").parse().unwrap_or(0))
            .collect();
        assert!(nums.iter().all(|&n| n > 0), "every line carries a real number: {b:?}");
        assert!(
            nums.windows(2).all(|w| w[1] == w[0] + 1),
            "passage lines must be consecutive: {nums:?}"
        );
    }
    assert!(!r.stdout.contains("semgrep:"), "stdout stays data-only");
}

#[test]
fn ranked_search_also_keeps_stdout_clean() {
    let sg = Sg::new();
    // `--passage-lines 1` because this asserts the *result* count and, since
    // §26, one result is 18 lines rather than one. Counting stdout lines would
    // be counting passages.
    let r = sg.run(&["how is the retry delay computed", "-k", "5", "--passage-lines", "1"]);
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert!(r.lines().len() <= 5, "-k caps the result count");
    assert!(!r.stdout.contains("semgrep:"));
    assert!(r.stderr.contains("semgrep:"));
}

// ---------------------------------------------------------------------------
// exact mode == grep semantics
// ---------------------------------------------------------------------------

#[test]
fn exact_mode_enumerates_every_match() {
    // `-e` promises enumeration, not the k best: the count must exceed the
    // ranked default of 10 for a pattern this common, and ignore -k entirely.
    let sg = Sg::new();
    let r = sg.run(&["-e", "pub fn", "-k", "3"]);
    assert_eq!(r.code, 0);
    assert!(r.lines().len() > 3, "-k must not truncate exact mode; got {}", r.lines().len());
}

#[test]
fn ignore_case_and_fixed_string_flags_apply() {
    let sg = Sg::new();
    assert_eq!(sg.run(&["-e", "COMPUTE_BACKOFF_DELAY"]).code, 1, "case-sensitive by default");
    assert_eq!(
        sg.run(&["-e", "COMPUTE_BACKOFF_DELAY", "-i"]).code,
        0,
        "-i must match regardless of case"
    );

    // `.` is a regex wildcard until -F makes it a literal.
    assert_eq!(sg.run(&["-e", "compute.backoff.delay"]).code, 0);
    assert_eq!(
        sg.run(&["-e", "compute.backoff.delay", "-F"]).code,
        1,
        "-F must treat the pattern literally"
    );
}

#[test]
fn context_flag_frames_the_hit() {
    let sg = Sg::new();
    let r = sg.run(&["-e", "fn compute_backoff_delay", "-C", "2"]);
    assert_eq!(r.code, 0);
    // Context lines use `path-line-text`; the hit itself uses `path:line:text`.
    assert!(r.stdout.contains("--"), "context blocks are separated by --");
    let context_lines = r.lines().iter().filter(|l| l.contains(".rs-")).count();
    assert!(context_lines >= 2, "expected context lines around the hit");
}

/// The block is dedented by what its lines *share*, not by the hit's own indent.
///
/// Written because the first attempt dedented by the hit's indentation, and the
/// matched line is usually the deepest one in its block — so every context line
/// landed at the margin and `-C` returned a flat list of statements, having been
/// asked for the shape of the code around them.
#[test]
fn context_dedents_the_block_without_flattening_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("nest.py"),
        "class Handler:\n    def add_code(self, payload):\n        if payload:\n            \
         return self.registry.add_code(payload)\n        return None\n",
    )
    .expect("write");
    let sg = Sg::new();
    let r = sg.run_in(&["-e", "return self.registry", "-C", "2"], dir.path());
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    // `class Handler:` is outside the window, so the four framed lines share the
    // `def`'s four spaces and only those come off. What nests deeper stays deeper.
    assert!(
        r.stdout.contains("nest.py-2-def add_code(self, payload):"),
        "the shallowest line reaches the margin: {:?}",
        r.stdout
    );
    assert!(
        r.stdout.contains("nest.py-3-    if payload:"),
        "one level in stays one level in: {:?}",
        r.stdout
    );
    assert!(
        r.stdout.contains("nest.py:4:        return self.registry.add_code(payload)"),
        "the hit keeps its offset from the block too: {:?}",
        r.stdout
    );
}

// ---------------------------------------------------------------------------
// output formats
// ---------------------------------------------------------------------------

#[test]
fn json_emits_one_object_per_line_with_a_stable_field_set() {
    let sg = Sg::new();
    let r = sg.run(&["--json", "validate a session token", "-k", "3"]);
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert!(!r.stdout.is_empty());
    for line in r.lines() {
        let v: serde_json::Value = serde_json::from_str(line).expect("each line is JSON");
        for field in ["path", "start_line", "end_line", "line", "text", "score"] {
            assert!(v.get(field).is_some(), "missing {field} in {line}");
        }
        assert!(v["start_line"].as_u64().unwrap() <= v["line"].as_u64().unwrap());
        assert!(v["line"].as_u64().unwrap() <= v["end_line"].as_u64().unwrap());
    }
}

/// A ranked result costs k lines, and until `-M` nothing said how wide one could
/// be. A single-line 374 KB JSON fixture made a real k=5 search return 659 KB —
/// which does not merely cost tokens, it overruns the reader's tool-result limit
/// and deletes the hits ranked below it.
#[test]
fn one_long_line_cannot_crowd_out_the_hits_beneath_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("bundle.min.js"),
        format!("var add_code={};\n", "x".repeat(200_000)),
    )
    .expect("write");
    std::fs::write(
        dir.path().join("real.py"),
        "class Handler:\n    def add_code(self, payload):\n        return payload\n",
    )
    .expect("write");
    let sg = Sg::new();

    let r = sg.run_in(&["add_code", "-k", "5"], dir.path());
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert!(r.stdout.len() < 4_000, "capped, got {} bytes", r.stdout.len());
    assert!(r.stdout.contains("omitted end of long line"), "the cut is declared, not silent");
    assert!(r.stdout.contains("real.py"), "the hit ranked below the long line survives it");

    // The escape hatch, for a caller that wants the bytes and knows the cost.
    let full = sg.run_in(&["add_code", "-k", "5", "-M", "0"], dir.path());
    assert_eq!(full.code, 0, "stderr: {}", full.stderr);
    assert!(full.stdout.len() > 100_000, "-M 0 restores the whole line");
    assert!(!full.stdout.contains("omitted end of long line"));
}

/// Indentation is the one part of a line that is pure position — already carried
/// by the line number, and reconstructible from the file the hit names.
#[test]
fn result_lines_arrive_without_their_indentation() {
    let sg = Sg::new();
    let r = sg.run(&["-e", "def _reap"]);
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    let line = r.lines()[0].to_string();
    let text = line.splitn(3, ':').nth(2).expect("path:line:text");
    assert!(!text.starts_with(' ') && !text.starts_with('\t'), "indent stripped: {line:?}");
    assert!(text.starts_with("def _reap"), "the code itself is untouched: {line:?}");
}

/// `-k` typed with nothing after it used to exit 2 and cost the caller a whole
/// round-trip. Neither grep nor ripgrep has `-k` at all, so nothing was being
/// honored by refusing — it is `tar -k`/`df -k`/`du -k` muscle memory, where the
/// flag is complete on its own.
#[test]
fn a_bare_k_asks_for_more_rather_than_failing() {
    let sg = Sg::new();
    // `run_bare` so the path lands *before* the flag: a bare `-k` takes the next
    // token as its value if there is one, so it only reads as bare at the end of
    // the line. That is where it appears — of 1,554 real `-k` invocations, 1,552
    // are followed by a number and 2 by nothing at all, and none by a path.
    let dir = corpus();
    let path = dir.to_str().expect("utf-8 corpus path");
    let bare = sg.run_bare(&["session token", path, "--passage-lines", "1", "-k"]);
    assert_eq!(bare.code, 0, "stderr: {}", bare.stderr);
    assert_eq!(bare.lines().len(), 20, "bare -k means 20");

    // The flag's absence still means the engine's default: "no opinion" and
    // "more than the default" are different statements and keep different
    // answers.
    let absent = sg.run(&["session token", "--passage-lines", "1"]);
    assert_eq!(absent.lines().len(), 10, "no -k means the engine default");

    // And an explicit value still wins over both — the common real form.
    let explicit = sg.run(&["session token", "-k", "5", "--passage-lines", "1"]);
    assert_eq!(explicit.lines().len(), 5, "-k N is unchanged");
}

/// `| head` is the most common thing anyone does to this tool — 237 of the 300
/// pipes in the §19.7 campaign — and it used to crash it. Rust sets SIGPIPE to
/// SIG_IGN before main, so the write returns EPIPE and `println!` panics. Only
/// past the ~64 KB pipe buffer, which is why `-M 200` hid it in ranked mode and
/// `--all` still reached it, and why no test caught it: nothing here pipes.
#[test]
fn a_closed_pipe_is_not_a_panic() {
    let dir = tempfile::tempdir().expect("tempdir");
    let body: String = (0..60_000).map(|i| format!("def fn_{i}(x): return x\n")).collect();
    std::fs::write(dir.path().join("big.py"), body).expect("write");
    let sg = Sg::new();

    // `head -1` closes the read end after one line, while the child still has
    // tens of thousands to write.
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!(
            "{} -e 'def fn_' {} --all 2>&1 >/dev/null | head -1",
            bin().display(),
            dir.path().display()
        ))
        .env("SEMGREP_CACHE_DIR", &sg.cache)
        .output()
        .expect("run");
    let stderr = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(!stderr.contains("panicked"), "panicked on a closed pipe: {stderr}");
    assert!(!stderr.contains("Broken pipe"), "leaked an EPIPE message: {stderr}");
}

/// The one thing agents piped into `awk`/`grep` that `-k` could not serve.
#[test]
fn lines_narrows_to_a_range_in_every_spelling() {
    let dir = tempfile::tempdir().expect("tempdir");
    let body: String = (1..1000)
        .map(|i| if i % 37 == 0 { format!("def fn_{i}(x): pass\n") } else { "# filler\n".into() })
        .collect();
    std::fs::write(dir.path().join("f.py"), body).expect("write");
    let f = dir.path().join("f.py");
    let sg = Sg::new();
    // One named file, so the tool omits the path and the line number leads.
    let nums = |r: &Run| -> Vec<u32> {
        r.lines().iter().filter_map(|l| l.split(':').next()?.parse().ok()).collect()
    };

    let all = sg.run_in(&["-e", "def fn_", "--all"], &f);
    assert!(nums(&all).len() > 20, "fixture should have many matches");

    for spec in ["800-939", "1-100", "900-"] {
        let r = sg.run_in(&["-e", "def fn_", "--all", "--lines", spec], &f);
        assert_eq!(r.code, 0, "stderr: {}", r.stderr);
        let (lo, hi) = match spec {
            "800-939" => (800, 939),
            "1-100" => (1, 100),
            _ => (900, u32::MAX),
        };
        let got = nums(&r);
        assert!(!got.is_empty(), "--lines {spec} dropped everything");
        assert!(got.iter().all(|n| *n >= lo && *n <= hi), "--lines {spec} leaked {got:?}");
    }

    // `-B` in the spaced form. Without allow_hyphen_values clap reads `-100` as
    // a stray flag and advises `-- -1`, so `--lines=-100` worked and this did
    // not — the kind of split that costs a turn to discover.
    let r = sg.run_in(&["-e", "def fn_", "--all", "--lines", "-100"], &f);
    assert_eq!(r.code, 0, "spaced -B form rejected: {}", r.stderr);
    assert!(nums(&r).iter().all(|n| *n <= 100));

    // And it says which filter did the cutting.
    assert!(
        r.stderr.contains("narrowed to lines"),
        "the message should name --lines, not the paths: {}",
        r.stderr
    );

    for bad in ["abc", "900-100"] {
        let r = sg.run_in(&["-e", "def", "--lines", bad], &f);
        assert_eq!(r.code, 2, "--lines {bad:?} should be a usage error");
        assert!(r.stderr.contains("--lines"), "error should name the flag: {}", r.stderr);
    }
}

/// `find … | sg query -`, so a path list composes without `xargs`.
#[test]
fn a_dash_reads_paths_from_stdin() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("a.py"), "def target_fn(): pass\n").expect("write");
    std::fs::write(dir.path().join("b.py"), "def other(): pass\n").expect("write");
    let sg = Sg::new();
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!(
            "printf '%s\\n' {}/a.py | {} -e 'def ' -",
            dir.path().display(),
            bin().display()
        ))
        .env("SEMGREP_CACHE_DIR", &sg.cache)
        .output()
        .expect("run");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("target_fn"), "stdin path not searched: {stdout}");
    assert!(!stdout.contains("other"), "searched a path stdin did not name: {stdout}");
}

/// grep's rule: one named file means the path is on every line and tells the
/// reader nothing they did not just type. The snapshot cannot catch a
/// regression here — all 114 of its cases search the corpus directory — and a
/// single file is 53% of real agent invocations (RESEARCH.md §19.9).
#[test]
fn one_named_file_drops_the_path_and_leads_with_the_line() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("f.py"), "def target(): pass\n").expect("write");
    let f = dir.path().join("f.py");
    let sg = Sg::new();

    let one = sg.run_in(&["-e", "def target"], &f);
    assert_eq!(one.code, 0, "stderr: {}", one.stderr);
    assert_eq!(one.lines()[0], "1:\tdef target(): pass",
               "one named file: line, tab, text — no path");

    // -H asks for it back, which is what a caller splitting on `:` wants.
    let forced = sg.run_in(&["-e", "def target", "-H"], &f);
    assert!(forced.lines()[0].starts_with("f.py:1:"), "-H restores the path: {:?}",
            forced.lines()[0]);

    // A directory scope is unchanged: the path is doing real work there.
    let many = sg.run_in(&["-e", "def target"], dir.path());
    assert!(many.lines()[0].starts_with("f.py:1:"), "dir scope keeps the path: {:?}",
            many.lines()[0]);

    // --json and -l are path-carrying formats by definition.
    let js = sg.run_in(&["-e", "def target", "--json"], &f);
    assert!(js.lines()[0].contains("\"path\":\"f.py\""), "json keeps path: {:?}", js.lines()[0]);
    let l = sg.run_in(&["-e", "def target", "-l"], &f);
    assert_eq!(l.lines()[0], "f.py", "-l is a path listing");
}

#[test]
fn stats_report_goes_to_stderr_with_stage_provenance() {
    let sg = Sg::new();
    let r = sg.run(&["--stats", "drain urgent jobs first", "-k", "3"]);
    assert_eq!(r.code, 0);
    assert!(r.stderr.contains("mode="), "expected a stats line");
    assert!(r.stderr.contains("provenance:"), "expected per-stage timings");
    assert!(r.stderr.contains("unattributed="), "expected the residual");
    assert!(!r.stdout.contains("mode="), "stats must not reach stdout");
}

#[test]
fn stats_json_stays_off_stdout_and_composes_with_json() {
    // `--json` owns stdout. If the envelope leaked there, every consumer that
    // parses hits would break on a line that is not a hit.
    let sg = Sg::new();
    let r = sg.run(&["--json", "--stats-json", "validate a session token", "-k", "3"]);
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);

    for line in r.lines() {
        let v: serde_json::Value = serde_json::from_str(line).expect("stdout stays hit JSONL");
        assert!(v.get("path").is_some(), "stdout carried a non-hit object: {line}");
        assert!(v.get("schema").is_none(), "the trace envelope reached stdout");
    }

    let envelope: serde_json::Value = r
        .stderr
        .lines()
        .find_map(|l| serde_json::from_str(l).ok())
        .expect("the envelope is on stderr");
    assert_eq!(envelope["schema"], "semgrep.trace/1");
    assert_eq!(envelope["kind"], "search");
    assert!(envelope["timing"]["stages"].as_array().is_some_and(|a| !a.is_empty()));
}

#[test]
fn the_trace_file_records_every_engine_invocation_including_the_hidden_one() {
    // An exact-mode miss over an indexed scope runs a *second*, complete
    // `search()` to build its suggestion. `--stats` never showed it — it
    // describes the primary result only — so the cost of a failed `-e` looked
    // like a keyword scan when it was a keyword scan plus a ranked query.
    let sg = Sg::new();
    let trace = sg.cache.join("trace.jsonl");
    let trace_s = trace.to_string_lossy().into_owned();

    // Warm an index for this scope, or the suggestion path declines to run.
    sg.run(&["retry backoff jitter", "-k", "3"]);
    let r = sg.run_in_env(
        &["-e", "zzz_no_such_symbol_anywhere"],
        &corpus(),
        &[("SEMGREP_TRACE_FILE", &trace_s)],
    );
    assert_eq!(r.code, 1, "an exact miss still exits 1");

    let text = std::fs::read_to_string(&trace).expect("trace file written");
    let records: Vec<serde_json::Value> =
        text.lines().map(|l| serde_json::from_str(l).expect("one object per line")).collect();
    assert_eq!(records.len(), 2, "one primary and one suggestion: {text}");

    let phases: Vec<&str> = records.iter().map(|r| r["phase"].as_str().unwrap()).collect();
    assert_eq!(phases, vec!["primary", "suggest"]);
    assert_eq!(
        records[0]["query_id"], records[1]["query_id"],
        "both invocations belong to one command"
    );
    assert_eq!(records[0]["input"]["mode"], "keyword");
    assert_eq!(records[1]["input"]["mode"], "semantic");
    assert_eq!(records[1]["input"]["mode_reason"], "exact-miss-suggestion");

    // Two index resolutions for one failed `-e`, neither of them the user's:
    // `suggest_ranked_alternatives` resolves to decide whether it is cheap to
    // suggest, then the nested `search()` resolves again to actually do it.
    // Unchanged by the removal of the CLI's cold-start pre-check, which never
    // ran in keyword mode — this pair is the suggestion path's own, and closing
    // it means teaching the suggestion to reuse what it already resolved.
    assert_eq!(
        records[0]["resolution"]["discover_calls"], 0,
        "the keyword scan itself resolves nothing"
    );
    assert_eq!(
        records[1]["resolution"]["discover_calls"], 2,
        "the suggestion path resolves the index twice"
    );
}

#[test]
fn a_warm_ranked_query_resolves_the_index_once() {
    // It used to resolve twice: the CLI called `cache::discover` on *every*
    // ranked search, not only the first, purely to decide whether to print one
    // line, and the engine then resolved the same scope again. Each resolution
    // canonicalizes the path and scans the generation directory, so the second
    // one was pure waste on the most common path there is. The notice now comes
    // from the engine (`SearchOptions::on_first_search`), which had already
    // resolved. Pinned at one so a future caller cannot quietly add a third.
    let sg = Sg::new();
    let trace = sg.cache.join("warm-trace.jsonl");
    let trace_s = trace.to_string_lossy().into_owned();

    sg.run(&["retry backoff jitter", "-k", "3"]); // build the entry
    let r = sg.run_in_env(
        &["retry backoff jitter", "-k", "3"],
        &corpus(),
        &[("SEMGREP_TRACE_FILE", &trace_s)],
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);

    let text = std::fs::read_to_string(&trace).unwrap();
    let v: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
    assert_eq!(v["resolution"]["path_taken"], "warm");
    assert_eq!(v["resolution"]["discover_calls_engine"], 1);
    assert_eq!(v["resolution"]["discover_calls"], 1, "the CLI must not resolve on its own");
}

#[test]
fn the_trace_file_is_opt_in_and_its_failure_is_silent() {
    // Telemetry must never turn a working query into a failed one.
    let sg = Sg::new();
    let r = sg.run_in_env(
        &["retry backoff jitter", "-k", "3"],
        &corpus(),
        &[("SEMGREP_TRACE_FILE", "/nonexistent-dir/nope/trace.jsonl")],
    );
    assert_eq!(r.code, 0, "an unwritable trace file must not fail the search");
    assert!(!r.stdout.is_empty(), "and must not suppress results");
}

#[test]
fn indexing_reports_its_own_stage_schedule() {
    // `BuildStats` carried counts and no timings, so where a 45-second kernel
    // build spent its time was unrecoverable.
    let sg = Sg::new();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.rs"), "fn retry_with_backoff() {}\n".repeat(40)).unwrap();
    let trace = sg.cache.join("index-trace.jsonl");
    let trace_s = trace.to_string_lossy().into_owned();

    let r = sg.run_in_env(&["index"], dir.path(), &[("SEMGREP_TRACE_FILE", &trace_s)]);
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);

    let text = std::fs::read_to_string(&trace).expect("trace file written");
    let v: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
    assert_eq!(v["kind"], "index");
    let names: Vec<&str> = v["timing"]["stages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["stage"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"build:embed"), "got {names:?}");
    assert!(names.contains(&"build:write"), "got {names:?}");
}

// ---------------------------------------------------------------------------
// subcommands
// ---------------------------------------------------------------------------

#[test]
fn cache_status_reports_the_generation_and_budget() {
    let sg = Sg::new();
    sg.run(&["retry backoff jitter", "-k", "3"]); // warm an entry
    let r = sg.run_bare(&["cache"]);
    assert_eq!(r.code, 0);
    assert!(r.stdout.contains("generation "), "status names the compat generation");
    assert!(r.stdout.contains("budget"), "status reports the budget");
}

#[test]
fn index_status_distinguishes_present_from_absent() {
    let sg = Sg::new();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.py"), "def only_symbol():\n    return 1\n").unwrap();

    assert_eq!(sg.run_in(&["index", "--status"], dir.path()).code, 1, "no index yet → exit 1");
    assert_eq!(sg.run_in(&["index"], dir.path()).code, 0);

    let present = sg.run_in(&["index", "--status"], dir.path());
    assert_eq!(present.code, 0, "fresh index → exit 0");
    assert!(present.stdout.contains("index:"));
}

// ---------------------------------------------------------------------------
// the stdout contract under hostile input (SIMULATION.md §1.5, §1.6)
// ---------------------------------------------------------------------------

#[test]
fn a_nonexistent_path_is_an_error_not_an_empty_result() {
    // Exit 1 means "nothing found", and that is what an agent reads: *the code
    // is not there*. The path was simply wrong. Exit 2 exists for this.
    let sg = Sg::new();
    let missing = corpus().join("no_such_subdir");

    for args in [vec!["exponential backoff retry policy"], vec!["-e", "compute_backoff"]] {
        let r = sg.run_in(&args, &missing);
        assert_eq!(r.code, 2, "{args:?} on a missing path must exit 2, got {}", r.code);
        assert!(r.stdout.is_empty(), "{args:?} wrote to stdout: {:?}", r.stdout);
        assert!(
            r.stderr.contains("no such file or directory"),
            "{args:?} must say why: {:?}",
            r.stderr
        );
        // The old failure mode: announcing a cache build for a scope that does
        // not exist, on the way to reporting no results.
        assert!(
            !r.stderr.contains("caching it"),
            "{args:?} must not announce caching a missing scope: {:?}",
            r.stderr
        );
    }
}

#[test]
fn one_hit_per_file_however_the_file_is_named() {
    // Six files on disk used to produce seven stdout lines: a newline in a
    // filename split one hit across two records, and `od:d.py` mis-parsed as
    // path "od", line "d.py". The scope is a directory holding *only* the
    // hostile names, because the first version of this check ran against a tree
    // whose 200k-line file overflowed the capture cap long before the odd names
    // appeared — it passed, vacuously (SIMULATION.md §5).
    let dir = tempfile::tempdir().unwrap();
    let body = "def compute_backoff(): pass\n";
    let names = ["ordinary.py", "with space.py", "-dash.py", "qu\"ote.py", "od:d.py", "we\nird.py"];
    let mut written = 0;
    for n in names {
        // A newline in a filename is legal on unix but not on every filesystem;
        // skip rather than fail if the OS refuses, and assert on what landed.
        if std::fs::write(dir.path().join(n), body).is_ok() {
            written += 1;
        }
    }
    assert!(written >= 5, "expected the hostile names to be creatable, wrote {written}");

    let sg = Sg::new();
    let r = sg.run_in(&["-e", "compute_backoff"], dir.path());
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(
        r.lines().len(),
        written,
        "{written} files on disk must give {written} stdout lines, got {:?}",
        r.lines()
    );

    // Every line must still parse as path:line:text — the promise that makes
    // semgrep drop-in for grep. A quoted path is unambiguous; a bare one must
    // not contain the separator.
    for line in r.lines() {
        let (path, rest) = if let Some(after) = line.strip_prefix('"') {
            // The closing quote is the first unescaped one — `qu"ote.py` quotes
            // to `"qu\"ote.py"`, and a parser that stops at the first `"` it
            // sees splits the path in half. Consumers have to do this too, which
            // is exactly why only ambiguous names get quoted.
            let mut escaped = false;
            let end = after
                .char_indices()
                .find(|&(_, c)| {
                    let close = c == '"' && !escaped;
                    escaped = c == '\\' && !escaped;
                    close
                })
                .map(|(i, _)| i)
                .expect("a quoted path closes its quote");
            (&after[..end], &after[end + 1..])
        } else {
            let i = line.find(':').expect("an unquoted line has a separator");
            (&line[..i], &line[i..])
        };
        assert!(!path.is_empty(), "empty path in {line:?}");
        let rest = rest.strip_prefix(':').expect("path is followed by :line:text");
        let (num, text) = rest.split_once(':').expect("line number is followed by :text");
        assert!(num.parse::<u32>().is_ok(), "{num:?} is not a line number in {line:?}");
        assert_eq!(text, body.trim_end(), "wrong text in {line:?}");
    }
}

#[test]
fn json_output_is_one_object_per_hit_under_the_same_names() {
    // The control: serde already escaped what println! did not, so --json was
    // the one mode the simulation found intact. It must stay that way, and it
    // must not pick up the quoting the plain path now applies.
    let dir = tempfile::tempdir().unwrap();
    for n in ["plain.py", "od:d.py", "we\nird.py"] {
        let _ = std::fs::write(dir.path().join(n), "def compute_backoff(): pass\n");
    }
    let on_disk = std::fs::read_dir(dir.path()).unwrap().count();

    let sg = Sg::new();
    let r = sg.run_in(&["-e", "compute_backoff", "--json"], dir.path());
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);

    let objs: Vec<serde_json::Value> =
        r.stdout.lines().map(|l| serde_json::from_str(l).expect("valid JSON per line")).collect();
    assert_eq!(objs.len(), on_disk, "one object per file: {:?}", r.stdout);
    for o in &objs {
        let p = o["path"].as_str().expect("path is a string");
        assert!(!p.starts_with('"'), "--json must not double-quote {p:?}");
    }
}

/// Grep muscle memory must parse (RESEARCH.md §17).
///
/// Measured on the rg arm of the §16.10 campaign: `-n` appeared in 1,829 of
/// 2,074 real agent invocations (88%), and semgrep exited 2 on every one. The
/// assertion is `code != 2` — zero hits is a legitimate answer, failing to
/// parse the command line is not.
#[test]
fn grep_compatible_flags_are_accepted() {
    let sg = Sg::new();
    for args in [
        &["-n", "-k", "3", "retry backoff"][..],
        &["-r", "-k", "3", "retry backoff"][..],
        &["-R", "-k", "3", "retry backoff"][..],
        &["-H", "-k", "3", "retry backoff"][..],
        // Combined shorts: agents type `-rn`, and clap only resolves these
        // once every constituent short exists.
        &["-rn", "-k", "3", "retry backoff"][..],
        &["-rln", "-k", "3", "retry backoff"][..],
        &["-A", "2", "-k", "2", "retry backoff"][..],
        &["-B", "2", "-k", "2", "retry backoff"][..],
        &["-g", "*.md", "-k", "3", "retry backoff"][..],
        &["--include", "*.md", "-k", "3", "retry backoff"][..],
    ] {
        let r = sg.run(args);
        assert_ne!(r.code, 2, "{args:?} is a usage error: {}", r.stderr);
    }
    // The control: parsing must not have simply gone permissive.
    let r = sg.run(&["--definitely-not-a-flag", "-k", "3", "retry backoff"]);
    assert_eq!(r.code, 2, "an unknown flag must still be a usage error");
}

/// `-l` prints unique paths in rank order and nothing else.
#[test]
fn files_with_matches_prints_paths_only() {
    let sg = Sg::new();
    let r = sg.run(&["-l", "-k", "5", "retry backoff"]);
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    let lines = r.lines();
    assert!(!lines.is_empty(), "expected paths");
    for l in &lines {
        assert!(!l.contains(':'), "-l must print bare paths, got {l:?}");
    }
    let mut uniq = lines.clone();
    uniq.sort_unstable();
    uniq.dedup();
    assert_eq!(uniq.len(), lines.len(), "-l must not repeat a path: {lines:?}");
}

/// Several paths at once — grep's contract, and 31% of real agent invocations.
/// Results must come back, and only from the paths asked for.
#[test]
fn multiple_paths_search_all_of_them_and_nothing_else() {
    let sg = Sg::new();
    let corpus = corpus();
    let mut cmd = std::process::Command::new(bin());
    cmd.args(["-k", "10", "retry backoff"])
        .arg(corpus.join("src"))
        .arg(corpus.join("docs"))
        .env("SEMGREP_CACHE_DIR", &sg.cache)
        .env("SEMGREP_CACHE_TTL_SECS", "0");
    let out = cmd.output().expect("run semgrep");
    assert_eq!(out.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!stdout.is_empty(), "multi-path search returned nothing");
    for line in stdout.lines().filter(|l| !l.trim().is_empty()) {
        // Blank lines separate passages since §26 and carry no path.
        let path = line.split(':').next().unwrap_or("");
        assert!(
            path.starts_with("src/") || path.starts_with("docs/"),
            "hit outside the requested paths: {line:?}"
        );
    }
}

/// A scope with files in it that none of which can be read must say so.
///
/// This is the §16.11 signature: the file-scope bug walked one file, failed to
/// read it, and reported an ordinary miss. Agents retried, then read `--help`
/// — 27 of 27 help probes in the campaign followed three or more empty
/// searches. "No results" and "nothing here was readable" are different facts
/// and the caller can only act on the second.
#[test]
fn an_unreadable_scope_says_so_rather_than_reporting_a_miss() {
    let dir = tempfile::tempdir().unwrap();
    for n in ["a.bin", "b.bin"] {
        std::fs::write(dir.path().join(n), [0u8, 1, 2, 0, 3, 4]).unwrap();
    }
    let sg = Sg::new();
    let r = sg.run_in(&["-k", "3", "retry backoff"], dir.path());
    assert_eq!(r.code, 1, "no hits is exit 1");
    assert!(
        r.stderr.contains("could read none of them"),
        "expected the unreadable-scope diagnostic, got: {}",
        r.stderr
    );

    // And an genuinely empty directory reports the *other* fact.
    let empty = tempfile::tempdir().unwrap();
    let r = sg.run_in(&["-k", "3", "retry backoff"], empty.path());
    assert!(
        r.stderr.contains("nothing to search"),
        "expected the empty-scope diagnostic, got: {}",
        r.stderr
    );
}
