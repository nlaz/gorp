//! The unit view's row selection: which lines around a hit's window earn a
//! place in the output (RESEARCH.md §34).
//!
//! The fine window is the *scoring* granule; alone it reads as an orphan —
//! four lines of a body with nothing saying whose. This module picks the
//! rows that de-orphan it, under rules calibrated by the §34 noise audit of
//! 90 rendered hits across ten languages: every category of added row was
//! either redundant with something already on screen (a namespace head
//! restating the path, a closing brace restating the header span) or it
//! carried a name/boundary nothing else states. Only the second kind
//! survives here. The audit's arithmetic: uncalibrated, added rows doubled
//! every hit; calibrated, ~1.5 added rows per hit carry the value.
//!
//! Everything is an indentation heuristic over raw lines — no parser, no
//! language table — because that is what the pilot measured and it degrades
//! to "no added rows" on prose and on files the rules cannot read.
//! `corpus::funcchunk` has real tree-sitter definition boundaries (with
//! comment attachment) and is the refinement path if the heuristic's known
//! misses ever matter enough; it is per-language and costs a parse per hit,
//! so it is not v1.
//!
//! Pure line geometry, no I/O: `compute` sees the file only as `&[&str]`,
//! which is what makes every rule in this module unit-testable and keeps
//! the cold and warm paths trivially identical (both call it on the same
//! re-read bytes).

use super::UnitRow;
use crate::text;
use std::collections::BTreeSet;

/// The unit-view rows for a hit whose (already fine-chosen) window spans
/// `start..=end`, 1-based. Returned in file order; a jump between
/// consecutive rows' line numbers is an elision the renderer marks.
///
/// The window rows are always present. Around them, at most: one innermost
/// head, one outer head (only when it names something the path does not),
/// one doc line under the innermost head, one contiguous closing line, and
/// the lines of any gap of three or fewer.
pub(crate) fn compute(lines: &[&str], path: &str, start: u32, end: u32) -> Vec<UnitRow> {
    if lines.is_empty() || start == 0 || start as usize > lines.len() {
        return Vec::new();
    }
    let mut ws = (start - 1) as usize;
    let mut we = ((end.max(start) - 1) as usize).min(lines.len() - 1);

    // Snap (§34 rule 1): a window must not open on a line that only closes
    // things it does not show, nor end on a line that only opens things it
    // does not show. Blank edges peel with them.
    while ws < we && (blank(lines[ws]) || is_closer_only(lines[ws])) {
        ws += 1;
    }
    while we > ws && (blank(lines[we]) || is_opener_only(lines[we])) {
        we -= 1;
    }

    // Truncate at the unit's visible end (§34.4): the fine window likes
    // landing on boundaries — a declaration line embeds strongly — so ~5% of
    // real windows arrive as [last statement, `}`, blank, next declaration].
    // A closer-only line shallower than the window's opening line says the
    // unit demonstrably ends there; everything after it belongs to the next
    // unit and misleads twice, dangling foreign rows below the match and
    // dragging the anchor so far down the head walk finds nothing.
    if let Some(open) = (ws..=we).find(|&i| !blank(lines[i])) {
        let open_ind = indent_of(lines[open]);
        if let Some(end) = (open + 1..=we)
            .find(|&i| is_closer_only(lines[i]) && indent_of(lines[i]) < open_ind)
        {
            we = end;
        }
    }

    // The anchor the head walk descends from: the shallowest *content* line.
    // Closer-only rows are excluded (§34.4) — a trailing `}` two levels out
    // would otherwise set the anchor and the walk would overshoot the
    // enclosing function to whatever sits above the whole block.
    let anchor = (ws..=we)
        .filter(|&i| !blank(lines[i]) && !is_closer_only(lines[i]))
        .map(|i| indent_of(lines[i]))
        .min()
        .or_else(|| (ws..=we).filter(|&i| !blank(lines[i])).map(|i| indent_of(lines[i])).min());
    let Some(anchor) = anchor else {
        // An all-blank window frames nothing; ship it bare rather than
        // decorate emptiness.
        return (ws..=we).map(|i| row(lines, i)).collect();
    };

    let mut chosen: BTreeSet<usize> = (ws..=we).collect();

    // Heads (§34 rules 2-4). The innermost is unconditional — it is the one
    // row the audit found doing nearly all the work. The outer must pay for
    // itself with a name the path prefix does not already carry, which is
    // the rule that keeps `class BaseModelForm` above a hit in models.py
    // and drops `module Cop` above a hit in cop/layout/foo.rb.
    let inner = find_head(lines, ws, anchor);
    if let Some(h) = inner {
        chosen.insert(h);
        if let Some(d) = doc_line_under(lines, h, ws) {
            chosen.insert(d);
        }
        if let Some(o) = find_head(lines, h, indent_of(lines[h]))
            && outer_head_is_informative(path, lines[o])
        {
            chosen.insert(o);
        }
    }

    // Close (§34 rule 5): only when it touches the window. A `}` right
    // after the match says "nothing else happens in this unit" — real
    // information. A close reached across an elision restates the header
    // span and nothing more, so it never prints.
    if we + 1 < lines.len() {
        let c = lines[we + 1];
        if is_closer_only(c) && indent_of(c) <= anchor {
            chosen.insert(we + 1);
        }
    }

    // Gap fill (§34 rule 6): a gap of up to three lines costs less to show
    // than to mark — an elision row is a row too — so short gaps arrive
    // whole and only real distance becomes a jump.
    let picked: Vec<usize> = chosen.iter().copied().collect();
    for pair in picked.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        if b - a > 1 && b - a - 1 <= 3 {
            chosen.extend(a + 1..b);
        }
    }

    chosen.into_iter().map(|i| row(lines, i)).collect()
}

