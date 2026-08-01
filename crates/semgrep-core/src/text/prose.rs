//! Prose rendering for the embedding path (RESEARCH.md §14.2).
//!
//! ese is a prose model: its wordpiece pipeline shreds `scalar_None` into
//! `[scalar, _, none]`, gives `_` the same pooled mass as an identifier, and
//! leaves `computeBackoffDelay` as one out-of-vocabulary blob (§9.8). These
//! variants re-render code into the prose the model was trained on —
//! identifier subtokens, no punctuation — before it is embedded. BM25 and
//! keyword mode never see this: their tokenizer already does it.
//!
//! Chunks and queries must be rendered identically or they are not in one
//! vector space, which is why the choice is persisted in `meta.json` exactly
//! as `sif` is, and the warm path takes it from the index, never from a flag.

use crate::text::token;
use std::borrow::Cow;

/// How chunk and query text is rendered before it reaches the embedder.
/// An experiment lever (RESEARCH.md §14.3): harness use, isolated cache dirs —
/// like `--sif`, the variant is not part of the cache entry key.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum EmbedPreproc {
    /// Raw `doc_text` through ese's own pipeline (the shipped behavior).
    #[default]
    None,
    /// Code-aware subtokens, space-joined: `getUserName` → `get user name`.
    Split,
    /// `Split` plus the whole lowercased identifier where it decomposed:
    /// `getUserName` → `get user name getusername`.
    SplitWhole,
    /// `Split` minus language keywords and pure-number tokens.
    SplitNokw,
}

impl EmbedPreproc {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "none" => Some(Self::None),
            "split" => Some(Self::Split),
            "split-whole" => Some(Self::SplitWhole),
            "split-nokw" => Some(Self::SplitNokw),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Split => "split",
            Self::SplitWhole => "split-whole",
            Self::SplitNokw => "split-nokw",
        }
    }

    pub fn is_none(self) -> bool {
        self == Self::None
    }
}

/// Render `text` for embedding. `None` is the identity and allocates nothing;
/// every other variant emits the code-aware token stream, space-joined, in
/// corpus order — so token frequency (which mean pooling weights by) survives,
/// only the noise is gone.
pub fn render(text: &str, p: EmbedPreproc) -> Cow<'_, str> {
    if p.is_none() {
        return Cow::Borrowed(text);
    }
    let whole_idents = p == EmbedPreproc::SplitWhole;
    let mut out = String::with_capacity(text.len());
    token::for_each_token_with(text, whole_idents, |tok| {
        if p == EmbedPreproc::SplitNokw && (is_keyword(tok) || is_number(tok)) {
            return;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(tok);
    });
    Cow::Owned(out)
}

fn is_number(tok: &str) -> bool {
    tok.chars().all(|c| c.is_ascii_digit())
}

/// Language keywords across the corpus languages this project measures
/// (C, Rust, TS/JS, Python, Java, Go, Ruby). Lowercased, sorted — tokens
/// arriving here already are. Type names double as English words (`string`,
/// `float`) stay: dropping them costs prose queries more than it saves chunks.
const KEYWORDS: &[&str] = &[
    "async", "await", "begin", "break", "case", "catch", "class", "const", "continue", "def",
    "default", "defer", "elif", "else", "elsif", "end", "enum", "except", "extends", "final",
    "finally", "fn", "for", "func", "goto", "if", "impl", "implements", "import", "include",
    "instanceof", "interface", "lambda", "let", "match", "mod", "module", "mut", "namespace",
    "new", "nil", "package", "pass", "private", "protected", "pub", "public", "raise", "require",
    "return", "self", "sizeof", "static", "struct", "super", "switch", "then", "this", "throw",
    "throws", "trait", "try", "typedef", "typeof", "unless", "unsafe", "until", "use", "var",
    "void", "while", "yield",
];

fn is_keyword(tok: &str) -> bool {
    KEYWORDS.binary_search(&tok).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_is_the_identity_and_borrows() {
        let s = "fn get_user_name(&self) -> String";
        assert!(matches!(render(s, EmbedPreproc::None), Cow::Borrowed(_)));
    }

    #[test]
    fn split_renders_identifiers_as_prose() {
        let s = "fn get_user_name(&self) { retry_backoff += 1; }";
        assert_eq!(render(s, EmbedPreproc::Split), "fn get user name self retry backoff");
    }

    #[test]
    fn split_whole_keeps_the_identifier_too() {
        assert_eq!(
            render("getUserName", EmbedPreproc::SplitWhole),
            "get user name getusername"
        );
        // Undecomposed words carry no duplicate.
        assert_eq!(render("backoff", EmbedPreproc::SplitWhole), "backoff");
    }

    #[test]
    fn nokw_drops_keywords_and_numbers_but_not_content() {
        let s = "def compute_delay(retries): return delay * 250";
        assert_eq!(render(s, EmbedPreproc::SplitNokw), "compute delay retries delay");
    }

    #[test]
    fn keyword_table_is_sorted_for_binary_search() {
        assert!(KEYWORDS.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn punctuation_noise_never_survives() {
        // §9.8: `_` matched anything with cosine 1.0. It must not exist here.
        let r = render("a[_x] = {_y: *_z}; // _", EmbedPreproc::Split);
        assert!(!r.contains('_'), "{r:?}");
        assert!(!r.contains('['));
    }
}
