//! CLI contract tests: exit codes, stream discipline, and exact-mode grep
//! semantics. These assert the promises the README makes to a caller — the
//! things an agent or a shell script depends on and that no library test can
//! see, because they are properties of the *process*.
//!
//! Every test gets its own cache directory, so nothing here touches the
//! developer's real `~/.cache/gorp` and no two tests share cache state.

use std::path::{Path, PathBuf};
use std::process::Command;

fn bin() -> PathBuf {
    // The integration test binary lives in target/<profile>/deps/.
    let mut p = std::env::current_exe().expect("test exe path");
    p.pop();
    if p.ends_with("deps") {
        p.pop();
    }
    p.join("gorp")
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

/// The `N:␣␣text` rows of block-formatted stdout — every printed source line.
/// In exact mode without `-C` these are exactly the matches, which is how the
/// enumeration tests count them now that headers and blank separators share
/// the stream.
fn gutter_rows(stdout: &str) -> Vec<&str> {
    stdout
        .lines()
        .filter(|l| l.split_once(":  ").is_some_and(|(n, _)| n.parse::<u32>().is_ok()))
        .collect()
}

/// A `gorp` invocation with its own cache.
///
/// Per-test rather than per-process on purpose: these tests run real processes
/// in parallel, and an opted-in (`--index`) ranked search writes through to the cache. One
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

    /// As `run_in`, with extra environment. `GORP_TRACE_FILE` is the reason
    /// this exists: the trace surface is deliberately reachable without an
    /// argv change, so it has to be testable without one.
    fn run_in_env(&self, args: &[&str], path: &Path, env: &[(&str, &str)]) -> Run {
        let mut cmd = Command::new(bin());
        cmd.args(args)
            .arg(path)
            .env("GORP_CACHE_DIR", &self.cache)
            .env("GORP_CACHE_TTL_SECS", "0");
        for (k, v) in env {
            cmd.env(k, v);
        }
        let out = cmd.output().expect("run gorp");
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
            .env("GORP_CACHE_DIR", &self.cache)
            .env("GORP_CACHE_TTL_SECS", "0")
            .output()
            .expect("run gorp");
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
        bad.stderr.contains("gorp:"),
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
    // Exact mode speaks the block grammar too: every non-blank line is a
    // `path:start-end` header, a bare elision, or a `line:␣␣text` row.
    for line in r.lines().into_iter().filter(|l| !l.is_empty()) {
        if line == "⋮" {
            continue;
        }
        if line.split_once(":  ").is_some_and(|(n, _)| n.parse::<u32>().is_ok()) {
            continue;
        }
        let (path, span) = line.rsplit_once(':').expect("a header is path:start-end");
        assert!(!path.is_empty(), "header carries a path: {line:?}");
        let (lo, hi) = span.split_once('-').unwrap_or_else(|| panic!("no span in {line:?}"));
        assert!(
            lo.parse::<u32>().is_ok() && hi.parse::<u32>().is_ok(),
            "header span must be numeric, got {span:?} in {line:?}"
        );
    }
    // The footer teaches the next move, and must not pollute stdout.
    assert!(r.stderr.contains("gorp:"), "expected a footer on stderr");
    assert!(!r.stdout.contains("gorp:"), "stdout must carry only results");
}

/// The default: each ranked result is a unit view (RESEARCH.md §34) — a
/// `path:start-end` header, then `line:<tab>text` rows dedented as a block,
/// `⋮` where row numbers jump — and results are separated by a blank line.
/// Blank source lines print as `N:` rows, never as bare blank lines, so the
/// separator stays unambiguous.
///
/// Pinned because three tests in this file counted stdout lines to count
/// results, and all three broke when the default changed — which is exactly
/// what a downstream consumer doing the same thing will experience. The shape
/// is now asserted somewhere rather than only implied by whatever else fails.
#[test]
fn the_default_result_is_a_unit_view() {
    let sg = Sg::new();
    let r = sg.run(&["how is the retry delay computed", "-k", "2"]);
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);

    let blocks: Vec<Vec<&str>> = r
        .stdout
        .split("\n\n")
        .map(|b| b.lines().collect::<Vec<_>>())
        .filter(|b: &Vec<&str>| !b.is_empty())
        .collect();
    assert_eq!(blocks.len(), 2, "one block per result, blank-line separated");
    for b in &blocks {
        let header = b[0];
        let (path, span) = header.rsplit_once(':').expect("first line is path:start-end");
        assert!(!path.is_empty(), "header carries the path: {header:?}");
        let (lo, hi) = span.split_once('-').expect("header span is start-end");
        let (lo, hi): (u32, u32) =
            (lo.parse().expect("span start"), hi.parse().expect("span end"));
        // Body rows: `line:<gutter>text` or a bare elision. Numbers strictly
        // increase and stay inside the span the header promised, and the
        // first and last rows ARE the span — the header states the display,
        // not some wider region the caller cannot see.
        let mut prev = 0u32;
        for l in &b[1..] {
            if *l == "⋮" {
                continue;
            }
            let (n, _) = l.split_once(":  ").unwrap_or_else(|| panic!("not a unit row: {l:?}"));
            let n: u32 = n.parse().unwrap_or_else(|_| panic!("gutter is a number: {l:?}"));
            assert!(n > prev, "row numbers must increase: {b:?}");
            assert!((lo..=hi).contains(&n), "row {n} outside header span {lo}-{hi}");
            prev = n;
        }
        assert_eq!(prev, hi, "the last row is the header span's end");
        // The §34.2 calibration bound: a fine-window-sized core plus at most
        // two heads, a doc line, a close, and short gap fills (16 lines of
        // furniture on top of the window).
        let fine = gorp_core::search::SearchOptions::default().fine_lines as usize;
        assert!(b.len() <= fine + 16, "a unit view stays small, got {}", b.len());
    }
    assert!(!r.stdout.contains("gorp:"), "stdout stays data-only");
}