fn row(lines: &[&str], i: usize) -> UnitRow {
    UnitRow { line: (i + 1) as u32, text: lines[i].to_string() }
}

fn blank(line: &str) -> bool {
    line.trim().is_empty()
}

/// Leading-whitespace width in columns, a tab counting as four. Columns and
/// not bytes because the comparison is *between* lines of one file, and a
/// file that mixes tabs and spaces (Go headers over space-indented bodies)
/// must not read as flat.
fn indent_of(line: &str) -> usize {
    let mut col = 0;
    for c in line.chars() {
        match c {
            ' ' => col += 1,
            '\t' => col += 4,
            _ => break,
        }
    }
    col
}

/// Non-blank, and nothing on it but closing punctuation — `}`, `),`, `];` —
/// or a bare `end`. The kind of line the §34 screenshots opened windows on.
fn is_closer_only(line: &str) -> bool {
    let t = line.trim();
    if t.is_empty() {
        return false;
    }
    if matches!(t, "end" | "end;" | "end,") {
        return true;
    }
    t.chars().all(|c| matches!(c, ')' | ']' | '}' | '>' | ',' | ';') || c.is_whitespace())
}

/// Non-blank, and nothing but opening punctuation (a trailing `:` or `,`
/// allowed): `(`, `{`, `[` on a line of their own.
fn is_opener_only(line: &str) -> bool {
    let t = line.trim();
    let core = t.trim_end_matches([':', ',']);
    !core.is_empty() && core.chars().all(|c| matches!(c, '(' | '[' | '{'))
}

/// Control flow is never a head: `} else {` above a window says nothing
/// about whose code it is, and the audit found four hits whose *only* added
/// row was such a line — worse than no head at all.
fn is_flow(trimmed: &str) -> bool {
    if trimmed.starts_with('}') {
        return true;
    }
    let first: String = trimmed
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    matches!(
        first.as_str(),
        "else"
            | "elif"
            | "elsif"
            | "except"
            | "finally"
            | "catch"
            | "case"
            | "default"
            | "then"
            | "begin"
            | "if"
            | "for"
            | "while"
            | "switch"
            | "match"
            | "loop"
            | "try"
            | "return"
            | "break"
            | "continue"
            | "unless"
            | "until"
            | "when"
    )
}

