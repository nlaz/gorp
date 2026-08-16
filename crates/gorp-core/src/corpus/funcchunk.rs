//! Function-boundary chunking: one chunk per definition, with its leading
//! comment attached (RESEARCH.md §11, §29).
//!
//! §11.1 measured the defect this addresses: a 32-line window rarely cuts a
//! function in half (11%) but swallows ~3 whole functions, so the vector for
//! the chunk pools the target with its neighbours. Cutting at definition
//! boundaries gives each function its own vector and makes the returned span
//! a meaningful unit with true edges.
//!
//! The walk needs one table per language — the node kinds that ARE a
//! definition worth emitting (`leaf_defs`). Everything else recurses, which
//! handles containers (classes, impls, modules → chunk per method, the header
//! and fields falling to gap cuts), wrapper nodes (`export_statement`,
//! `decorated_definition` — decorators re-attach via Rule B's `@` prefix),
//! and over-cap definitions (recursing finds the smaller definitions inside;
//! the uncovered remainder falls to gap cuts aligned at the definition's own
//! start, so the head cut carries the signature and its doc). A definition at
//! or under the cap is emitted whole and NOT recursed — a closure out of
//! context is noise, and it stays inside its parent.
//!
//! Determinism is the load-bearing property: the same bytes must cut the same
//! way from the build pass, the cold pass, and read-repair, or
//! `cold_and_warm_return_identical_results` breaks. Everything here is a pure
//! function of the file text; there is deliberately NO parser timeout — a
//! timeout would make the cut a function of machine load. A parse that fails
//! or produces any ERROR node returns `None` and the caller falls back to
//! line windows, so an errored tree's untrustworthy boundaries never reach an
//! index.

use tree_sitter::{Language, Node, Parser};

/// Max chunk span in lines: [`crate::FUNC_CAP_DEFAULT`], re-exported where
/// the tests use it. §11.1: 89% of functions are ≤32 lines weighted by count;
/// ≤96 covers the distribution's tail, and anything longer is too big to pool
/// into one useful vector anyway — it recurses and gap-cuts.
#[cfg(test)]
const CAP_DEFAULT: u32 = crate::FUNC_CAP_DEFAULT;

/// A definition shorter than this merges forward into the next *contiguous*
/// definition (never across a gap — §11.2's rule A failure was pulling
/// unrelated code). §11.1 medians run 3–12 lines; without a floor, packed
/// Java/Ruby accessors explode the chunk count (+76% on django).
const MIN_LINES: usize = 5;

/// Rule B (§11.2): leading comments attached by a shared prefix table, at
/// most this many lines, crossing at most one blank. Measured against the
/// walk-to-blank-line alternative: 0% wrongly-pulled code in all seven
/// languages, against up to 55%.
const COMMENT_MAX: usize = 20;
const COMMENT_PREFIXES: &[&str] = &["///", "//!", "//", "/*", "*/", "*", "#", "@"];

pub(crate) struct Lang {
    pub(crate) ts: fn() -> Language,
    leaf_defs: &'static [&'static str],
}

/// Extension → grammar. `.h` parses as C — a v1 limitation shared with the
/// §11.3 implementation; C++ headers fall back on parse errors anyway.
/// `pub(crate)` for `corpus::imports`, which shares the dispatch rather than
/// growing a second copy of it.
pub(crate) fn lang_for_path(rel_path: &str) -> Option<Lang> {
    let ext = rel_path.rsplit('.').next()?;
    let lang = match ext {
        "py" => Lang {
            ts: || tree_sitter_python::LANGUAGE.into(),
            leaf_defs: &["function_definition"],
        },
        "js" | "mjs" | "cjs" | "jsx" => Lang {
            ts: || tree_sitter_javascript::LANGUAGE.into(),
            leaf_defs: &[
                "function_declaration",
                "generator_function_declaration",
                "method_definition",
            ],
        },
        "ts" | "mts" | "cts" => Lang {
            ts: || tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            leaf_defs: &[
                "function_declaration",
                "generator_function_declaration",
                "method_definition",
                "interface_declaration",
                "enum_declaration",
            ],
        },
        "tsx" => Lang {
            ts: || tree_sitter_typescript::LANGUAGE_TSX.into(),
            leaf_defs: &[
                "function_declaration",
                "generator_function_declaration",
                "method_definition",
                "interface_declaration",
                "enum_declaration",
            ],
        },
        "rs" => Lang {
            ts: || tree_sitter_rust::LANGUAGE.into(),
            leaf_defs: &["function_item", "struct_item", "enum_item", "trait_item"],
        },
        "go" => Lang {
            ts: || tree_sitter_go::LANGUAGE.into(),
            leaf_defs: &["function_declaration", "method_declaration", "type_declaration"],
        },
        "c" | "h" => Lang {
            ts: || tree_sitter_c::LANGUAGE.into(),
            leaf_defs: &["function_definition"],
        },
        "java" => Lang {
            ts: || tree_sitter_java::LANGUAGE.into(),
            leaf_defs: &[
                "method_declaration",
                "constructor_declaration",
                "interface_declaration",
                "enum_declaration",
            ],
        },
        "rb" => Lang {
            ts: || tree_sitter_ruby::LANGUAGE.into(),
            leaf_defs: &["method", "singleton_method"],
        },
        "php" => Lang {
            ts: || tree_sitter_php::LANGUAGE_PHP.into(),
            leaf_defs: &["function_definition", "method_declaration"],
        },
        _ => return None,
    };
    Some(lang)
}

