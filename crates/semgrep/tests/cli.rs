//! CLI contract tests: exit codes, stream discipline, and exact-mode grep
//! semantics. These assert the promises the README makes to a caller — the
//! things an agent or a shell script depends on and that no library test can
//! see, because they are properties of the *process*.
//!
//! Every test runs against `tests/corpus` (frozen) with an isolated cache, so
//! nothing here touches the developer's real `~/.cache/semgrep`.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn bin() -> PathBuf {
    // The integration test binary lives in target/<profile>/deps/.
    let mut p = std::env::current_exe().expect("test exe path");
    p.pop();
    if p.ends_with("deps") {
        p.pop();
    }
    p.join("semgrep")
}

fn corpus() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/corpus")
}

/// One cache directory per test process, isolated from the user's and from
/// other tests' state. Leaked deliberately: it must outlive every child.
fn cache_dir() -> &'static Path {
    static DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        let d = tempfile::tempdir().expect("tempdir");
        let p = d.path().to_path_buf();
        std::mem::forget(d);
        p
    })
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

fn semgrep(args: &[&str]) -> Run {
    let out: Output = Command::new(bin())
        .args(args)
        .arg(corpus())
        .env("SEMGREP_CACHE_DIR", cache_dir())
        .env("SEMGREP_CACHE_TTL_SECS", "0")
        .output()
        .expect("run semgrep");
    Run {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

// ---------------------------------------------------------------------------
// exit codes
// ---------------------------------------------------------------------------

#[test]
fn hits_exit_zero_and_misses_exit_one() {
    let found = semgrep(&["-e", "compute_backoff_delay"]);
    assert_eq!(found.code, 0, "a match must exit 0\nstderr: {}", found.stderr);

    let missed = semgrep(&["-e", "zzz_definitely_not_present"]);
    assert_eq!(missed.code, 1, "no match must exit 1 (grep's contract)");
    assert!(missed.stdout.is_empty(), "a miss must print nothing to stdout");
}

#[test]
fn usage_and_bad_input_exit_two() {
    // No query at all.
    let bare = Command::new(bin())
        .env("SEMGREP_CACHE_DIR", cache_dir())
        .output()
        .expect("run semgrep");
    assert_eq!(bare.status.code(), Some(2), "missing query is a usage error");

    // A pattern that is not a valid regex.
    let bad = semgrep(&["-e", "fn ("]);
    assert_eq!(bad.code, 2, "an invalid pattern is an error, not a miss");
    assert!(
        String::from_utf8_lossy(bad.stderr.as_bytes()).contains("semgrep:"),
        "errors are prefixed so they are attributable in a shell pipeline"
    );

    let bad_mode = semgrep(&["--mode", "nonsense", "retry"]);
    assert_eq!(bad_mode.code, 2);
}

// ---------------------------------------------------------------------------
// stream discipline: stdout is data, stderr is commentary
// ---------------------------------------------------------------------------

#[test]
fn stdout_is_parseable_and_advice_goes_to_stderr() {
    let r = semgrep(&["-e", "fn \\w+_token"]);
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
    let r = semgrep(&["how is the retry delay computed", "-k", "5"]);
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
    let r = semgrep(&["-e", "pub fn", "-k", "3"]);
    assert_eq!(r.code, 0);
    assert!(
        r.lines().len() > 3,
        "-k must not truncate exact mode; got {} lines",
        r.lines().len()
    );
}

#[test]
fn ignore_case_and_fixed_string_flags_apply() {
    let sensitive = semgrep(&["-e", "COMPUTE_BACKOFF_DELAY"]);
    assert_eq!(sensitive.code, 1, "case-sensitive by default");

    let insensitive = semgrep(&["-e", "COMPUTE_BACKOFF_DELAY", "-i"]);
    assert_eq!(insensitive.code, 0, "-i must match regardless of case");

    // `.` is a regex wildcard until -F makes it a literal.
    let as_regex = semgrep(&["-e", "compute.backoff.delay"]);
    assert_eq!(as_regex.code, 0);
    let as_literal = semgrep(&["-e", "compute.backoff.delay", "-F"]);
    assert_eq!(as_literal.code, 1, "-F must treat the pattern literally");
}

#[test]
fn context_flag_frames_the_hit() {
    let r = semgrep(&["-e", "fn compute_backoff_delay", "-C", "2"]);
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
    let r = semgrep(&["--json", "validate a session token", "-k", "3"]);
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
    let r = semgrep(&["--stats", "drain urgent jobs first", "-k", "3"]);
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
    // Warm at least one entry first.
    semgrep(&["retry backoff jitter", "-k", "3"]);
    let out = Command::new(bin())
        .args(["cache"])
        .env("SEMGREP_CACHE_DIR", cache_dir())
        .output()
        .expect("run semgrep cache");
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("generation "), "status names the compat generation");
    assert!(text.contains("budget"), "status reports the budget");
}

#[test]
fn index_status_distinguishes_present_from_absent() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.py"), "def only_symbol():\n    return 1\n").unwrap();

    let missing = Command::new(bin())
        .args(["index", "--status"])
        .arg(dir.path())
        .env("SEMGREP_CACHE_DIR", cache_dir())
        .output()
        .expect("run");
    assert_eq!(missing.status.code(), Some(1), "no index yet → exit 1");

    let built = Command::new(bin())
        .arg("index")
        .arg(dir.path())
        .env("SEMGREP_CACHE_DIR", cache_dir())
        .output()
        .expect("run");
    assert_eq!(built.status.code(), Some(0));

    let present = Command::new(bin())
        .args(["index", "--status"])
        .arg(dir.path())
        .env("SEMGREP_CACHE_DIR", cache_dir())
        .output()
        .expect("run");
    assert_eq!(present.status.code(), Some(0), "fresh index → exit 0");
    assert!(String::from_utf8_lossy(&present.stdout).contains("index:"));
}