/// Does this line plausibly *enclose* the window? Three shapes count: a
/// block-introducing keyword (`def check(x)`, `module Cop`), a line ending
/// in an opener (`void f(int) {`, `describe("x", () => {`, `items.each do`),
/// or a modifier-led signature ending in `)` (Java's Allman
/// `public boolean hasNext()`). A *value* declaration — `let a = 1;` — is a
/// declaration to `declared_names` but declares no scope, and electing one
/// as a head frames the window with a random sibling statement; requiring
/// one of the three shapes is what rules it out. Flow lines never reach
/// here; `find_head` filters them first.
fn is_declaration(line: &str) -> bool {
    const BLOCK_KEYWORDS: [&str; 14] = [
        "class",
        "def",
        "enum",
        "fn",
        "func",
        "function",
        "impl",
        "interface",
        "module",
        "namespace",
        "object",
        "package",
        "struct",
        "trait",
    ];
    if line.split_whitespace().take(4).any(|w| BLOCK_KEYWORDS.contains(&w)) {
        return true;
    }
    let t = line.trim_end();
    if t.ends_with(['{', '(', '[', ':']) || t.ends_with(" do") || t.trim() == "do" {
        return true;
    }
    t.ends_with(')') && !text::declared_names(line).is_empty()
}

/// The nearest declaration above `below` at an indent strictly under
/// `max_indent`. Shallower lines that are not declarations — prose, the
/// raw content of a template literal sitting at column 0, a stray statement
/// — are walked *past*, which is what keeps a window inside a backtick
/// string from electing `END:VCALENDAR` as its head. A shallower line that
/// is the tail of a multi-line signature (`) {`) resolves to the
/// signature's first line instead of being taken at face value.
fn find_head(lines: &[&str], below: usize, max_indent: usize) -> Option<usize> {
    if max_indent == 0 {
        return None; // nothing can sit shallower than the margin
    }
    let mut i = below;
    while i > 0 {
        i -= 1;
        let l = lines[i];
        if blank(l) || indent_of(l) >= max_indent {
            continue;
        }
        let t = l.trim_start();
        // A comment is structure only for its own block (§34.4): when the
        // window sits inside a comment, the block's opening line completes
        // the sentence the window starts mid-list — the one head that
        // earns its row. A comment reached across code is never a head; a
        // `/* ... works:` two hundred lines up passed the shape checks
        // once and fabricated structure that was not there.
        if is_comment(l) {
            if (i + 1..below).all(|j| blank(lines[j]) || is_comment(lines[j])) {
                return Some(i);
            }
            continue;
        }
        if t.starts_with(')') || t.starts_with(']') {
            return Some(statement_start(lines, i));
        }
        if is_closer_only(l) || is_opener_only(l) || is_flow(t) {
            continue;
        }
        if is_declaration(l) {
            return Some(i);
        }
    }
    None
}

/// First line of the multi-line statement whose tail sits at `at`: the
/// nearest line above at the tail's indent or less that does not itself
/// start with a closer. Parameters sit deeper than the signature's first
/// and last lines, so the walk skips them by indent alone.
fn statement_start(lines: &[&str], at: usize) -> usize {
    let ind = indent_of(lines[at]);
    let mut i = at;
    while i > 0 {
        i -= 1;
        let l = lines[i];
        if blank(l) {
            break;
        }
        if indent_of(l) <= ind {
            let t = l.trim_start();
            if t.starts_with(')') || t.starts_with(']') || t.starts_with('}') {
                continue;
            }
            return i;
        }
    }
    at
}

/// An outer head earns its row only by adding a name the path does not
/// already carry. Compared squashed — lowercased, punctuation dropped — so
/// `TrailingEmptyLines` is found inside `trailing_empty_lines.rb` and
/// suppressed, while `BaseModelForm` is absent from `models.py` and kept.
/// A head whose name cannot be extracted keeps its row: redundancy must be
/// proven, not presumed.
fn outer_head_is_informative(path: &str, line: &str) -> bool {
    let names = text::declared_names(line);
    if names.is_empty() {
        return true;
    }
    let squash = |s: &str| -> String {
        s.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_ascii_lowercase()
    };
    let p = squash(path);
    names.iter().any(|n| {
        let n = squash(n);
        !n.is_empty() && !p.contains(&n)
    })
}

/// Does this line open or continue a comment? The `#` prefix also matches
/// Rust attributes and C preprocessor lines — acceptable on both call
/// sites: neither should head a window, and neither is a doc line worth
/// promoting.
fn is_comment(line: &str) -> bool {
    const DOC: [&str; 7] = ["#", "//", "/*", "*", "\"\"\"", "'''", "--"];
    let t = line.trim_start();
    DOC.iter().any(|p| t.starts_with(p))
}

