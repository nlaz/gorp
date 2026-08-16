//! The frozen vocabulary tables the prose renderer prunes against, and the
//! binary-search predicates over them.
//!
//! Separated from the renderer because these lists are *data with a
//! provenance*: each was measured against a published condition, and editing
//! one silently re-defines the arm it was measured in. Every table here is
//! sorted — the predicates binary-search — and every one is frozen unless a
//! new arm says otherwise.

/// Language keywords across the corpus languages this project measures
/// (C, Rust, TS/JS, Python, Java, Go, Ruby). Lowercased, sorted — tokens
/// arriving here already are. Type names double as English words (`string`,
/// `float`) stay: dropping them costs prose queries more than it saves chunks.
///
/// **Frozen.** `SplitNokw` was measured against exactly this list in §14.4;
/// what it turned out to be missing lives in [`KEYWORDS_EXTRA`] so the repair
/// is a separate, attributable arm rather than a silent edit to a published
/// condition.
pub(super) const KEYWORDS: &[&str] = &[
    "async", "await", "begin", "break", "case", "catch", "class", "const", "continue", "def",
    "default", "defer", "elif", "else", "elsif", "end", "enum", "except", "extends", "final",
    "finally", "fn", "for", "func", "goto", "if", "impl", "implements", "import", "include",
    "instanceof", "interface", "lambda", "let", "match", "mod", "module", "mut", "namespace",
    "new", "nil", "package", "pass", "private", "protected", "pub", "public", "raise", "require",
    "return", "self", "sizeof", "static", "struct", "super", "switch", "then", "this", "throw",
    "throws", "trait", "try", "typedef", "typeof", "unless", "unsafe", "until", "use", "var",
    "void", "while", "yield",
];

/// What [`KEYWORDS`] was missing. `function` and `export` are the conspicuous
/// two — they survived every §14 condition, so the most common tokens in the
/// TS corpus were being embedded as content. Sorted, and disjoint from
/// `KEYWORDS`; both are asserted.
pub(super) const KEYWORDS_EXTRA: &[&str] = &[
    "abstract", "and", "as", "assert", "auto", "chan", "constexpr", "declare", "define",
    "deinit", "del", "delete", "do", "elseif", "endif", "explicit", "export", "extern",
    "false", "friend", "from", "function", "global", "go", "ifdef", "ifndef", "in", "init",
    "inline", "is", "lateinit", "long", "nonlocal", "not", "null", "object", "of", "operator",
    "or", "override", "pragma", "readonly", "register", "short", "signed", "suspend",
    "template", "true", "type", "typename", "undefined", "union", "unsigned", "val", "virtual",
    "volatile", "when", "where", "with"
];

/// Low-signal code vocabulary: builtin namespaces, primitive and annotation
/// type names, unit suffixes, and throwaway variable names. Sorted.
///
/// This is the tier that is a judgement rather than a fact, and it is the one
/// most likely to overlap with what `--sif` already does — rarity weighting
/// demotes exactly the tokens that are common corpus-wide. §20 crosses the
/// tiers with SIF on and off for that reason: if the stoplist only reproduces
/// what SIF learns, it is redundant, and hand-maintaining a word list is a
/// cost with no return.
pub(super) const LOW_SIGNAL: &[&str] = &[
    "arg", "args", "argv", "array", "bar", "baz", "bool", "boolean", "byte", "bytes", "char",
    "console", "data", "dict", "double", "echo", "elem", "element", "err", "f32", "f64",
    "float", "fmt", "foo", "hours", "i16", "i32", "i64", "idx", "index", "int", "integer",
    "isize", "item", "items", "list", "log", "map", "math", "millis", "minutes", "ms", "msec",
    "msecs", "nsec", "number", "obj", "ok", "option", "options", "opts", "param", "params",
    "print", "printf", "println", "puts", "qux", "result", "sec", "seconds", "secs", "set",
    "short", "some", "str", "string", "temp", "tmp", "u16", "u32", "u64", "uint", "usec",
    "usize", "val", "value", "vec"
];

/// Keywords that introduce a name. A superset of the declaring subset of
/// [`KEYWORDS`]/[`KEYWORDS_EXTRA`] — `export` and `public` declare in the
/// sense that matters here (the next identifier is being defined). Sorted.
/// Words that sit between a declarer and the name it introduces, and are never
/// the name themselves. Sorted — `binary_search`.
///
/// Separate from [`DECLARERS`] on purpose: that table feeds
/// `declaration_sites`, which drives the measured `PruneDecl` renderings, so a
/// word added there would change numbers §20–§24 already published. This one is
/// display-only ([`declared_names`], RESEARCH.md §25.1).
pub(super) const MODIFIERS: &[&str] = &[
    "async", "extern", "final", "inline", "mut", "override", "sealed", "synchronized", "unsafe",
    "virtual",
];

pub(super) const DECLARERS: &[&str] = &[
    "abstract", "class", "const", "def", "enum", "export", "fn", "func", "function", "impl",
    "interface", "let", "module", "namespace", "object", "package", "private", "protected", "pub",
    "public", "readonly", "static", "struct", "trait", "type", "val", "var",
];

pub(super) fn is_keyword(tok: &str) -> bool {
    KEYWORDS.binary_search(&tok).is_ok()
}

pub(super) fn is_keyword_extra(tok: &str) -> bool {
    KEYWORDS_EXTRA.binary_search(&tok).is_ok()
}

pub(super) fn is_low_signal(tok: &str) -> bool {
    LOW_SIGNAL.binary_search(&tok).is_ok()
}

/// Case-insensitive because a declarer is matched against the raw word, before
/// the subtoken lowercasing that the rest of the pipeline does.
pub(super) fn is_declarer(word: &str) -> bool {
    let lower = word.to_ascii_lowercase();
    DECLARERS.binary_search(&lower.as_str()).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every table here is binary-searched, so sortedness is correctness and
    /// not style — an unsorted entry silently stops matching.
    #[test]
    fn tables_are_sorted_for_binary_search() {
        for t in [KEYWORDS, KEYWORDS_EXTRA, LOW_SIGNAL, DECLARERS, MODIFIERS] {
            assert!(t.windows(2).all(|w| w[0] < w[1]), "unsorted: {:?}", t[0]);
        }
    }

    #[test]
    fn the_frozen_table_and_its_repair_are_disjoint() {
        // Otherwise `prune-kw` would be doing the same work twice and the
        // "what was missing" claim in the docs would be wrong.
        for k in KEYWORDS_EXTRA {
            assert!(!is_keyword(k), "{k} is already in the frozen table");
        }
    }
}