/// Cut `text` at definition boundaries. Returns 0-based `[start, end)` line
/// ranges, sorted, disjoint, jointly covering `[0, n_lines)`. `None` means
/// the caller should fall back to line windows: unsupported extension,
/// parse failure, or an ERROR anywhere in the tree.
pub(crate) fn cut(
    rel_path: &str,
    text: &str,
    n_lines: usize,
    cap: u32,
    window: u32,
) -> Option<Vec<(usize, usize)>> {
    let lang = lang_for_path(rel_path)?;
    let mut parser = Parser::new();
    parser.set_language(&(lang.ts)()).ok()?;
    let tree = parser.parse(text, None)?;
    let root = tree.root_node();
    if root.has_error() {
        return None;
    }

    // 1. Definitions at or under the cap, in document order. Ends clamp to
    //    the caller's line count — a node ending exactly at EOF without a
    //    trailing newline must not claim a line past it.
    let mut units: Vec<(usize, usize)> = Vec::new();
    collect(root, &lang, cap as usize, &mut units);
    units.sort_unstable();
    units.dedup();
    for u in units.iter_mut() {
        u.1 = u.1.min(n_lines);
    }
    units.retain(|&(s, e)| e > s);

    // 2. Rule B: extend each unit upward over its leading comment. The floor
    //    is the previous unit's end, so an attachment can never claim lines a
    //    definition owns.
    let lines: Vec<&str> = text.lines().collect();
    let mut floor = 0usize;
    for u in units.iter_mut() {
        u.0 = attach_leading_comment(&lines, u.0, floor);
        floor = u.1;
    }

    // 3. Min-merge: a tiny definition folds forward into the next one when
    //    they touch and the merge stays under the cap.
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(units.len());
    for u in units {
        if let Some(last) = merged.last_mut()
            && last.1 == u.0
            && (last.1 - last.0) < MIN_LINES
            && (u.1 - last.0) <= cap as usize
        {
            last.1 = u.1;
            continue;
        }
        merged.push(u);
    }

    // 4. Gaps — imports, constants, class headers and fields, over-cap
    //    interiors — fall to non-overlapping `window`-sized cuts. Disjoint by
    //    construction, which is where §11.3's BM25-postings shrink came from.
    let window = window.max(1) as usize;
    let mut out: Vec<(usize, usize)> = Vec::with_capacity(merged.len() * 2);
    let mut at = 0usize;
    for &(s, e) in &merged {
        gap_cuts(at, s, window, &mut out);
        out.push((s, e));
        at = e;
    }
    gap_cuts(at, n_lines, window, &mut out);
    Some(out)
}

fn gap_cuts(mut at: usize, until: usize, window: usize, out: &mut Vec<(usize, usize)>) {
    while at < until {
        let end = (at + window).min(until);
        out.push((at, end));
        at = end;
    }
}

/// Emit definitions at or under the cap; recurse into everything else.
fn collect(node: Node, lang: &Lang, cap: usize, units: &mut Vec<(usize, usize)>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        let start = child.start_position().row;
        let end = child.end_position().row + 1;
        if lang.leaf_defs.contains(&child.kind()) && end - start <= cap {
            units.push((start, end));
        } else {
            collect(child, lang, cap, units);
        }
    }
}

