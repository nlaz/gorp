//! Tests for [`super::out`]: clipping, quoting and the unit renderer.

use super::{Emphasis, color_enabled, keyword_rows, paint, quote_path};

/// The six names the simulation put on disk (SIMULATION.md §1.5). Six files
/// produced seven stdout lines, and one of the six mis-parsed silently.
#[test]
fn only_ambiguous_names_are_quoted() {
    // Untouched — and this is the load-bearing half. A quoting scheme that
    // fires on ordinary paths would move `tools/snapshot.sh` and make every
    // consumer unquote every line.
    for plain in [
        "ordinary.py",
        "with space.py",
        "-dash.py",
        "src/rank/bm25.rs",
        "qu'ote.py",
        "über/naïve.py",
    ] {
        assert_eq!(quote_path(plain), plain, "{plain:?} must pass through");
    }

    assert_eq!(quote_path("we\nird.py"), r#""we\nird.py""#);
    assert_eq!(quote_path("od:d.py"), r#""od:d.py""#);
    assert_eq!(quote_path("qu\"ote.py"), r#""qu\"ote.py""#);
    assert_eq!(quote_path("tab\there.py"), r#""tab\there.py""#);
}

/// A quoted path must stay on one line, because that is the entire point:
/// the break this fixes was one hit arriving as two records.
#[test]
fn a_quoted_path_never_contains_a_raw_control_character() {
    for hostile in ["a\nb", "a\rb", "a\tb", "a\u{0}b", "a\u{1b}b", "a:b\nc"] {
        let q = quote_path(hostile);
        assert!(!q.chars().any(char::is_control), "{hostile:?} -> {q:?} still has a control");
        assert!(q.starts_with('"') && q.ends_with('"'), "{hostile:?} -> {q:?} is not quoted");
    }
}

/// Backslash is not a trigger — an unquoted one is unambiguous — but once
/// something else forces quoting it has to escape itself, or unquoting a
/// name like `a\nb` (literal backslash, letter n) gives back a newline.
#[test]
fn backslash_escapes_itself_only_inside_quotes() {
    assert_eq!(quote_path(r"back\slash.py"), r"back\slash.py");
    assert_eq!(quote_path("back\\slash:x.py"), r#""back\\slash:x.py""#);
}

/// Colour is additive: strip the escapes and the bytes are the uncoloured
/// ones, exactly. That is the property the `path:line:text` contract rests on
/// when a human is watching, and it is cheaper to assert here than to hunt
/// down later as a parser that started seeing `\x1b[35msrc` as a path.
#[test]
fn painting_only_wraps_and_never_edits() {
    assert_eq!(paint(false, "\x1b[35m", "src/rank/bm25.rs"), "src/rank/bm25.rs");
    let on = paint(true, "\x1b[35m", "src/rank/bm25.rs");
    assert_eq!(on, "\x1b[35msrc/rank/bm25.rs\x1b[0m");
    assert_eq!(on.replace("\x1b[35m", "").replace("\x1b[0m", ""), "src/rank/bm25.rs");
}

/// Ranked emphasis runs through the engine's tokenizer, which is the whole
/// point: the reader is shown the same subtokens the ranker scored on, so a
/// query of "user name" lights up `getUserName` and a query the text answers
/// without sharing a word lights up nothing at all.
#[test]
fn ranked_emphasis_follows_the_engine_tokenizer() {
    let e = Emphasis::of("how is the user name validated");
    let painted = |text: &str| e.apply(text, "", true);

    assert!(painted("fn getUserName(&self)").contains("\x1b[92mgetUserName\x1b[0m"));
    assert!(painted("let user_name = 1;").contains("\x1b[92muser_name\x1b[0m"));
    // Stopwords and short words carry no location and are dropped, or every
    // row of every result would light up.
    assert_eq!(painted("how is the cat"), "how is the cat");
    // A semantic hit that shares no word is emphasised nowhere, and that is
    // information rather than a failure.
    assert_eq!(painted("def rotate(x): return x"), "def rotate(x): return x");
}

/// Exact-mode emphasis is the literal and only the literal. Painting a
/// subtoken there would mark text the pattern did not match — the one thing
/// emphasis must never do, because `-e` output is used as proof.
#[test]
fn literal_emphasis_does_not_decompose_the_pattern() {
    let e = Emphasis::literal("compute_backoff_delay");
    assert!(e.apply("fn compute_backoff_delay(n)", "", true).contains("\x1b[92m"));
    assert_eq!(e.apply("/// Delay before attempt", "", true), "/// Delay before attempt");
}

/// The row budget: at most two runs painted, the rest printed plain. Nothing
/// is hidden by it — the line prints in full, only the paint stops — and a
/// row that lights up three times has stopped pointing at anything anyway.
#[test]
fn a_row_paints_at_most_twice() {
    let e = Emphasis::of("watcher parcel throttle");
    let row = e.apply("const watcher = parcel.throttle(watcher)", "", true);
    assert_eq!(row.matches("\x1b[92m").count(), 2, "cap holds: {row:?}");
    assert!(row.contains("throttle"), "the unpainted run still prints: {row:?}");

    // Not in exact mode: there the emphasis IS the match, and rationing it
    // would hide a hit the caller asked for.
    let lit = Emphasis::literal("watcher");
    let row = lit.apply("watcher watcher watcher", "", true);
    assert_eq!(row.matches("\x1b[92m").count(), 3, "a literal is never rationed: {row:?}");
}

/// The result budget: a word this answer repeats everywhere stops being
/// emphasised in it, because a word in every row distinguishes no row. The
/// cost is that emphasis is result-dependent — asserted here so the trade is
/// visible in the tests and not only in a comment.
#[test]
fn a_word_in_every_row_stops_being_painted() {
    let e = Emphasis::of("watcher throttle");
    let rows = [
        "class Watcher {",
        "  watcher.start()",
        "  watcher.stop()",
        "  return watcher",
        "  let w = watcher",
        "  throttle(w)",
        "  drop(watcher)",
    ];
    let live = e.for_result(rows.iter().copied());
    assert!(!live.apply("watcher.start()", "", true).contains("\x1b[92m"), "common word drops");
    assert!(live.apply("throttle(w)", "", true).contains("\x1b[92m"), "the rare one stays");

    // Too few rows to judge a distribution on: nothing is dropped.
    let live = e.for_result(["watcher", "watcher"].iter().copied());
    assert!(live.apply("watcher.start()", "", true).contains("\x1b[92m"), "no cut on 2 rows");
}

/// A base style survives an emphasis inside it: the reset that closes the
/// emphasised word closes the base too, so a dim context row would stop being
/// dim halfway through the line unless the base is re-opened.
#[test]
fn a_base_style_is_reopened_after_each_emphasis() {
    let e = Emphasis::of("retry backoff");
    let dim = e.apply("retry the backoff now", "\x1b[2m", true);
    assert!(dim.starts_with("\x1b[2m"), "{dim:?}");
    assert!(dim.ends_with("\x1b[0m"), "{dim:?}");
    assert_eq!(dim.matches("\x1b[2m").count(), 3, "base re-opened after each word: {dim:?}");
    // Colour off is the identity, base style or not.
    assert_eq!(e.apply("retry the backoff now", "\x1b[2m", false), "retry the backoff now");
}

/// The display re-indent: tabs and 4/8-space files rescale to two-space
/// levels, a 2-space file passes through byte-identical, and an irregular
/// block (a GCD of 1) is left alone rather than guessed at.
#[test]
fn re_indent_rescales_levels_and_leaves_the_illegible_alone() {
    use super::{indent_unit, two_space};

    // 4-space code: unit 4, one level renders as one two-space level, and
    // alignment past a level survives as the spaces it was.
    let four = ["def f():", "    if x:", "        g()"];
    let unit = indent_unit(four.iter().copied());
    assert_eq!(unit, 4);
    assert_eq!(two_space("    if x:", unit), "  if x:");
    assert_eq!(two_space("        g()", unit), "    g()");
    assert_eq!(two_space("      y)", unit), "    y)", "4+2 is a level plus alignment");

    // Tabs need no unit: one tab is one level wherever it appears.
    let tabs = ["suite() {", "\ttest() {", "\t\tgo();"];
    assert_eq!(indent_unit(tabs.iter().copied()), 0, "no leading spaces, no unit");
    assert_eq!(two_space("\t\tgo();", 0), "    go();");

    // Already two-space: the identity, without allocating.
    assert!(matches!(two_space("  b", 2), std::borrow::Cow::Borrowed("  b")));

    // Irregular (gcd 1): rescaling would misdraw it, so nothing moves.
    let odd = ["   a", "     b"];
    assert_eq!(indent_unit(odd.iter().copied()), 0);
    assert_eq!(two_space("     b", 0), "     b");
}

/// A keyword hit as the engine emits one: a single line, nothing optional.
fn khit(path: &str, line: u32, text: &str) -> gorp_core::search::SearchHit {
    gorp_core::search::SearchHit {
        path: path.into(),
        start_line: line,
        end_line: line,
        line,
        text: text.into(),
        score: 1.0,
        chunk_start_line: None,
        chunk_end_line: None,
        phrase: None,
        lines: None,
        lines_from: None,
        defines: None,
        unit_rows: None,
        features: None,
    }
}

/// The per-file row builder behind exact mode's blocks: overlapping `-C`
/// windows merge without duplicate rows, context clamps to the file's bounds,
/// and a file that cannot be re-read loses its context but never a match —
/// the matches still carry the text the scan saw.
#[test]
fn keyword_rows_merge_windows_and_survive_a_missing_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let body: String = (1..=10).map(|i| format!("line {i}\n")).collect();
    std::fs::write(dir.path().join("f.txt"), body).expect("write");
    let hits =
        [khit("f.txt", 2, "line 2"), khit("f.txt", 4, "line 4"), khit("f.txt", 9, "line 9")];

    // No context: the matches themselves, no file read.
    let bare = keyword_rows(dir.path(), &hits, 0, 0);
    assert_eq!(bare.iter().map(|r| r.line).collect::<Vec<_>>(), [2, 4, 9]);

    // -C 1: the 2±1 and 4±1 windows overlap at 3 and merge; the edges clamp
    // to lines 1 and 10.
    let rows = keyword_rows(dir.path(), &hits, 1, 1);
    assert_eq!(rows.iter().map(|r| r.line).collect::<Vec<_>>(), [1, 2, 3, 4, 5, 8, 9, 10]);
    assert_eq!(rows[0].text, "line 1", "context is re-read from the file");

    // A file that cannot be re-read: context vanishes, the match does not.
    let gone = [khit("gone.txt", 3, "scanned text")];
    let rows = keyword_rows(dir.path(), &gone, 2, 2);
    assert_eq!(rows.len(), 1, "no invented context for an unreadable file");
    assert_eq!((rows[0].line, rows[0].text.as_str()), (3, "scanned text"));
}

/// `never` and `always` are unconditional, and `auto` is false here because
/// a test's stdout is not a terminal — the same reason it is false under a
/// pipe, which is what keeps harness output uncoloured without a harness
/// having to say so.
#[test]
fn color_auto_is_off_when_nobody_is_watching() {
    assert!(color_enabled("always"));
    assert!(!color_enabled("never"));
    assert!(!color_enabled("auto"));
}