/// The first doc or comment line directly under a head, when it sits above
/// the window: a docstring's opening sentence is the densest context line a
/// unit has. Attaches to the innermost head only — under a namespace line
/// it was the audit's "comment cut mid-sentence" noise.
fn doc_line_under(lines: &[&str], head: usize, window_start: usize) -> Option<usize> {
    let d = head + 1;
    if d >= window_start || d >= lines.len() {
        return None;
    }
    is_comment(lines[d]).then_some(d)
}

#[cfg(test)]
mod tests {
    use super::compute;

    fn nums(rows: &[super::UnitRow]) -> Vec<u32> {
        rows.iter().map(|r| r.line).collect()
    }

    #[test]
    fn leading_closers_and_trailing_openers_are_peeled_from_the_window() {
        let f = [
            "}",                  // 1
            "),",                 // 2
            "const config = [",   // 3
            "\"verbose\",",       // 4
            "(",                  // 5
        ];
        let rows = compute(&f, "src/config.js", 1, 5);
        assert_eq!(nums(&rows), vec![3, 4], "closers peel from the front, openers from the back");
    }

    #[test]
    fn a_multiline_signature_head_walks_to_its_statement_start() {
        let f = [
            "function diffElementNodes(", // 1
            "    dom,",                   // 2
            "    newVNode,",              // 3
            ") {",                        // 4
            "    let a = 1;",             // 5
            "    if (newHtml) {",         // 6
            "        stuff();",           // 7
        ];
        let rows = compute(&f, "src/diff/index.js", 7, 7);
        assert_eq!(rows[0].line, 1, "the head is the signature's first line, not its `) {{` tail");
        assert!(!nums(&rows).contains(&4), "the tail line itself is not a head");
    }

    #[test]
    fn a_namespace_head_already_named_in_the_path_is_suppressed() {
        let f = [
            "module Cop",                     // 1
            "  module Layout",                // 2
            "    class TrailingEmptyLines",   // 3
            "      def check(x)",             // 4
            "        autocorrect(x)",         // 5
            "      end",                      // 6
        ];
        let rows = compute(&f, "lib/rubocop/cop/layout/trailing_empty_lines.rb", 5, 5);
        let n = nums(&rows);
        assert!(n.contains(&4), "the innermost head is unconditional");
        assert!(
            !n.contains(&3) && !n.contains(&2) && !n.contains(&1),
            "every outer name is already in the path: {n:?}"
        );
    }

    #[test]
    fn an_outer_class_absent_from_the_path_is_kept() {
        let f = [
            "class BaseModelForm(BaseForm):",        // 1
            "    def _update_errors(self, errors):", // 2
            "        # Override any validation",     // 3
            "        for f in errors:",              // 4
        ];
        let rows = compute(&f, "django/forms/models.py", 4, 4);
        let n = nums(&rows);
        assert!(n.contains(&2), "innermost head");
        assert!(n.contains(&1), "models.py names no class, so the class line is new information");
    }

    #[test]
    fn the_head_walk_passes_through_a_template_literal() {
        let f = [
            "it('parses recurrence', () => {", // 1
            "    const ics = `",               // 2
            "BEGIN:VCALENDAR",                 // 3
            "RRULE:FREQ=DAILY",                // 4
            "DTSTART:20200101",                // 5
            "SUMMARY:x",                       // 6
            "END:VCALENDAR`;",                 // 7
            "    expect(parse(ics)).toBe(1);", // 8
        ];
        let rows = compute(&f, "test/calendar/vcal.spec.ts", 8, 8);
        assert_eq!(rows[0].line, 1, "column-0 string content is not a head; the it() line is");
        assert!(!nums(&rows).contains(&7), "template content stays out: {:?}", nums(&rows));
    }

    #[test]
    fn the_close_line_appears_only_when_contiguous() {
        let f = [
            "fn compute() {",  // 1
            "    let a = 1;",  // 2
            "    a + 1",       // 3
            "}",               // 4
            "fn other() {",    // 5
            "    let b = 2;",  // 6
            "    trace(b);",   // 7
            "    b * 2",       // 8
            "}",               // 9
        ];
        let touching = compute(&f, "src/lib.rs", 2, 3);
        assert!(nums(&touching).contains(&4), "a close touching the window is real information");
        let apart = compute(&f, "src/lib.rs", 6, 6);
        assert!(
            !nums(&apart).contains(&9),
            "a close across a gap restates the header span: {:?}",
            nums(&apart)
        );
    }