/// Colour is a terminal-only overlay, and stripping it gives back the exact
/// bytes a pipe gets. Both halves matter: the default must stay uncoloured
/// here — every one of these tests, and every harness, reads gorp through a
/// pipe — and `--color always` must add escapes without moving a single
/// character of the data underneath them.
#[test]
fn color_is_off_under_a_pipe_and_purely_additive_when_forced() {
    let sg = Sg::new();
    let plain = sg.run(&["how is the retry delay computed", "-k", "3"]);
    let painted = sg.run(&["how is the retry delay computed", "-k", "3", "--color", "always"]);
    assert_eq!(plain.code, 0, "stderr: {}", plain.stderr);
    assert_eq!(painted.code, 0, "stderr: {}", painted.stderr);

    assert!(
        !plain.stdout.contains('\x1b'),
        "a piped search must not colour: {:?}",
        plain.stdout
    );
    assert!(painted.stdout.contains("\x1b[94m"), "--color always paints the path blue");
    assert!(painted.stdout.contains("\x1b[2;36m"), "--color always paints line numbers cyan");

    assert_eq!(strip_ansi(&painted.stdout), plain.stdout, "colour must not change the data");

    // `--json` owns its own escaping, and an ANSI escape inside a string
    // field is corrupt data rather than a colour.
    let json = sg.run(&["retry delay", "-k", "2", "--json", "--color", "always"]);
    assert!(!json.stdout.contains('\x1b'), "--json is never coloured: {:?}", json.stdout);
}

/// Every `\x1b[...m` sequence removed. Written out rather than matched on the
/// styles this build happens to use, so re-tuning the palette cannot quietly
/// turn the "colour is additive" assertion above into a weaker one.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('\x1b') {
        out.push_str(&rest[..i]);
        match rest[i..].find('m') {
            Some(j) => rest = &rest[i + j + 1..],
            None => return out + &rest[i..],
        }
    }
    out + rest
}

/// The three things emphasis says, which are three things ripgrep cannot say
/// because it has no ranking: which line the engine chose, which rows are
/// furniture around it, and where the words of the query landed.
#[test]
fn emphasis_marks_the_chosen_line_its_context_and_the_query_words() {
    let sg = Sg::new();
    let r =
        sg.run(&["how is the retry delay computed", "-k", "1", "-C", "2", "--color", "always"]);
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);

    // The best line is bold, and it is exactly one row of the block.
    let best: Vec<&str> =
        r.lines().into_iter().filter(|l| l.starts_with("\x1b[1;36m")).collect();
    assert_eq!(best.len(), 1, "exactly one row is the chosen line: {:?}", r.stdout);

    // `-C` rows are dim: outside the scored window, and marked as such.
    assert!(r.stdout.contains("\x1b[90m"), "context rows carry a grey gutter: {:?}", r.stdout);

    // A query word is emphasised, and it is a word of the query.
    let i = r.stdout.find("\x1b[92m").expect("a query word is emphasised");
    let painted: String = r.stdout[i + 5..].chars().take_while(|c| *c != '\x1b').collect();
    assert!(
        ["retry", "delay", "computed", "compute"].contains(&painted.to_lowercase().as_str()),
        "emphasis lands on a query word, got {painted:?}"
    );

    // And an exact-mode literal emphasises the literal, not its subtokens:
    // `-e compute_backoff_delay` must leave a nearby `Delay` alone.
    let exact = sg.run(&["-e", "compute_backoff_delay", "-C", "1", "--color", "always"]);
    let ctx: Vec<&str> =
        exact.lines().into_iter().filter(|l| l.contains("Delay before")).collect();
    assert_eq!(ctx.len(), 1, "the context line is printed: {:?}", exact.stdout);
    assert!(!ctx[0].contains("\x1b[92m"), "a subtoken is not the literal: {:?}", ctx[0]);
}

/// The gutter between a row's line number and its text is two spaces, not a
/// tab: a tab is 8 columns in one terminal and 4 in another, and any tab in
/// the row's own text then re-aligns everything after it. Asserted on the
/// bytes because it is a contract — `eval/` and gorp-bench's scorer both
/// recognise a no-path hit line by this shape.
#[test]
fn a_unit_row_gutter_is_two_spaces() {
    let sg = Sg::new();
    let r = sg.run(&["how is the retry delay computed", "-k", "2"]);
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert!(!r.stdout.contains('\t'), "no tabs in the gutter: {:?}", r.stdout);
    let rows: Vec<&str> = r
        .lines()
        .into_iter()
        .filter(|l| l.split_once(":  ").is_some_and(|(n, _)| n.parse::<u32>().is_ok()))
        .collect();
    assert!(!rows.is_empty(), "the unit view prints `line:  text` rows: {:?}", r.stdout);
}

/// `--no-unit` is the display control arm (RESEARCH.md §34.3): the bare
/// fine-window passage in `path:line:text` form — the pre-§34 default, byte
/// for byte, which is what makes a disp-unit / disp-nounit A/B a clean
/// comparison. This is the old default-shape test verbatim under the flag.
#[test]
fn no_unit_restores_the_fine_window_passage() {
    let sg = Sg::new();
    let r = sg.run(&["how is the retry delay computed", "-k", "2", "--no-unit"]);
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);

    let blocks: Vec<Vec<&str>> = r
        .stdout
        .split("\n\n")
        .map(|b| b.lines().filter(|l| !l.trim().is_empty()).collect::<Vec<_>>())
        .filter(|b: &Vec<&str>| !b.is_empty())
        .collect();
    assert_eq!(blocks.len(), 2, "one block per result, blank-line separated");
    for b in &blocks {
        let fine = gorp_core::search::SearchOptions::default().fine_lines as usize;
        assert!(b.len() <= fine, "a bare passage is at most the fine window, got {}", b.len());
        let nums: Vec<u32> =
            b.iter().map(|l| l.split(':').nth(1).unwrap_or("x").parse().unwrap_or(0)).collect();
        assert!(nums.iter().all(|&n| n > 0), "every line carries a real number: {b:?}");
        assert!(
            nums.windows(2).all(|w| w[1] == w[0] + 1),
            "passage lines must be consecutive: {nums:?}"
        );
    }
    assert!(!r.stdout.contains("gorp:"), "stdout stays data-only");
}

/// §34: the JSON grows `unit_rows` alongside the default display and drops it
/// under `--no-unit` — the same absent-by-default contract as every optional
/// field, so a consumer of the old shape sees the schema it always saw.
#[test]
fn unit_rows_ride_json_and_the_stable_fields_stand() {
    let sg = Sg::new();
    let j = sg.run(&["how is the retry delay computed", "--json", "-k", "2"]);
    assert_eq!(j.code, 0, "stderr: {}", j.stderr);
    for line in j.lines() {
        let v: serde_json::Value = serde_json::from_str(line).expect("each line is JSON");
        for k in ["path", "start_line", "end_line", "line", "text", "score"] {
            assert!(v.get(k).is_some(), "stable field {k} missing from {line:?}");
        }
    }
    assert!(j.stdout.contains("\"unit_rows\""), "default JSON carries the unit rows");
    let bare = sg.run(&["how is the retry delay computed", "--json", "-k", "2", "--no-unit"]);
    assert!(!bare.stdout.contains("\"unit_rows\""), "--no-unit JSON is the old schema");
}

