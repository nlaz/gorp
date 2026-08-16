//! Tests for [`super::out`]: clipping, quoting and the unit renderer.

use super::{Emphasis, color_enabled, paint, quote_path};

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

    assert!(painted("fn getUserName(&self)").contains("\x1b[1;92mgetUserName\x1b[0m"));
    assert!(painted("let user_name = 1;").contains("\x1b[1;92muser_name\x1b[0m"));
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
    assert!(e.apply("fn compute_backoff_delay(n)", "", true).contains("\x1b[1;92m"));
    assert_eq!(e.apply("/// Delay before attempt", "", true), "/// Delay before attempt");
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
