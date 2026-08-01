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