/// Exact mode's `--json` stays out of §34 entirely: keyword hits never grow
/// unit rows or passages, so the flat per-match schema — the contract the
/// snapshot and gorp-bench read — is untouched by the block display.
#[test]
fn exact_json_stays_flat() {
    let sg = Sg::new();
    let r = sg.run(&["-e", "compute_backoff_delay", "--json"]);
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert!(!r.stdout.is_empty());
    for line in r.lines() {
        let v: serde_json::Value = serde_json::from_str(line).expect("each line is JSON");
        assert!(v.get("unit_rows").is_none(), "no unit rows on a keyword hit: {line:?}");
        assert!(v.get("lines").is_none(), "no passage on a keyword hit: {line:?}");
        assert_eq!(v["start_line"], v["line"], "a keyword hit is one line: {line:?}");
        assert_eq!(v["line"], v["end_line"], "a keyword hit is one line: {line:?}");
    }
}

/// A floored search is a refusal, not a miss: empty stdout, exit 1, and a
/// stderr line naming the floor — the "colder, try again" signal §28.2 found
/// grep sends for free and ranked search never did.
/// `--path X` is accepted as a positional path, because agents type it.
///
/// §28.1 caught it once in 484 searches and §30's R1 four times in 511 — the
/// rise is desc-v10's own doing, since telling an agent to "add a path
/// argument" invites a named flag on an interface that takes a positional.
/// Failing it spends one of the agent's turns teaching argv trivia.
#[test]
fn the_path_flag_is_an_alias_for_a_positional_path() {
    let sg = Sg::new();
    let bare = sg.run(&["how is the retry delay computed", "-k", "3"]);
    let flagged = sg.run_bare(&[
        "how is the retry delay computed",
        "-k",
        "3",
        "--path",
        corpus().to_str().unwrap(),
    ]);
    assert_eq!(flagged.code, 0, "stderr: {}", flagged.stderr);
    assert_eq!(bare.stdout, flagged.stdout, "--path must match the positional form");
}

#[test]
fn a_floored_search_exits_one_with_an_explanation() {
    let sg = Sg::new();
    let r = sg.run(&["quantum chromodynamics lattice gauge", "--min-score", "0.99"]);
    assert_eq!(r.code, 1, "a floored search exits like a miss");
    assert!(r.stdout.is_empty(), "stdout stays data-only: {}", r.stdout);
    assert!(
        r.stderr.contains("too weak to be worth reading"),
        "stderr explains the refusal: {}",
        r.stderr
    );
    // §16.10: a footer that names a flag is a treatment. R1 caught an agent
    // typing `--min-score` with no value after reading this very message.
    assert!(
        !r.stderr.contains("--min-score"),
        "the refusal must not name a flag for the agent to fumble: {}",
        r.stderr
    );
}

/// ...and it must survive `GORP_NO_HINTS`, which every agent harness sets.
///
/// That variable suppresses the `-e` steer (§16.10's 7%→98% posture lever),
/// and the floor's explanation used to sit below the same early return — so
/// under a harness a floored search produced empty stdout, empty stderr and
/// exit 1, which is indistinguishable from an empty scope or a crash. A floor
/// nobody can hear only ever subtracts results (§30).
#[test]
fn the_floor_still_explains_itself_with_hints_off() {
    let sg = Sg::new();
    let r = sg.run_in_env(
        &["quantum chromodynamics lattice gauge", "--min-score", "0.99"],
        &corpus(),
        &[("GORP_NO_HINTS", "1")],
    );
    assert_eq!(r.code, 1);
    assert!(r.stdout.is_empty());
    assert!(
        r.stderr.contains("too weak to be worth reading"),
        "NO_HINTS must not silence a refusal: {:?}",
        r.stderr
    );
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
    assert!(!r.stdout.contains("gorp:"));
    assert!(r.stderr.contains("gorp:"));
}

/// §31: a multi-phrase query keeps the stdout contract — every line is a unit
/// header, a unit row, or an elision (§34) — and the JSON grows a `phrase`
/// field ONLY at K>1, so a single-phrase consumer sees the schema it always
/// saw.
#[test]
fn multi_phrase_queries_keep_the_output_contract() {
    let sg = Sg::new();
    let r = sg.run(&["compute the backoff delay | validate a session token", "-k", "4"]);
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    for line in r.lines().into_iter().filter(|l| !l.trim().is_empty()) {
        if line == "⋮" {
            continue;
        }
        if let Some((n, _)) = line.split_once(":  ") {
            assert!(n.parse::<u32>().is_ok(), "a unit row's gutter is a number: {line:?}");
            continue;
        }
        let span_ok = line.rsplit_once(':').is_some_and(|(path, span)| {
            !path.is_empty()
                && span
                    .split_once('-')
                    .is_some_and(|(a, z)| a.parse::<u32>().is_ok() && z.parse::<u32>().is_ok())
        });
        assert!(span_ok, "header, unit row, or elision: {line:?}");
    }
    let j =
        sg.run(&["compute the backoff delay | validate a session token", "--json", "-k", "4"]);
    assert!(j.stdout.contains("\"phrase\":"), "multi-phrase JSON carries the phrase index");
    let j1 = sg.run(&["compute the backoff delay", "--json", "-k", "4"]);
    assert!(!j1.stdout.contains("\"phrase\":"), "single-phrase JSON is unchanged");
}

/// §31 + §29.2: the partial-floor verdict names the dead phrase on stderr and
/// survives GORP_NO_HINTS, because a verdict is a result, not a hint.
#[test]
fn a_dead_phrase_is_named_on_stderr() {
    let sg = Sg::new();
    let r = sg.run_in_env(
        &[
            "compute the backoff delay | quantum chromodynamics lattice gauge",
            "--min-score",
            "0.42",
        ],
        &corpus(),
        &[("GORP_NO_HINTS", "1")],
    );
    assert_eq!(r.code, 0, "the live phrase still answers: {}", r.stderr);
    assert!(!r.stdout.is_empty());
    assert!(
        r.stderr.contains("nothing matched 'quantum chromodynamics lattice gauge'"),
        "the dead phrase is named: {}",
        r.stderr
    );
    assert!(!r.stderr.contains("--min-score"), "no flag in the verdict");
}

