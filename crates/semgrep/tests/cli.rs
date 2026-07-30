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
        let out = Command::new(bin())
            .args(args)
            .arg(path)
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

#[test]
fn ranked_search_also_keeps_stdout_clean() {
    let sg = Sg::new();
    let r = sg.run(&["how is the retry delay computed", "-k", "5"]);
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

#[test]
fn stats_report_goes_to_stderr_with_stage_provenance() {
    let sg = Sg::new();
    let r = sg.run(&["--stats", "drain urgent jobs first", "-k", "3"]);
    assert_eq!(r.code, 0);
    assert!(r.stderr.contains("mode="), "expected a stats line");
    assert!(r.stderr.contains("provenance:"), "expected per-stage timings");
    assert!(!r.stdout.contains("mode="), "stats must not reach stdout");
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