    #[test]
    fn small_gaps_fill_and_large_gaps_stay_jumps() {
        let f = [
            "def outer():",     // 1
            "    a = 1",        // 2
            "    b = 2",        // 3
            "    c = 3",        // 4
            "    d = 4",        // 5
            "    e = 5",        // 6
            "    f = 6",        // 7
            "    return f",     // 8
        ];
        // Head at 1, window at 5..6: gap of three (2,3,4) arrives whole.
        let filled = compute(&f, "pkg/mod.py", 5, 6);
        assert_eq!(nums(&filled), vec![1, 2, 3, 4, 5, 6], "a gap of three costs less shown");
        // Window at 7..8: gap of five stays a jump.
        let jumped = compute(&f, "pkg/mod.py", 7, 8);
        assert_eq!(nums(&jumped), vec![1, 7, 8], "a gap of five is an elision");
    }

    #[test]
    fn a_window_truncates_at_its_units_visible_end() {
        // The sendEncrypt.ts shape (§34.4): the fine window elects
        // [last statement, `};`, blank, next declaration]. The rows after
        // the shallow closer belong to a different unit, and the closer
        // used to drag the anchor to column 0 so no head was found at all.
        let f = [
            "const encryptBody = async (pack) => {",          // 1
            "    const encrypted = await encrypt(pack);",     // 2
            "    pack.Body = toBase64(encrypted);",           // 3
            "};",                                             // 4
            "",                                               // 5
            "const encryptPackage = async ({",                // 6
        ];
        let rows = compute(&f, "lib/mail/send/sendEncrypt.ts", 3, 6);
        let n = nums(&rows);
        assert!(!n.contains(&6), "the next unit's opener never dangles: {n:?}");
        assert!(n.contains(&4), "the unit's own close survives");
        assert!(n.contains(&1), "with the anchor undragged, the head resolves: {n:?}");
    }

    #[test]
    fn a_comment_across_code_is_never_a_head() {
        // The expire.c shape (§34.4): a `/* ... met:` comment far above the
        // window ends in a colon and passed the declaration shape checks,
        // fabricating structure. Code sits between it and the window, so it
        // heads nothing.
        let f = [
            "void active_cycle(int type) {",                       // 1
            "    int checked = 0;",                                // 2
            "    setup(type);",                                    // 3
            "    prime_counters();",                               // 4
            "    reset_clock();",                                  // 5
            "    /* Stop iteration when a condition is met:",      // 6
            "     * 1) enough databases were checked. */",         // 7
            "    while (running) {",                               // 8
            "        do_work();",                                  // 9
            "        checked++;",                                  // 10
        ];
        let rows = compute(&f, "src/expire.c", 10, 10);
        let n = nums(&rows);
        assert!(!n.contains(&6), "a comment across code heads nothing: {n:?}");
        assert!(n.contains(&1), "the walk continues to the real declaration: {n:?}");
    }

    #[test]
    fn a_comment_block_may_head_its_own_window() {
        // The aof.c shape (§34.4): the match is INSIDE a comment block, and
        // the block's opening line completes the sentence the window starts
        // mid-list. Every line between head and window is comment, so the
        // head stands.
        let f = [
            "/* This is how background rewrite works:",  // 1
            " *",                                        // 2
            " * 1) The user calls BGREWRITEAOF",         // 3
            " * 2) The server forks a child",            // 4
        ];
        let rows = compute(&f, "src/aof.c", 3, 4);
        assert_eq!(rows[0].line, 1, "the block's own opener heads the window: {:?}", nums(&rows));
    }

    #[test]
    fn the_first_doc_line_attaches_to_the_innermost_head_only() {
        let f = [
            "class Outer:",              // 1
            "    \"\"\"Outer doc.\"\"\"", // 2
            "    A = 1",                 // 3
            "    B = 2",                 // 4
            "    C = 3",                 // 5
            "    D = 4",                 // 6
            "    def inner(self):",      // 7
            "        \"\"\"Inner doc.\"\"\"", // 8
            "        x = 1",             // 9
        ];
        let rows = compute(&f, "pkg/other.py", 9, 9);
        let n = nums(&rows);
        assert!(n.contains(&8), "the innermost head's doc line is kept");
        assert!(n.contains(&1), "outer class is informative here");
        assert!(!n.contains(&2), "the outer head's doc line is noise: {n:?}");
    }
}