/// §31's free win, made deliberate: an exact-mode miss on a grep-style
/// alternation gets multi-phrase ranked suggestions — the suggestion path
/// runs a full ranked search, which now splits the pattern the agent typed.
#[test]
fn an_exact_alternation_miss_suggests_across_both_phrases() {
    let sg = Sg::new();
    // The suggestion only fires when an index already covers the scope, so
    // warm it first (caching is opt-in, hence `--index`) — the same
    // precondition a real session meets by prewarming.
    sg.run(&["retry", "-k", "1", "--index"]);
    // Neither name exists literally (case mismatch), so -e misses; the ranked
    // suggestion should still reach both concepts' homes.
    let r = sg.run(&["-e", r"Compute_Backoff_Delay\|Validate_Session_Token"]);
    assert_eq!(r.code, 1, "an exact miss is still exit 1");
    assert!(r.stdout.is_empty(), "suggestions stay on stderr");
    assert!(
        r.stderr.contains("ranked search for the same terms finds"),
        "the suggestion fired: {}",
        r.stderr
    );
    assert!(
        r.stderr.contains("retry.rs") && r.stderr.contains("auth.rs"),
        "both phrases' homes suggested: {}",
        r.stderr
    );
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
    let rows = gutter_rows(&r.stdout);
    assert!(rows.len() > 3, "-k must not truncate exact mode; got {}", rows.len());
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
    // Context joins the block as ordinary rows — no `path-line-text` grammar,
    // no `--` separators — and the header span widens to cover it.
    assert!(!r.lines().contains(&"--"), "no -- separators in the block format: {}", r.stdout);
    assert!(!r.stdout.contains(".rs-"), "no dash-form context rows: {}", r.stdout);
    let (_, span) = r.lines()[0].rsplit_once(':').expect("first line is a header");
    let (lo, hi) = span.split_once('-').expect("header span is start-end");
    let (lo, hi): (u32, u32) = (lo.parse().unwrap(), hi.parse().unwrap());
    assert!(hi - lo >= 2, "-C 2 must widen the span past the one matched line: {span:?}");
    assert!(gutter_rows(&r.stdout).len() >= 3, "context rows frame the hit: {}", r.stdout);
}

/// The block is dedented by what its lines *share*, not by the hit's own
/// indent — and what nesting survives is rescaled to two-space levels, so a
/// 4-space file and a tab file read at the same width.
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
    // `class Handler:` is outside the window, so the four framed lines share
    // the `def`'s four spaces and only those come off; the file's 4-space
    // levels then render as 2. What nests deeper stays deeper.
    assert!(
        r.stdout.contains("nest.py:2-5"),
        "the header spans the widened block: {:?}",
        r.stdout
    );
    assert!(
        r.stdout.contains("2:  def add_code(self, payload):"),
        "the shallowest line reaches the margin: {:?}",
        r.stdout
    );
    assert!(
        r.stdout.contains("3:    if payload:"),
        "one level in renders as one two-space level: {:?}",
        r.stdout
    );
    assert!(
        r.stdout.contains("4:      return self.registry.add_code(payload)"),
        "the hit keeps its offset from the block too: {:?}",
        r.stdout
    );
}

/// Exact mode speaks the unit-view grammar (§34), grouped one block per FILE:
/// all of a file's matches under one `path:first-last` header, `⋮` where the
/// line numbers jump, a blank line between files — and none of the old grep
/// grammar (`path:line:text` rows, `--` separators) anywhere.
#[test]
fn exact_mode_prints_one_block_per_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("a.py"), "needle one\nfiller\nfiller\nfiller\nneedle two\n")
        .expect("write");
    std::fs::write(dir.path().join("b.py"), "needle three\n").expect("write");
    let sg = Sg::new();
    let r = sg.run_in(&["-e", "needle"], dir.path());
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);

    let blocks: Vec<Vec<&str>> = r
        .stdout
        .split("\n\n")
        .map(|b| b.lines().collect::<Vec<_>>())
        .filter(|b: &Vec<&str>| !b.is_empty())
        .collect();
    assert_eq!(blocks.len(), 2, "one block per file, blank-line separated: {:?}", r.stdout);
    assert_eq!(blocks[0][0], "a.py:1-5", "header span is first to last match");
    assert_eq!(
        blocks[0][1..],
        ["1:  needle one", "⋮", "5:  needle two"],
        "matches only, the gap elided"
    );
    assert_eq!(blocks[1], ["b.py:1-1", "1:  needle three"]);
    assert!(!r.stdout.contains("--"), "no -- separators: {:?}", r.stdout);
}

/// Exact mode's colour roles: every matched line takes the bold gutter (there
/// is no single chosen line — each match is one), `-C` rows are grey, and
/// overlapping context windows merge into the block without duplicating rows.
/// Colour stays additive, same as ranked mode.
#[test]
fn exact_context_rows_are_grey_and_matches_are_bold() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("f.txt"), "one\nneedle a\nbetween\nneedle b\nfive\n")
        .expect("write");
    let sg = Sg::new();
    let plain = sg.run_in(&["-e", "needle", "-C", "1"], dir.path());
    let painted = sg.run_in(&["-e", "needle", "-C", "1", "--color", "always"], dir.path());
    assert_eq!(painted.code, 0, "stderr: {}", painted.stderr);

    // The two windows (1-3 and 3-5) merge: five rows, no duplicated line 3.
    assert_eq!(plain.lines()[0], "f.txt:1-5", "the header spans the merged windows");
    let nums: Vec<&str> =
        gutter_rows(&plain.stdout).iter().map(|l| l.split_once(':').unwrap().0).collect();
    assert_eq!(nums, ["1", "2", "3", "4", "5"], "merged, sorted, no duplicates");

    let bold: Vec<&str> =
        painted.lines().into_iter().filter(|l| l.starts_with("\x1b[1;36m")).collect();
    assert_eq!(bold.len(), 2, "every matched line is bold: {:?}", painted.stdout);
    assert!(
        painted.stdout.contains("\x1b[90m"),
        "context rows carry the grey gutter: {:?}",
        painted.stdout
    );
    assert_eq!(strip_ansi(&painted.stdout), plain.stdout, "colour must not change the data");
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
    assert!(r.stdout.contains("[… +"), "the cut is declared, not silent");
    assert!(r.stdout.contains(" chars]"), "the marker says how much was cut");
    assert!(r.stdout.contains("real.py"), "the hit ranked below the long line survives it");

    // The escape hatch, for a caller that wants the bytes and knows the cost.
    let full = sg.run_in(&["add_code", "-k", "5", "-M", "0"], dir.path());
    assert_eq!(full.code, 0, "stderr: {}", full.stderr);
    assert!(full.stdout.len() > 100_000, "-M 0 restores the whole line");
    assert!(!full.stdout.contains("[… +"));
}