/// Rule B (§11.2): walk upward from `start` accepting comment-prefixed lines,
/// crossing at most one blank, taking at most [`COMMENT_MAX`] lines, never
/// below `floor`. Returns the new start. A pure line classifier rather than a
/// tree-sitter query so the same function can serve a fallback path later,
/// and because the measured rule needs nothing more than a prefix table.
fn attach_leading_comment(lines: &[&str], start: usize, floor: usize) -> usize {
    let mut new_start = start;
    let mut taken = 0usize;
    let mut gap = 0usize;
    let mut i = start;
    while i > floor && taken < COMMENT_MAX {
        let l = lines[i - 1].trim_start();
        if l.is_empty() {
            if gap >= 1 {
                break;
            }
            gap += 1;
            i -= 1;
            continue;
        }
        if COMMENT_PREFIXES.iter().any(|p| l.starts_with(p)) {
            i -= 1;
            new_start = i;
            taken += 1;
            gap = 0;
        } else {
            break;
        }
    }
    new_start
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spans(path: &str, text: &str) -> Vec<(usize, usize)> {
        let n = text.lines().count();
        cut(path, text, n, CAP_DEFAULT, 32).expect("parseable fixture")
    }

    #[test]
    fn python_functions_cut_at_their_boundaries() {
        let src = "import os\n\n\n# Compute the thing.\n# Carefully.\ndef alpha(x):\n    a = 1\n    b = 2\n    c = 3\n    return a + b + c\n\n\ndef beta(y):\n    d = 4\n    e = 5\n    f = 6\n    return d + e + f\n";
        let s = spans("m.py", src);
        // alpha owns its comment (lines 3..10 0-based); beta is its own unit.
        assert!(s.contains(&(3, 10)), "alpha + its comment: {s:?}");
        assert!(s.contains(&(12, 17)), "beta: {s:?}");
        covered(src, &s);
    }

    #[test]
    fn a_class_chunks_per_method_not_as_one_blob() {
        let src = "class Big:\n    field = 1\n\n    def one(self):\n        a = 1\n        b = 2\n        c = 3\n        return a\n\n    def two(self):\n        d = 4\n        e = 5\n        f = 6\n        return d\n";
        let s = spans("m.py", src);
        assert!(s.contains(&(3, 8)), "method one is a unit: {s:?}");
        assert!(s.contains(&(9, 14)), "method two is a unit: {s:?}");
        covered(src, &s);
    }

    #[test]
    fn decorators_reattach_via_the_at_prefix() {
        let src = "import x\n\n@route(\"/api\")\n@cached\ndef handler(req):\n    a = 1\n    b = 2\n    c = 3\n    return a\n";
        let s = spans("m.py", src);
        // decorated_definition is transparent; Rule B pulls the two @ lines.
        assert!(s.contains(&(2, 9)), "decorators attach to the def: {s:?}");
    }

    #[test]
    fn rule_b_crosses_one_blank_but_not_two() {
        let src = "# far away\n\n\n# near\ndef alpha(x):\n    a = 1\n    b = 2\n    c = 3\n    return a\n";
        let s = spans("m.py", src);
        // One blank crossed reaches "# near"; the second blank stops the walk
        // before "# far away".
        assert!(s.contains(&(3, 9)), "only the near comment attaches: {s:?}");
    }

    #[test]
    fn tiny_contiguous_defs_merge_forward() {
        // Rust getters, 3 lines each, back to back.
        let src = "fn a() -> u32 {\n    1\n}\nfn b() -> u32 {\n    2\n}\nfn c() -> u32 {\n    3\n}\n";
        let s = spans("m.rs", src);
        assert!(s.len() < 3, "packed accessors merge: {s:?}");
        covered(src, &s);
    }

    #[test]
    fn an_over_cap_function_recurses_and_gap_cuts() {
        let mut src = String::from("fn huge() {\n");
        for i in 0..CAP_DEFAULT + 20 {
            src.push_str(&format!("    let x{i} = {i};\n"));
        }
        src.push_str("}\n");
        let s = spans("m.rs", &src);
        assert!(s.len() > 1, "an over-cap def must split: {s:?}");
        assert!(s.iter().all(|(a, b)| b - a <= CAP_DEFAULT as usize));
        assert_eq!(s[0].0, 0, "the head cut starts at the signature");
        covered(&src, &s);
    }

    #[test]
    fn unsupported_and_unparseable_fall_back() {
        assert!(cut("notes.md", "# heading\ntext\n", 2, CAP_DEFAULT, 32).is_none());
        let broken = "def alpha(:\n    ???\n";
        assert!(cut("m.py", broken, 2, CAP_DEFAULT, 32).is_none(), "error tree falls back");
    }

    #[test]
    fn the_cut_is_deterministic() {
        // A mixed fixture: comments, a struct, methods, a gap, an over-cap fn.
        let mut src = String::from(
            "use std::io;\n\n/// A thing.\nstruct T { x: u32 }\n\nimpl T {\n    /// Get.\n    fn get(&self) -> u32 {\n        let a = 1;\n        let b = 2;\n        self.x + a + b\n    }\n}\n\n",
        );
        src.push_str("fn huge() {\n");
        for i in 0..CAP_DEFAULT + 5 {
            src.push_str(&format!("    let x{i} = {i};\n"));
        }
        src.push_str("}\n");
        let n = src.lines().count();
        let a = cut("m.rs", &src, n, CAP_DEFAULT, 32);
        assert!(a.is_some());
        for _ in 0..20 {
            assert_eq!(a, cut("m.rs", &src, n, CAP_DEFAULT, 32));
        }
    }

    /// Every line is in exactly one range, in order — the disjoint-coverage
    /// invariant the whole design leans on.
    fn covered(text: &str, spans: &[(usize, usize)]) {
        let n = text.lines().count();
        let mut at = 0;
        for &(s, e) in spans {
            assert_eq!(s, at, "ranges must tile without gap or overlap: {spans:?}");
            assert!(e > s);
            at = e;
        }
        assert_eq!(at, n, "ranges must cover the whole file: {spans:?}");
    }
}
