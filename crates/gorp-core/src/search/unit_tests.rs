//! Tests for [`super::unit`]: the unit view's row selection.

use super::compute;

fn nums(rows: &[super::UnitRow]) -> Vec<u32> {
    rows.iter().map(|r| r.line).collect()
}

#[test]
fn leading_closers_and_trailing_openers_are_peeled_from_the_window() {
    let f = [
        "}",                // 1
        "),",               // 2
        "const config = [", // 3
        "\"verbose\",",     // 4
        "(",                // 5
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
        "module Cop",                   // 1
        "  module Layout",              // 2
        "    class TrailingEmptyLines", // 3
        "      def check(x)",           // 4
        "        autocorrect(x)",       // 5
        "      end",                    // 6
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
        "fn compute() {", // 1
        "    let a = 1;", // 2
        "    a + 1",      // 3
        "}",              // 4
        "fn other() {",   // 5
        "    let b = 2;", // 6
        "    trace(b);",  // 7
        "    b * 2",      // 8
        "}",              // 9
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
        "def outer():", // 1
        "    a = 1",    // 2
        "    b = 2",    // 3
        "    c = 3",    // 4
        "    d = 4",    // 5
        "    e = 5",    // 6
        "    f = 6",    // 7
        "    return f", // 8
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
        "const encryptBody = async (pack) => {",      // 1
        "    const encrypted = await encrypt(pack);", // 2
        "    pack.Body = toBase64(encrypted);",       // 3
        "};",                                         // 4
        "",                                           // 5
        "const encryptPackage = async ({",            // 6
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
        "void active_cycle(int type) {",                  // 1
        "    int checked = 0;",                           // 2
        "    setup(type);",                               // 3
        "    prime_counters();",                          // 4
        "    reset_clock();",                             // 5
        "    /* Stop iteration when a condition is met:", // 6
        "     * 1) enough databases were checked. */",    // 7
        "    while (running) {",                          // 8
        "        do_work();",                             // 9
        "        checked++;",                             // 10
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
        "/* This is how background rewrite works:", // 1
        " *",                                       // 2
        " * 1) The user calls BGREWRITEAOF",        // 3
        " * 2) The server forks a child",           // 4
    ];
    let rows = compute(&f, "src/aof.c", 3, 4);
    assert_eq!(rows[0].line, 1, "the block's own opener heads the window: {:?}", nums(&rows));
}

#[test]
fn a_dangling_comment_close_peels_like_any_closer() {
    // The preact renderComponent shape (§34.5 A): the fine window opens
    // on the `*/` of the doc block above the declaration it matched.
    let f = [
        "/**",                                   // 1
        " * Trigger in-place re-rendering.",     // 2
        " */",                                   // 3
        "function renderComponent(component) {", // 4
        "    let vnode = component._vnode;",     // 5
    ];
    let rows = compute(&f, "src/component.js", 3, 5);
    assert_eq!(rows[0].line, 4, "the `*/` peels; the block opens on the declaration");
}

#[test]
fn a_mid_block_window_walks_back_to_its_opener() {
    // The preact enqueueRender shape (§34.5 B): the window starts
    // mid-javadoc and contains the col-0 declaration, so anchor = 0 and
    // no head walk can reach the opener — the walk-back does.
    let f = [
        "/**",                                   // 1
        " * Enqueue a rerender of a component",  // 2
        " * @param c The component to rerender", // 3
        " */",                                   // 4
        "export function enqueueRender(c) {",    // 5
    ];
    let rows = compute(&f, "src/component.js", 2, 5);
    assert_eq!(rows[0].line, 1, "the block's opener joins the window: {:?}", nums(&rows));
}

#[test]
fn the_walk_back_is_capped_and_elides_the_middle() {
    // A long javadoc with the window at its tail: at most
    // BLOCK_LINES_CAP rows of the block's top arrive, the middle is a
    // jump the renderer marks, and one oversized line trips the
    // character cap.
    let long = "x".repeat(300);
    let long_row = format!(" * {long}");
    let f = [
        "/**",                     // 1
        " * Summary sentence.",    // 2
        " * Second sentence.",     // 3
        " * Detail one.",          // 4
        " * Detail two.",          // 5
        " * Detail three.",        // 6
        " * Detail four.",         // 7
        " * @param a first",       // 8
        " */",                     // 9
        "fn documented(a: u32) {", // 10
    ];
    let rows = compute(&f, "src/lib.rs", 8, 10);
    let n = nums(&rows);
    let prepended: Vec<u32> = n.iter().copied().filter(|&x| x < 8).collect();
    assert!(prepended.len() <= 3, "block top is capped: {n:?}");
    assert_eq!(prepended.first(), Some(&1), "the opener always arrives");
    assert!(!n.contains(&7), "the middle elides: {n:?}");

    // The char cap: an oversized second line stops the prepend after
    // the (exempt) opener.
    let g = [
        "/**",               // 1
        long_row.as_str(),   // 2
        " * @param a first", // 3
        " */",               // 4
        "fn documented() {", // 5
    ];
    let rows = compute(&g, "src/lib.rs", 3, 5);
    let n = nums(&rows);
    assert!(n.contains(&1), "the opener is exempt from the char cap");
    assert!(!n.contains(&2), "an oversized block line is not dragged in: {n:?}");
}

#[test]
fn a_redundant_namespace_never_heads_even_innermost() {
    // The fluentd shape (§34.5 C): a window directly at module scope
    // makes the namespace the innermost head, where the path-redundancy
    // check used not to run.
    let f = [
        "module Fluent",                             // 1
        "  NULL_CHAIN = NullOutputChain.instance",   // 2
        "  LIMIT = ::Fluent::Buffer::OverflowError", // 3
    ];
    let redundant = compute(&f, "fluent/event_router.rb", 2, 3);
    assert!(
        !nums(&redundant).contains(&1),
        "the path already says fluent: {:?}",
        nums(&redundant)
    );
    let informative = compute(&f, "lib/pipeline.rb", 2, 3);
    assert!(
        nums(&informative).contains(&1),
        "a namespace the path does not carry still heads: {:?}",
        nums(&informative)
    );
}

#[test]
fn the_first_doc_line_attaches_to_the_innermost_head_only() {
    let f = [
        "class Outer:",                   // 1
        "    \"\"\"Outer doc.\"\"\"",     // 2
        "    A = 1",                      // 3
        "    B = 2",                      // 4
        "    C = 3",                      // 5
        "    D = 4",                      // 6
        "    def inner(self):",           // 7
        "        \"\"\"Inner doc.\"\"\"", // 8
        "        x = 1",                  // 9
    ];
    let rows = compute(&f, "pkg/other.py", 9, 9);
    let n = nums(&rows);
    assert!(n.contains(&8), "the innermost head's doc line is kept");
    assert!(n.contains(&1), "outer class is informative here");
    assert!(!n.contains(&2), "the outer head's doc line is noise: {n:?}");
}