/// Indentation is the one part of a line that is pure position — already carried
/// by the line number, and reconstructible from the file the hit names.
#[test]
fn result_lines_arrive_without_their_indentation() {
    let sg = Sg::new();
    let r = sg.run(&["-e", "def _reap"]);
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    // Line 0 is the block header; line 1 is the match, dedented to the margin.
    let row = r.lines()[1].to_string();
    let (_, text) = row.split_once(":  ").expect("line:  text row");
    assert!(!text.starts_with(' ') && !text.starts_with('\t'), "indent stripped: {row:?}");
    assert!(text.starts_with("def _reap"), "the code itself is untouched: {row:?}");
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
    // Five since §26.3, not ten: with a passage attached to every result,
    // ranks 6-10 carried half the payload and a tenth of the value.
    assert_eq!(absent.lines().len(), 5, "no -k means the engine default");

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
        .env("GORP_CACHE_DIR", &sg.cache)
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
    let body: String =
        (1..1000)
            .map(|i| {
                if i % 37 == 0 { format!("def fn_{i}(x): pass\n") } else { "# filler\n".into() }
            })
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
        // The count, not just the range: a filter selects which matches
        // count, never how many are reported (the -e truncation defect).
        let expected = (lo..=hi.min(999)).filter(|i| i % 37 == 0).count();
        assert_eq!(got.len(), expected, "--lines {spec} lost matches: {got:?}");
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
        .env("GORP_CACHE_DIR", &sg.cache)
        .output()
        .expect("run");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("target_fn"), "stdin path not searched: {stdout}");
    assert!(!stdout.contains("other"), "searched a path stdin did not name: {stdout}");
}

/// The block format replaces grep's one-named-file rule: the path appears
/// once per block header rather than on every line, so a single named file
/// keeps its header too — once per block is not per-line noise — and `-H`
/// changes nothing. A single file is 53% of real agent invocations
/// (RESEARCH.md §19.9), so the shape is pinned here.
#[test]
fn a_block_header_names_the_file_even_when_only_one_was_given() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("f.py"), "def target(): pass\n").expect("write");
    let f = dir.path().join("f.py");
    let sg = Sg::new();

    let one = sg.run_in(&["-e", "def target"], &f);
    assert_eq!(one.code, 0, "stderr: {}", one.stderr);
    assert_eq!(one.lines()[0], "f.py:1-1", "the header names the file and its span");
    assert_eq!(one.lines()[1], "1:  def target(): pass", "the row leads with the line");

    // -H is a display no-op here: the header already carries the path.
    let forced = sg.run_in(&["-e", "def target", "-H"], &f);
    assert_eq!(forced.stdout, one.stdout, "-H changes nothing in the block format");

    // A directory scope prints the same shape.
    let many = sg.run_in(&["-e", "def target"], dir.path());
    assert_eq!(many.stdout, one.stdout, "one file and its directory agree on the shape");

    // --json and -l are path-carrying formats by definition.
    let js = sg.run_in(&["-e", "def target", "--json"], &f);
    assert!(
        js.lines()[0].contains("\"path\":\"f.py\""),
        "json keeps path: {:?}",
        js.lines()[0]
    );
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
    assert_eq!(envelope["schema"], "gorp.trace/1");
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

    // Warm an index for this scope (caching is opt-in), or the suggestion
    // path declines to run.
    sg.run(&["retry backoff jitter", "-k", "3", "--index"]);
    let r = sg.run_in_env(
        &["-e", "zzz_no_such_symbol_anywhere"],
        &corpus(),
        &[("GORP_TRACE_FILE", &trace_s)],
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

    sg.run(&["retry backoff jitter", "-k", "3", "--index"]); // build the entry (opt-in)
    let r = sg.run_in_env(
        &["retry backoff jitter", "-k", "3"],
        &corpus(),
        &[("GORP_TRACE_FILE", &trace_s)],
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
        &[("GORP_TRACE_FILE", "/nonexistent-dir/nope/trace.jsonl")],
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

    let r = sg.run_in_env(&["index"], dir.path(), &[("GORP_TRACE_FILE", &trace_s)]);
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
fn caching_is_opt_in() {
    // The default leaves nothing behind: a plain ranked search streams,
    // writes no cache entry, and announces no build. `--index` or
    // `GORP_AUTO_INDEX=1` is the opt-in that restores write-through.
    // A published entry is exactly one `meta.json`, so count those.
    fn entries(dir: &Path) -> usize {
        fn walk(dir: &Path, n: &mut usize) {
            let Ok(rd) = std::fs::read_dir(dir) else { return };
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, n);
                } else if p.file_name().is_some_and(|f| f == "meta.json") {
                    *n += 1;
                }
            }
        }
        let mut n = 0;
        walk(dir, &mut n);
        n
    }

    let sg = Sg::new();
    // GORP_AUTO_INDEX pinned to "0" so a developer's own opt-in cannot
    // leak into the child and turn the restraint half of this test warm.
    let r = sg.run_in_env(
        &["retry backoff jitter", "-k", "3"],
        &corpus(),
        &[("GORP_AUTO_INDEX", "0")],
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert!(!r.stderr.contains("caching it"), "no build was asked for, none may be announced");
    assert_eq!(entries(&sg.cache), 0, "a default search must leave the cache empty");

    let opted = sg.run_in_env(
        &["retry backoff jitter", "-k", "3"],
        &corpus(),
        &[("GORP_AUTO_INDEX", "1")],
    );
    assert_eq!(opted.code, 0, "stderr: {}", opted.stderr);
    assert!(opted.stderr.contains("caching it"), "the opt-in announces the build it pays for");
    assert_eq!(entries(&sg.cache), 1, "GORP_AUTO_INDEX=1 writes the entry the default withheld");
}

#[test]
fn cache_status_reports_the_generation_and_budget() {
    let sg = Sg::new();
    sg.run(&["retry backoff jitter", "-k", "3", "--index"]); // warm an entry (opt-in)
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
    let names =
        ["ordinary.py", "with space.py", "-dash.py", "qu\"ote.py", "od:d.py", "we\nird.py"];
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
    let blocks: Vec<Vec<&str>> = r
        .stdout
        .split("\n\n")
        .map(|b| b.lines().collect::<Vec<_>>())
        .filter(|b: &Vec<&str>| !b.is_empty())
        .collect();
    assert_eq!(
        blocks.len(),
        written,
        "{written} files on disk must give {written} blocks, got {:?}",
        r.stdout
    );

    // Every header must still parse as path:span — the promise that makes the
    // block grammar consumable. A quoted path is unambiguous; a bare one must
    // not contain the separator.
    for b in &blocks {
        let header = b[0];
        let (path, rest) = if let Some(after) = header.strip_prefix('"') {
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
            let i = header.find(':').expect("an unquoted header has a separator");
            (&header[..i], &header[i..])
        };
        assert!(!path.is_empty(), "empty path in {header:?}");
        let span = rest.strip_prefix(':').expect("path is followed by :span");
        assert_eq!(span, "1-1", "one match on line 1 in {header:?}");
        assert_eq!(b.len(), 2, "one match means one row: {b:?}");
        assert_eq!(b[1], format!("1:  {}", body.trim_end()), "wrong row in {b:?}");
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

    let objs: Vec<serde_json::Value> = r
        .stdout
        .lines()
        .map(|l| serde_json::from_str(l).expect("valid JSON per line"))
        .collect();
    assert_eq!(objs.len(), on_disk, "one object per file: {:?}", r.stdout);
    for o in &objs {
        let p = o["path"].as_str().expect("path is a string");
        assert!(!p.starts_with('"'), "--json must not double-quote {p:?}");
    }
}

/// Grep muscle memory must parse (RESEARCH.md §17).
///
/// Measured on the rg arm of the §16.10 campaign: `-n` appeared in 1,829 of
/// 2,074 real agent invocations (88%), and gorp exited 2 on every one. The
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
        .env("GORP_CACHE_DIR", &sg.cache)
        .env("GORP_CACHE_TTL_SECS", "0");
    let out = cmd.output().expect("run gorp");
    assert_eq!(out.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!stdout.is_empty(), "multi-path search returned nothing");
    let mut headers = 0;
    for line in stdout.lines().filter(|l| !l.trim().is_empty()) {
        // Since §34 the path lives on each hit's header line; body rows and
        // elision markers carry none.
        if line == "⋮" || line.split_once(":  ").is_some_and(|(n, _)| n.parse::<u32>().is_ok())
        {
            continue;
        }
        headers += 1;
        let path = line.split(':').next().unwrap_or("");
        assert!(
            path.starts_with("src/") || path.starts_with("docs/"),
            "hit outside the requested paths: {line:?}"
        );
    }
    assert!(headers > 0, "at least one hit header names its path");
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

// ---------------------------------------------------------------------------
// production hardening: argv edge cases, bounded retention, reclamation
// ---------------------------------------------------------------------------

/// `gorp ""` used to embed the empty string and return an arbitrary-looking
/// top-k; `gorp -e ""` matched every line of the corpus. Both are usage
/// errors: exit 2, nothing on stdout.
#[test]
fn an_empty_query_is_a_usage_error() {
    let sg = Sg::new();
    for args in [&[""][..], &["   "][..], &["-e", ""][..]] {
        let r = sg.run(args);
        assert_eq!(r.code, 2, "{args:?} must be a usage error, not a search");
        assert!(r.stdout.is_empty(), "{args:?} must print nothing to stdout");
        assert!(
            r.stderr.contains("usage"),
            "{args:?} should point at usage, got: {}",
            r.stderr
        );
    }
}

/// `-k` flows into allocations and width arithmetic; an absurd value must be
/// a range error that names the legal bound (the caller is usually an agent
/// that invented the number), never an allocation abort.
#[test]
fn an_absurd_k_is_a_range_error_not_an_abort() {
    let sg = Sg::new();
    let r = sg.run(&["-k", "99999999999999", "retry backoff"]);
    assert_eq!(r.code, 2, "out-of-range -k is a usage error");
    assert!(r.stderr.contains("10000"), "the error names the bound: {}", r.stderr);

    let ok = sg.run(&["-k", "10000", "-e", "compute_backoff_delay"]);
    assert_eq!(ok.code, 0, "the top of the range is accepted\nstderr: {}", ok.stderr);
}

/// Exact mode prints at most 250 matches but must count (and be able to show
/// with --all) every one — and the printed 250 are the head of the sorted
/// output even though retention now bounds what the scan keeps in memory.
#[test]
fn exact_mode_caps_printing_but_counts_everything() {
    let dir = tempfile::tempdir().unwrap();
    for name in ["a.txt", "b.txt", "c.txt"] {
        let body: String = (1..=100).map(|i| format!("needle line {i}\n")).collect();
        std::fs::write(dir.path().join(name), body).unwrap();
    }
    let sg = Sg::new();
    let r = sg.run_in(&["-e", "needle"], dir.path());
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(gutter_rows(&r.stdout).len(), 250, "printing is capped at 250 matched rows");
    // The head of the sort prints first, and the cap cuts the last block short.
    let headers: Vec<&str> = r.lines().into_iter().filter(|l| l.contains(".txt:")).collect();
    assert_eq!(
        headers,
        ["a.txt:1-100", "b.txt:1-100", "c.txt:1-50"],
        "blocks arrive in (path, line) order and the 250th match ends the last"
    );
    assert!(
        r.stderr.contains("showing 250 of 300 matches in 3 files"),
        "the true totals survive bounded retention: {}",
        r.stderr
    );

    let all = sg.run_in(&["-e", "needle", "--all"], dir.path());
    assert_eq!(gutter_rows(&all.stdout).len(), 300, "--all still prints every match");
}

/// A permission-denied subtree must not be silently invisible: on a zero-hit
/// search the caller is told the walk skipped things it could not read,
/// because "no results" over a partly-unreadable scope is otherwise silence
/// that looks like a real answer (the §16.11 shape).
#[cfg(unix)]
#[test]
fn an_unreadable_subdirectory_is_reported_not_silently_skipped() {
    use std::os::unix::fs::PermissionsExt;
    if unsafe { libc::geteuid() } == 0 {
        return; // root reads anything; the scenario cannot be constructed
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("open.txt"), "nothing relevant here\n").unwrap();
    let secret = dir.path().join("secret");
    std::fs::create_dir(&secret).unwrap();
    std::fs::write(secret.join("hidden.txt"), "the needle lives here\n").unwrap();
    std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o000)).unwrap();

    let sg = Sg::new();
    let r = sg.run_in(&["-e", "needle"], dir.path());
    std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o755)).unwrap();

    assert_eq!(r.code, 1, "stdout: {}\nstderr: {}", r.stdout, r.stderr);
    assert!(
        r.stderr.contains("could not be read"),
        "walk errors surface on a zero-hit search: {}",
        r.stderr
    );
}

/// `gorp index` with stderr piped (an agent harness, CI) must not emit
/// `\r`-rewriting progress — that is terminal furniture. The summary line
/// still prints.
#[test]
fn index_progress_is_tty_only_but_the_summary_is_not() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.rs"), "fn alpha() {}\n").unwrap();
    let sg = Sg::new();
    let r = sg.run_in(&["index"], dir.path());
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(!r.stderr.contains('\r'), "no carriage returns in captured stderr: {:?}", r.stderr);
    assert!(!r.stderr.contains("indexing "), "per-batch progress is TTY-only: {}", r.stderr);
    assert!(r.stderr.contains("indexed "), "the summary still prints: {}", r.stderr);
}

/// `-R` stays accepted (grep muscle memory), even though gorp never follows
/// symlinks — its help text says so rather than promising dereferencing.
#[test]
fn dash_r_is_still_accepted() {
    let sg = Sg::new();
    assert_eq!(sg.run(&["-R", "-e", "compute_backoff_delay"]).code, 0);
}

/// `--version` carries build provenance (the same fields the telemetry
/// envelope stamps); `-V` stays the terse one-liner.
#[test]
fn version_reports_build_provenance() {
    let sg = Sg::new();
    let long = sg.run_bare(&["--version"]);
    assert_eq!(long.code, 0);
    assert!(long.stdout.contains(env!("CARGO_PKG_VERSION")), "{}", long.stdout);
    assert!(long.stdout.contains("embed dim:"), "provenance block present: {}", long.stdout);
    assert!(long.stdout.contains("compat key:"), "provenance block present: {}", long.stdout);

    let short = sg.run_bare(&["-V"]);
    assert_eq!(short.code, 0);
    assert_eq!(short.stdout.lines().count(), 1, "-V is one line: {}", short.stdout);
}

// ---------------------------------------------------------------------------
// the 2026-08 flag redesign: exact filters enumerate, -w/-c, ranked -C
// ---------------------------------------------------------------------------

/// `-e` plus a filter must still enumerate: the filter selects which matches
/// count, never how many are reported. This used to truncate to the ranked
/// default of 5 whenever any filter was active — multiple paths, `-g`, or
/// `--lines` — and the footer restated the totals to match, so the loss was
/// self-consistent and invisible.
#[test]
fn exact_mode_with_multiple_paths_returns_every_match() {
    let dir = tempfile::tempdir().unwrap();
    for sub in ["one", "two"] {
        std::fs::create_dir(dir.path().join(sub)).unwrap();
        let body: String = (1..=10).map(|i| format!("needle {i}\n")).collect();
        std::fs::write(dir.path().join(sub).join("f.txt"), body).unwrap();
    }
    let sg = Sg::new();
    let out = std::process::Command::new(bin())
        .args(["-e", "needle"])
        .arg(dir.path().join("one"))
        .arg(dir.path().join("two"))
        .env("GORP_CACHE_DIR", &sg.cache)
        .env("GORP_CACHE_TTL_SECS", "0")
        .output()
        .expect("run gorp");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "stderr: {stderr}");
    assert_eq!(gutter_rows(&stdout).len(), 20, "every match, not the ranked k: {stdout}");
    assert!(
        !stderr.contains("narrowed to the paths"),
        "filtering to the paths given is exact mode's ordinary contract: {stderr}"
    );
}

#[test]
fn exact_mode_with_a_glob_returns_every_match() {
    let dir = tempfile::tempdir().unwrap();
    let body: String = (1..=12).map(|i| format!("needle {i}\n")).collect();
    std::fs::write(dir.path().join("a.py"), &body).unwrap();
    std::fs::write(dir.path().join("b.txt"), &body).unwrap();
    let sg = Sg::new();
    let r = sg.run_in(&["-e", "needle", "-g", "*.py"], dir.path());
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(
        gutter_rows(&r.stdout).len(),
        12,
        "every .py match survives the glob: {}",
        r.stdout
    );
    assert!(r.stdout.contains("a.py:1-12"), "the .py file heads its block: {}", r.stdout);
    assert!(!r.stdout.contains("b.txt"), "glob leaked: {}", r.stdout);
}

/// `-w`: whole words only, and `-wF` word-matches the literal (grep
/// semantics, straight from the engine's own word() builder).
#[test]
fn word_regexp_matches_whole_words() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "foo\nfoobar\na.b\naXb\n").unwrap();
    let sg = Sg::new();
    assert_eq!(gutter_rows(&sg.run_in(&["-e", "foo"], dir.path()).stdout).len(), 2);
    let word = sg.run_in(&["-e", "-w", "foo"], dir.path());
    assert_eq!(word.code, 0, "stderr: {}", word.stderr);
    assert_eq!(gutter_rows(&word.stdout).len(), 1, "-w must exclude foobar: {}", word.stdout);

    let lit = sg.run_in(&["-e", "-w", "-F", "a.b"], dir.path());
    assert_eq!(
        gutter_rows(&lit.stdout).len(),
        1,
        "-wF word-matches the literal: {}",
        lit.stdout
    );
    assert!(lit.stdout.contains("a.b"), "{}", lit.stdout);

    // Combined shorts parse, same as the grep-compat set.
    assert_ne!(sg.run_in(&["-e", "-wiF", "FOO"], dir.path()).code, 2);
}

/// In ranked mode `-w` is accepted by construction: ranked matching is
/// subtoken-based, so there is nothing for a word boundary to constrain and
/// the output must be byte-identical to the flagless run.
#[test]
fn word_regexp_is_inert_in_ranked_mode() {
    let sg = Sg::new();
    let base = sg.run(&["-k", "3", "retry backoff"]);
    let word = sg.run(&["-w", "-k", "3", "retry backoff"]);
    assert_eq!(word.code, base.code);
    assert_eq!(word.stdout, base.stdout, "-w must not change ranked output");
}

/// `-c` prints per-file counts: the true totals even past the 250-line
/// retention bound, `path:count` per line in the hits' own order.
#[test]
fn count_prints_per_file_totals_past_retention() {
    let dir = tempfile::tempdir().unwrap();
    for name in ["a.txt", "b.txt", "c.txt"] {
        let body: String = (1..=100).map(|i| format!("needle line {i}\n")).collect();
        std::fs::write(dir.path().join(name), body).unwrap();
    }
    let sg = Sg::new();
    let r = sg.run_in(&["-e", "needle", "-c"], dir.path());
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.lines(), vec!["a.txt:100", "b.txt:100", "c.txt:100"], "{}", r.stdout);
    assert!(
        !r.stderr.contains("matches in"),
        "the footer must not restate what stdout just said: {}",
        r.stderr
    );

    // -c beats -l, grep's own precedence.
    let both = sg.run_in(&["-e", "needle", "-c", "-l"], dir.path());
    assert_eq!(both.lines(), vec!["a.txt:100", "b.txt:100", "c.txt:100"], "{}", both.stdout);

    // A filter narrows what gets counted, not whether counting is honest.
    let globbed = sg.run_in(&["-e", "needle", "-c", "-g", "a.txt"], dir.path());
    assert_eq!(globbed.lines(), vec!["a.txt:100"], "{}", globbed.stdout);

    // --json: one {"path", "count"} object per line.
    let json = sg.run_in(&["-e", "needle", "-c", "--json"], dir.path());
    for line in json.lines() {
        let v: serde_json::Value = serde_json::from_str(line).expect("count json parses");
        assert!(v["path"].is_string() && v["count"].is_u64(), "{line}");
    }

    // A miss: empty stdout, exit 1 — consistent with the no-zero-rows rule.
    let miss = sg.run_in(&["-e", "absent_needle", "-c"], dir.path());
    assert_eq!(miss.code, 1);
    assert!(miss.stdout.is_empty(), "no zero-count rows: {}", miss.stdout);
}

/// grep's single-file rule applies to `-c` too: one named file prints a bare
/// count, and `-H` restores the path.
#[test]
fn count_drops_the_path_for_one_named_file() {
    let dir = tempfile::tempdir().unwrap();
    let body: String = (1..=7).map(|i| format!("needle {i}\n")).collect();
    std::fs::write(dir.path().join("f.txt"), body).unwrap();
    let f = dir.path().join("f.txt");
    let sg = Sg::new();
    let bare = sg.run_in(&["-e", "needle", "-c"], &f);
    assert_eq!(bare.lines(), vec!["7"], "{}", bare.stdout);
    let with_path = sg.run_in(&["-e", "needle", "-c", "-H"], &f);
    assert_eq!(with_path.lines(), vec!["f.txt:7"], "{}", with_path.stdout);
}

/// Ranked `-c` is the counting analog of `-l`: per-file counts of the k
/// ranked hits, in rank order.
#[test]
fn count_in_ranked_mode_counts_the_ranked_hits() {
    let sg = Sg::new();
    let r = sg.run(&["-c", "-k", "5", "retry backoff"]);
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    let lines = r.lines();
    assert!(!lines.is_empty() && lines.len() <= 5, "{}", r.stdout);
    let mut total = 0usize;
    for l in &lines {
        let (_, n) = l.rsplit_once(':').expect("path:count shape");
        total += n.parse::<usize>().expect("count is a number");
    }
    assert_eq!(total, 5, "the counts sum to the k hits: {}", r.stdout);
}

/// Ranked `-C/-A/-B`: context widens the unit view — the header span grows
/// with the rows, so first/last row == header span stays true, and every row
/// still parses in the one row grammar.
#[test]
fn context_widens_the_unit_view_in_ranked_mode() {
    let sg = Sg::new();
    let span = |r: &Run| -> (u32, u32) {
        let header = r.lines()[0];
        let (a, b) = header.rsplit_once(':').unwrap().1.split_once('-').unwrap();
        (a.parse().unwrap(), b.parse().unwrap())
    };
    let base = sg.run(&["-k", "1", "retry backoff"]);
    assert_eq!(base.code, 0, "stderr: {}", base.stderr);
    let (b_first, b_last) = span(&base);

    let ctx = sg.run(&["-k", "1", "-C", "3", "retry backoff"]);
    let (c_first, c_last) = span(&ctx);
    assert!(
        c_first <= b_first && c_last >= b_last && (c_first, c_last) != (b_first, b_last),
        "-C must widen the span: {b_first}-{b_last} vs {c_first}-{c_last}"
    );
    assert!(
        ctx.lines().len() > base.lines().len(),
        "-C must add rows:\n{}\nvs\n{}",
        base.stdout,
        ctx.stdout
    );
    // Every non-header line is still a unit row or an elision marker.
    for line in ctx.lines().iter().skip(1).filter(|l| !l.trim().is_empty()) {
        assert!(
            *line == "⋮"
                || line.split_once(":  ").is_some_and(|(n, _)| n.parse::<u32>().is_ok())
                || line.rsplit_once(':').is_some_and(|(_, s)| s.contains('-')),
            "row grammar broken by -C: {line:?}"
        );
    }

    // -A alone grows only the tail.
    let after = sg.run(&["-k", "1", "-A", "2", "retry backoff"]);
    let (a_first, a_last) = span(&after);
    assert_eq!(a_first, b_first, "-A must not touch the head");
    assert!(a_last >= b_last, "-A grows the tail");

    // Context never rides --json: the schema is frozen.
    let json_base = sg.run(&["--json", "-k", "2", "retry backoff"]);
    let json_ctx = sg.run(&["--json", "-k", "2", "-C", "3", "retry backoff"]);
    assert_eq!(json_ctx.stdout, json_base.stdout, "-C must not change --json");
}

/// Ranked `-C` under `--no-unit` widens the passage in its own shape.
#[test]
fn context_widens_the_passage_under_no_unit() {
    let sg = Sg::new();
    let base = sg.run(&["--no-unit", "-k", "1", "retry backoff"]);
    let ctx = sg.run(&["--no-unit", "-k", "1", "-C", "3", "retry backoff"]);
    assert_eq!(ctx.code, 0, "stderr: {}", ctx.stderr);
    assert!(
        ctx.lines().len() > base.lines().len(),
        "-C must widen the passage:\n{}\nvs\n{}",
        base.stdout,
        ctx.stdout
    );
}

/// The visible surface is the README's Usage table and nothing else: the
/// tuning knobs, the trace surface, and the operator probes stay functional
/// but hidden.
#[test]
fn help_lists_the_promised_surface_and_nothing_hidden() {
    let sg = Sg::new();
    let help = sg.run_bare(&["--help"]);
    assert_eq!(help.code, 0);
    for visible in
        ["-e", "-w,", "-c,", "-k,", "-C,", "-g,", "--lines", "--json", "--stats", "cache"]
    {
        assert!(help.stdout.contains(visible), "{visible} missing from --help");
    }
    for hidden in
        ["--passage-chars", "--stats-json", "--check-stale", "--mode", "--path ", "-n,"]
    {
        assert!(!help.stdout.contains(hidden), "{hidden} must stay hidden");
    }

    // Hidden is not gone: each still parses.
    assert_ne!(sg.run(&["--stats-json", "-k", "2", "retry backoff"]).code, 2);
    assert_ne!(sg.run(&["--check-stale", "-k", "2", "retry backoff"]).code, 2);
    assert_ne!(sg.run(&["--passage-chars", "100", "-k", "2", "retry backoff"]).code, 2);
}
