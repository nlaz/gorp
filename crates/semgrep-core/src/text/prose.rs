//! Prose rendering for the embedding path (RESEARCH.md §14.2, §20).
//!
//! ese is a prose model: its wordpiece pipeline shreds `scalar_None` into
//! `[scalar, _, none]`, gives `_` the same pooled mass as an identifier, and
//! leaves `computeBackoffDelay` as one out-of-vocabulary blob (§9.8). These
//! variants re-render code into the prose the model was trained on —
//! identifier subtokens, no punctuation — before it is embedded. BM25 and
//! keyword mode never see this: their tokenizer already does it.
//!
//! §20 extends that from *normalizing* the token stream to *pruning* it. Under
//! uniform mean pooling every surviving token gets an equal share, so dropping
//! one is not merely removing noise — it is handing that mass to the tokens
//! that remain. The ladder below is ordered by how much it assumes:
//! keywords are boilerplate everywhere, the low-signal table is a judgement
//! about code vocabulary, and declaration-position is a claim about which
//! *occurrence* of a name carries the meaning.
//!
//! Chunks and queries must be rendered into one vector space, which is why the
//! choice is persisted in `meta.json` exactly as `sif` is, and the warm path
//! takes it from the index, never from a flag.
//!
//! That constraint is stronger than it first looks, and §20.6 is where this
//! module had it wrong. It binds the *vocabulary*, not just the token→vector
//! mapping. Ranking is cosine against a fixed query, so `|q|` cancels and the
//! score decomposes additively over query tokens:
//!
//! ```text
//! score(d)  ∝  <C_q, d> + <K_q, d>       C = content tokens, K = pruned class
//! ```
//!
//! Prune neither side and `<K_q, d>` is a weak matching term. Prune both and
//! it vanishes. Prune **documents only** and `K_q` survives in the query while
//! every document has lost its counterpart: word vectors are not orthogonal,
//! so the term stays non-zero and still varies by document — an additive error
//! with nothing to align against. Measured, that costs −0.040 on tokio and is
//! negative on all four corpora (§20.6).
//!
//! So the rule is **mirror what a query can mirror**. The keyword table and
//! the low-signal table both apply to queries. Declaration position cannot —
//! prose has no declaration sites — which makes `PruneDecl` structurally
//! asymmetric and is the leading explanation for why it loses hardest (§20.7).

use crate::text::token;
use std::borrow::Cow;

/// How chunk and query text is rendered before it reaches the embedder.
/// An experiment lever (RESEARCH.md §14.3, §20): harness use, isolated cache
/// dirs — like `--sif`, the variant is not part of the cache entry key.
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
    ///
    /// Frozen against [`KEYWORDS`], which is missing `function` and `export`
    /// among others. Kept exactly as measured so §14.4's numbers stay
    /// comparable; [`PruneKw`](Self::PruneKw) is the same idea with the table
    /// repaired.
    SplitNokw,
    /// T1′: `SplitNokw` over the repaired keyword table ([`KEYWORDS_EXTRA`]).
    PruneKw,
    /// T2: `PruneKw` minus low-signal code vocabulary — builtin namespaces,
    /// primitive and annotation type names, unit suffixes, throwaway variable
    /// names ([`LOW_SIGNAL`]).
    PruneLex,
    /// T3: `PruneLex` keeping only declaration-position identifiers — the
    /// declared name, its parameters, assignment left-hand sides. Every
    /// reference is dropped.
    ///
    /// The aggressive end of the ladder, and the one with a predictable
    /// failure mode: it deletes the tokens a "where is this called" query
    /// would match, which is a real share of the agentic regime (§16).
    PruneDecl,
    /// T5: `PruneLex` with declaration-position tokens emitted twice and
    /// references once. The same belief as `PruneDecl` expressed as weight
    /// rather than deletion — emission count *is* weight under mean pooling.
    PruneSoft,
    /// T4: `PruneLex` with each distinct token emitted once. Turns the chunk
    /// into a set of words, which is the strongest possible statement that
    /// repetition inside a chunk carries no signal.
    PruneUniq,
    /// `PruneLex` with the low-signal table applied to queries as well
    /// (RESEARCH.md §20.7).
    ///
    /// The arm that discriminates §20.5's prediction 2 from the §20.6
    /// mechanism. `PruneLex` was specified document-side on the reasoning that
    /// a query cannot mirror it — but unlike declaration position, it can: a
    /// query says `number` and `string` as readily as a chunk does. If
    /// mirroring recovers the loss, the stoplist was never the problem.
    PruneLexSym,
    /// `PruneKw` with the keyword table fired **positionally**: a word is
    /// dropped only when the subtoken is the entire `[A-Za-z0-9_]+` run it came
    /// from (RESEARCH.md §22.1).
    ///
    /// `PruneKw`'s table deletes tokens that are *identifier components* in a
    /// real corpus, not just syntactic boilerplate. Measured against the 421
    /// gold function names agents were hunting in §21: the naive rule damages
    /// **20.9%** of them, the positional rule **0.7%**. `__init__` alone is 30
    /// of the 88 — when an agent searches `__init__`, `PruneKw` deletes `init`
    /// from the query and from every chunk, so the function becomes unfindable
    /// by the name it has. `def foo()` still loses `def`.
    PruneKwPos,
    /// `PruneKwPos` on documents, with the query left **untouched**
    /// (RESEARCH.md §22.1).
    ///
    /// The other half of §22's 2x2. §20.6 established that pruning documents
    /// but not queries costs recall, and the cost scales with the unmirrored
    /// share — but that was measured on generated queries, and §21.2 showed
    /// that instrument mispredicts the agent regime. Against it: chunk
    /// boilerplate is *obligatory* (the grammar forces `def` into every
    /// function) while a query token is *elective* — the agent spent one of
    /// its ~5 tokens on it. Under positional pruning the disputed share is
    /// 9.1% of query tokens either way; this arm keeps them.
    PruneKwPosQ0,
    /// `PruneUniq` mirrored: low-signal table and dedupe both applied to the
    /// query (RESEARCH.md §20.7).
    ///
    /// Weaker prior than `PruneLexSym`. Dedupe removes *repetitions*, not a
    /// token class, so the query never retains a vocabulary the documents have
    /// lost and §20.6's noise term should not arise. Run because it is nearly
    /// free and because "should not arise" is a prediction, not a measurement.
    PruneUniqSym,
}

/// How the relative path — line 1 of [`crate::corpus::doc_text`] — is rendered.
///
/// Orthogonal to the tier, and it has to be, because pruning the body raises
/// the path's share of the pooled mean mechanically: at `PruneDecl` the
/// example chunk in §20.1 is 11 path tokens to 5 body tokens. Left alone,
/// every window in a long file converges toward the same vector and
/// within-file discrimination dies exactly where a file has the most chunks.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum PathRender {
    /// Every path subtoken, in order, repeats and all (the shipped behavior).
    #[default]
    Full,
    /// Each distinct path subtoken once. `searchEditor/searchEditorActions.ts`
    /// stops saying "search editor" twice.
    Dedupe,
    /// The last two segments only, deduped — parent directory and filename.
    /// Drops the prefix every file in the tree shares.
    Tail,
    /// Deduped, and capped at [`PATH_SHARE`] of the body's token count so the
    /// path's share of the pooled mean stays put as the body shrinks. Taken
    /// from the tail, which is the most specific end.
    Scaled,
}

/// Path tokens per body token under [`PathRender::Scaled`].
pub const PATH_SHARE: f32 = 0.25;

impl EmbedPreproc {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "none" => Some(Self::None),
            "split" => Some(Self::Split),
            "split-whole" => Some(Self::SplitWhole),
            "split-nokw" => Some(Self::SplitNokw),
            "prune-kw" => Some(Self::PruneKw),
            "prune-lex" => Some(Self::PruneLex),
            "prune-decl" => Some(Self::PruneDecl),
            "prune-soft" => Some(Self::PruneSoft),
            "prune-uniq" => Some(Self::PruneUniq),
            "prune-kw-pos" => Some(Self::PruneKwPos),
            "prune-kw-pos-q0" => Some(Self::PruneKwPosQ0),
            "prune-lex-sym" => Some(Self::PruneLexSym),
            "prune-uniq-sym" => Some(Self::PruneUniqSym),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Split => "split",
            Self::SplitWhole => "split-whole",
            Self::SplitNokw => "split-nokw",
            Self::PruneKw => "prune-kw",
            Self::PruneLex => "prune-lex",
            Self::PruneDecl => "prune-decl",
            Self::PruneSoft => "prune-soft",
            Self::PruneUniq => "prune-uniq",
            Self::PruneKwPos => "prune-kw-pos",
            Self::PruneKwPosQ0 => "prune-kw-pos-q0",
            Self::PruneLexSym => "prune-lex-sym",
            Self::PruneUniqSym => "prune-uniq-sym",
        }
    }

    /// Every accepted spelling, for the CLI's error message and the harness.
    pub const ALL: &'static [&'static str] = &[
        "none",
        "split",
        "split-whole",
        "split-nokw",
        "prune-kw",
        "prune-lex",
        "prune-decl",
        "prune-soft",
        "prune-uniq",
        "prune-kw-pos",
        "prune-kw-pos-q0",
        "prune-lex-sym",
        "prune-uniq-sym",
    ];

    pub fn is_none(self) -> bool {
        self == Self::None
    }

    /// What this variant does, as a plan the renderer executes. Making the
    /// ladder a data table rather than a chain of `if`s is what keeps "T3 is
    /// T2 plus one thing" checkable — see `ladder_is_cumulative`.
    fn plan(self) -> Plan {
        let base = Plan {
            whole_idents: false,
            keywords: Keywords::None,
            positional: false,
            low_signal: false,
            decl: DeclMode::Off,
            dedupe: false,
        };
        match self {
            Self::None => base,
            Self::Split => base,
            Self::SplitWhole => Plan { whole_idents: true, ..base },
            Self::SplitNokw => Plan { keywords: Keywords::Legacy, ..base },
            Self::PruneKw => Plan { keywords: Keywords::Extended, ..base },
            Self::PruneLex => {
                Plan { keywords: Keywords::Extended, low_signal: true, ..base }
            }
            Self::PruneDecl => Plan {
                keywords: Keywords::Extended,
                low_signal: true,
                decl: DeclMode::Only,
                ..base
            },
            Self::PruneSoft => Plan {
                keywords: Keywords::Extended,
                low_signal: true,
                decl: DeclMode::Boost,
                ..base
            },
            Self::PruneUniq | Self::PruneUniqSym => Plan {
                keywords: Keywords::Extended,
                low_signal: true,
                dedupe: true,
                ..base
            },
            Self::PruneLexSym => {
                Plan { keywords: Keywords::Extended, low_signal: true, ..base }
            }
            Self::PruneKwPos | Self::PruneKwPosQ0 => {
                Plan { keywords: Keywords::Extended, positional: true, ..base }
            }
        }
    }

    /// The plan a *query* renders under: the tier's normalization, none of its
    /// pruning. See the module docs for why this is asymmetric.
    ///
    /// **Keyword pruning stays on both sides, and that is a measured result
    /// rather than a convenience (§20.6).** It looks indefensible: `in`, `is`,
    /// `of`, `as`, `from`, `not`, `with`, `and`, `or`, `type`, `object`, `map`
    /// are ordinary English, and the extended table removes **15.8% of all
    /// query tokens across 771 of CoSQA's 1,200 real human queries** —
    /// "python logging can not create file" loses its `not`. Removing that
    /// damage was tried, on all four corpora, and **lost on every one**:
    /// −0.040 tokio, −0.020 etcd, −0.010 vscode, and −0.003 to −0.014 on the
    /// CoSQA arms the change was designed to rescue.
    ///
    /// The reason is the part of the one-space constraint this module used to
    /// state too weakly. It is not only the token→vector mapping that must
    /// match across the two sides — it is the *vocabulary*. Strip a token
    /// class from documents and not from queries and the query vector points
    /// partly in a direction no document occupies; under a uniform mean that
    /// mass is simply lost. Matching the corpus beats keeping the word.
    ///
    /// So: pruning that a query can mirror (the keyword table) applies to
    /// both. Pruning it structurally cannot (declaration position, which prose
    /// has none of; the low-signal table, which would eat "parse a number from
    /// a string") stays document-side.
    fn query_plan(self) -> Plan {
        let p = self.plan();
        // Declaration position is never mirrorable: prose has no declaration
        // sites, so `DeclMode::Off` holds for every variant without exception.
        let (low_signal, dedupe) = match self {
            Self::PruneLexSym => (true, false),
            Self::PruneUniqSym => (true, true),
            _ => (false, false),
        };
        // The one variant that does not touch the query at all (§22.1 P3).
        let keywords =
            if self == Self::PruneKwPosQ0 { Keywords::None } else { p.keywords };
        Plan { keywords, low_signal, decl: DeclMode::Off, dedupe, ..p }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Plan {
    whole_idents: bool,
    keywords: Keywords,
    /// Fire the keyword table only on a subtoken that is the whole run.
    positional: bool,
    low_signal: bool,
    decl: DeclMode,
    dedupe: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Keywords {
    None,
    /// The frozen §14 table.
    Legacy,
    /// The frozen table plus what it was missing.
    Extended,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DeclMode {
    Off,
    /// Keep declaration-position tokens, drop references.
    Only,
    /// Emit declaration-position tokens twice, references once.
    Boost,
}

/// Render a `doc_text` — relative path on line 1, chunk body after — for
/// embedding. `None` is the identity and allocates nothing; every other
/// variant emits the code-aware token stream, space-joined.
///
/// Token *order* is irrelevant downstream (both `ese::encode_single` and
/// `embed_sif` pool by a weighted mean, and MaxSim scores per token), but
/// token *frequency* is not — it is what mean pooling weights by. That is why
/// the prune tiers express "matters less" as fewer emissions rather than as a
/// reordering, and why `Scaled` may safely emit the path after the body.
pub fn render_doc(doc: &str, p: EmbedPreproc, path: PathRender) -> Cow<'_, str> {
    if p.is_none() && path == PathRender::Full {
        return Cow::Borrowed(doc);
    }
    let (path_text, body) = match doc.find('\n') {
        Some(i) => (&doc[..i], &doc[i + 1..]),
        // No newline: doc_text always writes one, so this is a body-only
        // string from a caller that never had a path. Treat it as all body.
        None => ("", doc),
    };
    let plan = p.plan();
    let mut body_toks = Vec::new();
    render_body_into(body, plan, &mut body_toks);
    let mut path_toks = Vec::new();
    render_path_into(path_text, plan, path, body_toks.len(), &mut path_toks);

    path_toks.extend(body_toks);
    if plan.dedupe {
        dedupe(&mut path_toks);
    }
    Cow::Owned(path_toks.join(" "))
}

/// Render text that carries no path — the SIF counting pass reads whole files
/// (`store::build::sif`), and its frequencies must describe what pooling will
/// see rather than what one chunk happens to contain.
pub fn render_body(text: &str, p: EmbedPreproc) -> Cow<'_, str> {
    if p.is_none() {
        return Cow::Borrowed(text);
    }
    let plan = p.plan();
    let mut toks = Vec::new();
    render_body_into(text, plan, &mut toks);
    if plan.dedupe {
        dedupe(&mut toks);
    }
    Cow::Owned(toks.join(" "))
}

/// Render a query. Normalization from the tier, pruning from none of it.
pub fn render_query(query: &str, p: EmbedPreproc) -> Cow<'_, str> {
    if p.is_none() {
        return Cow::Borrowed(query);
    }
    let plan = p.query_plan();
    let mut toks = Vec::new();
    render_body_into(query, plan, &mut toks);
    if plan.dedupe {
        dedupe(&mut toks);
    }
    Cow::Owned(toks.join(" "))
}

fn render_body_into(text: &str, plan: Plan, out: &mut Vec<String>) {
    let words = word_ranges(text);
    let decl = if plan.decl == DeclMode::Off {
        Vec::new()
    } else {
        declaration_sites(text, &words)
    };
    let mut buf = String::with_capacity(32);
    for (i, &(s, e)) in words.iter().enumerate() {
        let is_decl = decl.get(i).copied().unwrap_or(false);
        if plan.decl == DeclMode::Only && !is_decl {
            continue;
        }
        let reps = if plan.decl == DeclMode::Boost && is_decl { 2 } else { 1 };
        token::subtokens_of(
            &text[s..e],
            plan.whole_idents,
            &mut buf,
            &mut |tok: &str, whole: bool| {
                if drops(tok, whole, plan) {
                    return;
                }
                for _ in 0..reps {
                    out.push(tok.to_string());
                }
            },
        );
    }
}

fn render_path_into(
    path_text: &str,
    plan: Plan,
    path: PathRender,
    n_body: usize,
    out: &mut Vec<String>,
) {
    if path_text.is_empty() {
        return;
    }
    // The path is never keyword- or low-signal-filtered: `src/vs/type/...` is
    // a directory called type, not the TypeScript keyword. Only the tier's
    // subtoken normalization applies.
    let source = match path {
        PathRender::Tail | PathRender::Scaled => tail_segments(path_text, 2),
        _ => path_text,
    };
    let mut buf = String::with_capacity(32);
    let mut toks = Vec::new();
    for &(s, e) in word_ranges(source).iter() {
        token::subtokens_of(
            &source[s..e],
            plan.whole_idents,
            &mut buf,
            &mut |tok: &str, _whole: bool| toks.push(tok.to_string()),
        );
    }
    if path != PathRender::Full {
        dedupe(&mut toks);
    }
    if path == PathRender::Scaled {
        // Keep the most specific end: the filename says more than its parent.
        let cap = ((n_body as f32 * PATH_SHARE).round() as usize).max(1);
        if toks.len() > cap {
            toks.drain(..toks.len() - cap);
        }
    }
    out.extend(toks);
}

/// The last `n` `/`-separated segments of a path.
fn tail_segments(path: &str, n: usize) -> &str {
    let mut cut = path.len();
    for _ in 0..n {
        match path[..cut].rfind('/') {
            Some(i) => cut = i,
            None => return path,
        }
    }
    &path[cut + 1..]
}

fn dedupe(toks: &mut Vec<String>) {
    let mut seen = std::collections::HashSet::new();
    toks.retain(|t| seen.insert(t.clone()));
}

fn drops(tok: &str, whole: bool, plan: Plan) -> bool {
    // Positional: the table fires only on a token that WAS the whole run.
    // `def` in `def foo()` is boilerplate; the `init` inside `__init__` is the
    // name of the thing being searched for (§22.1).
    let eligible = !plan.positional || whole;
    let kw = eligible
        && match plan.keywords {
            Keywords::None => false,
            Keywords::Legacy => is_keyword(tok),
            Keywords::Extended => is_keyword(tok) || is_keyword_extra(tok),
        };
    if kw || (plan.keywords != Keywords::None && eligible && is_number(tok)) {
        return true;
    }
    plan.low_signal && is_low_signal(tok)
}

fn is_number(tok: &str) -> bool {
    tok.chars().all(|c| c.is_ascii_digit())
}

/// Byte ranges of every `[alphanumeric_]+` run, matching the segmentation
/// `token::for_each_token_with` splits on — so a word here is exactly a word
/// there, and a declaration verdict maps onto the subtokens it produces.
fn word_ranges(text: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    for (i, c) in text.char_indices() {
        if c.is_alphanumeric() || c == '_' {
            start.get_or_insert(i);
        } else if let Some(s) = start.take() {
            out.push((s, i));
        }
    }
    if let Some(s) = start {
        out.push((s, text.len()));
    }
    out
}

/// Which words sit in a declaration position, decided lexically — no parser,
/// because the corpus is seven languages and a parse per chunk is a per-build
/// cost this lever has not yet earned.
///
/// Three cases: a name following a declaring keyword, a name inside the
/// parameter list that keyword opened, and the left-hand side of a plain `=`.
/// Errs toward *declaring*: everything inside a parameter list counts, so
/// `int attempt` and `attempt: number` both keep `attempt` without knowing
/// which language put the type first — the type name itself is what
/// [`LOW_SIGNAL`] is for.
fn declaration_sites(text: &str, words: &[(usize, usize)]) -> Vec<bool> {
    let mut decl = vec![false; words.len()];
    let mut depth: i32 = 0;
    let mut param_depth: Option<i32> = None;
    // Inside a declaration head: past a declaring keyword, not yet past the
    // token that ends the head. Not "the very next word" — `static int
    // compute_backoff(` puts a return type between the two, and C, Java and Go
    // all do this. Every word in the head counts, and [`LOW_SIGNAL`] is what
    // removes the type, the same way it does inside the parameter list.
    let mut in_head = false;
    let mut prev_end = 0usize;

    for (i, &(s, e)) in words.iter().enumerate() {
        for c in text[prev_end..s].chars() {
            match c {
                '(' | '[' => {
                    // The parenthesis that ends a declaration head opens the
                    // parameter list that head declares.
                    if in_head && c == '(' && param_depth.is_none() {
                        param_depth = Some(depth);
                    }
                    depth += 1;
                    if c == '(' {
                        in_head = false;
                    }
                }
                ')' | ']' => {
                    depth -= 1;
                    if param_depth.is_some_and(|d| depth <= d) {
                        param_depth = None;
                    }
                }
                // Anything that ends a declaration head. The newline matters:
                // without it a bare `const` bleeds into the next statement.
                '=' | ';' | '{' | '}' | ',' | '\n' => in_head = false,
                _ => {}
            }
        }
        prev_end = e;

        let word = &text[s..e];
        if is_declarer(word) {
            // `export function foo`: two declarers in a row still declare foo.
            in_head = true;
            continue;
        }

        let rest = text[e..].trim_start();
        let mut chars = rest.chars();
        let next = chars.next();
        // A plain `=`, not ==, =>, or =~.
        let assigns =
            next == Some('=') && !matches!(chars.next(), Some('=') | Some('>') | Some('~'));
        let in_params = param_depth.is_some_and(|d| depth > d);

        if in_head || in_params || assigns {
            decl[i] = true;
        }
    }
    decl
}

/// Language keywords across the corpus languages this project measures
/// (C, Rust, TS/JS, Python, Java, Go, Ruby). Lowercased, sorted — tokens
/// arriving here already are. Type names double as English words (`string`,
/// `float`) stay: dropping them costs prose queries more than it saves chunks.
///
/// **Frozen.** `SplitNokw` was measured against exactly this list in §14.4;
/// what it turned out to be missing lives in [`KEYWORDS_EXTRA`] so the repair
/// is a separate, attributable arm rather than a silent edit to a published
/// condition.
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

/// What [`KEYWORDS`] was missing. `function` and `export` are the conspicuous
/// two — they survived every §14 condition, so the most common tokens in the
/// TS corpus were being embedded as content. Sorted, and disjoint from
/// `KEYWORDS`; both are asserted.
const KEYWORDS_EXTRA: &[&str] = &[
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
const LOW_SIGNAL: &[&str] = &[
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
const DECLARERS: &[&str] = &[
    "abstract", "class", "const", "def", "enum", "export", "fn", "func", "function", "impl",
    "interface", "let", "module", "namespace", "object", "package", "private", "protected", "pub",
    "public", "readonly", "static", "struct", "trait", "type", "val", "var",
];

fn is_keyword(tok: &str) -> bool {
    KEYWORDS.binary_search(&tok).is_ok()
}

fn is_keyword_extra(tok: &str) -> bool {
    KEYWORDS_EXTRA.binary_search(&tok).is_ok()
}

fn is_low_signal(tok: &str) -> bool {
    LOW_SIGNAL.binary_search(&tok).is_ok()
}

/// Case-insensitive because a declarer is matched against the raw word, before
/// the subtoken lowercasing that the rest of the pipeline does.
fn is_declarer(word: &str) -> bool {
    let lower = word.to_ascii_lowercase();
    DECLARERS.binary_search(&lower.as_str()).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PATH: &str = "src/vs/workbench/contrib/searchEditor/browser/searchEditorActions.ts";
    const BODY: &str = "export function computeBackoffDelay(attempt: number): number {\n  \
                        const jitter = Math.random() * BASE_DELAY_MS;\n  \
                        return Math.min(MAX_DELAY_MS, 2 ** attempt * jitter);\n}";

    fn doc() -> String {
        crate::corpus::doc_text(PATH, BODY)
    }

    fn render(p: EmbedPreproc) -> String {
        let d = doc();
        render_doc(&d, p, PathRender::Full).into_owned()
    }

    #[test]
    fn none_is_the_identity_and_borrows() {
        let s = "fn get_user_name(&self) -> String";
        assert!(matches!(
            render_doc(s, EmbedPreproc::None, PathRender::Full),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn split_renders_identifiers_as_prose() {
        let s = "fn get_user_name(&self) { retry_backoff += 1; }";
        assert_eq!(
            render_body(s, EmbedPreproc::Split),
            "fn get user name self retry backoff"
        );
    }

    #[test]
    fn split_whole_keeps_the_identifier_too() {
        assert_eq!(
            render_body("getUserName", EmbedPreproc::SplitWhole),
            "get user name getusername"
        );
        // Undecomposed words carry no duplicate.
        assert_eq!(render_body("backoff", EmbedPreproc::SplitWhole), "backoff");
    }

    #[test]
    fn nokw_drops_keywords_and_numbers_but_not_content() {
        let s = "def compute_delay(retries): return delay * 250";
        assert_eq!(
            render_body(s, EmbedPreproc::SplitNokw),
            "compute delay retries delay"
        );
    }

    #[test]
    fn as_str_round_trips_and_matches_the_serde_spelling() {
        // meta.json stores the serde (kebab) form and the harness compares it
        // against the flag string, so a variant whose as_str disagrees with its
        // serde name builds an index the readback assertion then rejects.
        for name in EmbedPreproc::ALL {
            let v = EmbedPreproc::parse(name).unwrap_or_else(|| panic!("{name}"));
            assert_eq!(v.as_str(), *name);
            let json = serde_json::to_string(&v).unwrap();
            assert_eq!(json, format!("\"{name}\""), "serde disagrees for {name}");
        }
    }

    #[test]
    fn tables_are_sorted_for_binary_search() {
        for t in [KEYWORDS, KEYWORDS_EXTRA, LOW_SIGNAL, DECLARERS] {
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

    #[test]
    fn the_frozen_table_really_was_missing_function_and_export() {
        // The finding that motivated §20. If this ever fails, someone edited a
        // published condition and §14.4's numbers no longer describe it.
        assert!(!is_keyword("function"));
        assert!(!is_keyword("export"));
        assert!(is_keyword_extra("function"));
        assert!(is_keyword_extra("export"));
    }

    #[test]
    fn punctuation_noise_never_survives() {
        // §9.8: `_` matched anything with cosine 1.0. It must not exist here.
        let r = render_body("a[_x] = {_y: *_z}; // _", EmbedPreproc::Split);
        assert!(!r.contains('_'), "{r:?}");
        assert!(!r.contains('['));
    }

    #[test]
    fn kebab_and_snake_separators_are_removed_not_kept() {
        // Hyphens are split chars like any punctuation, so kebab-case CSS/CLI
        // identifiers render as prose too — and no separator reaches ese.
        assert_eq!(
            render_body("--embed-preproc font-size", EmbedPreproc::Split),
            "embed preproc font size"
        );
        assert_eq!(render_body("get_user-name", EmbedPreproc::Split), "get user name");
    }

    #[test]
    fn prune_kw_drops_what_the_frozen_table_missed() {
        let nokw = render(EmbedPreproc::SplitNokw);
        let kw = render(EmbedPreproc::PruneKw);
        assert!(nokw.contains(" export "), "{nokw}");
        assert!(nokw.contains(" function "), "{nokw}");
        assert!(!kw.contains(" export "), "{kw}");
        assert!(!kw.contains(" function "), "{kw}");
    }

    #[test]
    fn prune_lex_drops_types_namespaces_and_units() {
        let r = render(EmbedPreproc::PruneLex);
        for gone in ["number", "math", "ms"] {
            assert!(!r.split(' ').any(|t| t == gone), "{gone} survived: {r}");
        }
        // The domain words are exactly what must not be touched.
        for kept in ["compute", "backoff", "delay", "jitter", "attempt"] {
            assert!(r.split(' ').any(|t| t == kept), "{kept} was dropped: {r}");
        }
    }

    #[test]
    fn prune_decl_keeps_the_declared_name_and_params_only() {
        let r = render(EmbedPreproc::PruneDecl);
        let body: Vec<&str> = r.split(' ').skip_while(|t| *t != "ts").skip(1).collect();
        assert_eq!(body, ["compute", "backoff", "delay", "attempt", "jitter"]);
    }

    #[test]
    fn prune_soft_weights_declarations_without_deleting_references() {
        let r = render(EmbedPreproc::PruneSoft);
        let n = |t: &str| r.split(' ').filter(|x| *x == t).count();
        // Declared name twice, a pure reference still present exactly once.
        assert_eq!(n("backoff"), 2);
        assert_eq!(n("random"), 1);
        assert!(n("min") >= 1, "call-site tokens must survive: {r}");
    }

    #[test]
    fn prune_uniq_emits_each_token_once() {
        let r = render(EmbedPreproc::PruneUniq);
        let toks: Vec<&str> = r.split(' ').collect();
        let mut sorted = toks.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(toks.len(), sorted.len(), "duplicate survived: {r}");
    }

    #[test]
    fn declaration_sites_ignore_comparison_and_arrow() {
        // `==` and `=>` are not assignments; treating them as such would
        // declare half of every conditional.
        let s = "if (retryCount == maxRetries) { items.map(x => x.id); }";
        let r = render_body(s, EmbedPreproc::PruneDecl);
        assert!(!r.contains("retry"), "{r}");
        assert!(!r.contains("max"), "{r}");
    }

    #[test]
    fn declaration_sites_survive_c_style_parameter_order() {
        // `int attempt` puts the type first; erring toward declaring keeps the
        // name, and LOW_SIGNAL removes the type.
        let s = "static int compute_backoff(int attempt, long base) { return attempt; }";
        let r = render_body(s, EmbedPreproc::PruneDecl);
        for kept in ["compute", "backoff", "attempt", "base"] {
            assert!(r.split(' ').any(|t| t == kept), "{kept} missing: {r}");
        }
    }

    #[test]
    fn path_dedupe_collapses_the_repeated_segment() {
        let d = doc();
        let full = render_doc(&d, EmbedPreproc::PruneLex, PathRender::Full);
        let dd = render_doc(&d, EmbedPreproc::PruneLex, PathRender::Dedupe);
        // `searchEditor/` and `searchEditorActions` say it twice, and the body
        // says it not at all.
        assert_eq!(full.split(' ').filter(|t| *t == "editor").count(), 2);
        assert_eq!(dd.split(' ').filter(|t| *t == "editor").count(), 1);
    }

    #[test]
    fn path_tail_keeps_only_the_last_two_segments() {
        let d = doc();
        let r = render_doc(&d, EmbedPreproc::PruneLex, PathRender::Tail);
        assert!(!r.contains("workbench"), "{r}");
        assert!(!r.contains("contrib"), "{r}");
        assert!(r.contains("browser"), "{r}");
        assert!(r.contains("actions"), "{r}");
    }

    #[test]
    fn path_scaled_pins_the_paths_share_as_the_body_shrinks() {
        // The problem Scaled exists for: at the aggressive tier Full leaves the
        // path holding 11 of 16 tokens, so the vector mostly says "where this
        // file lives" and every window in the file converges. Scaled keeps the
        // share near PATH_SHARE at every rung instead of letting it climb.
        let d = doc();
        let n_full = render_doc(&d, EmbedPreproc::PruneDecl, PathRender::Full)
            .split(' ')
            .count();
        assert_eq!(n_full, 16);

        for tier in [EmbedPreproc::PruneLex, EmbedPreproc::PruneDecl] {
            let r = render_doc(&d, tier, PathRender::Scaled).into_owned();
            let total = r.split(' ').count() as f32;
            let body = render_body(BODY, tier).split(' ').count() as f32;
            let share = (total - body) / total;
            assert!(
                (0.1..=0.35).contains(&share),
                "{tier:?} path share {share:.2} outside the band: {r}"
            );
        }
    }

    #[test]
    fn queries_are_never_pruned_only_normalized() {
        // A query has no declarations; PruneDecl on the query side would empty
        // it, and the low-signal table would eat real query words.
        let q = "parse a number from a string";
        for p in [EmbedPreproc::PruneLex, EmbedPreproc::PruneDecl, EmbedPreproc::PruneUniq] {
            let r = render_query(q, p);
            assert!(r.contains("number"), "{p:?} ate a query word: {r}");
            assert!(r.contains("string"), "{p:?} ate a query word: {r}");
            assert!(r.contains("parse"), "{p:?}: {r}");
        }
    }

    #[test]
    fn keyword_pruning_is_symmetric_and_the_rest_is_not() {
        // §20.6, pinned, because it is the counter-intuitive half. Keyword
        // pruning applies to queries too — it costs real query words and wins
        // anyway, on all four corpora, because matching the corpus vocabulary
        // beats keeping the word. Anything a query cannot mirror stays
        // document-side.
        let q = "python logging can not create file from a type of object";
        // PruneKwPosQ0 is deliberately excluded: it is the arm that does NOT
        // mirror, and §22.1 P3 is the experiment that decides whether §20.6's
        // rule survives in the agent regime.
        for p in [EmbedPreproc::PruneKw, EmbedPreproc::PruneLex, EmbedPreproc::PruneDecl] {
            let r = render_query(q, p);
            for gone in ["not", "from", "type", "of", "object"] {
                assert!(!r.split(' ').any(|t| t == gone), "{p:?} kept {gone:?}: {r}");
            }
            // ...and the content words are all still there.
            for kept in ["python", "logging", "can", "create", "file"] {
                assert!(r.split(' ').any(|t| t == kept), "{p:?} ate {kept:?}: {r}");
            }
        }
        // The asymmetric half: a query has no declarations and the low-signal
        // table would eat it, so neither reaches the query side.
        let r = render_query("parse a number from a string", EmbedPreproc::PruneDecl);
        for kept in ["parse", "number", "string"] {
            assert!(r.split(' ').any(|t| t == kept), "{kept:?} lost: {r}");
        }
    }

    #[test]
    fn the_sym_variants_mirror_their_tier_onto_the_query() {
        // §20.7: same documents as the tiers they mirror, different queries.
        // If the doc side moved too, the arm would confound symmetry with a
        // rendering change and could not discriminate anything.
        let d = doc();
        for (asym, sym) in [
            (EmbedPreproc::PruneLex, EmbedPreproc::PruneLexSym),
            (EmbedPreproc::PruneUniq, EmbedPreproc::PruneUniqSym),
        ] {
            assert_eq!(
                render_doc(&d, asym, PathRender::Full),
                render_doc(&d, sym, PathRender::Full),
                "{sym:?} changed the document side"
            );
        }
        // The query side is where they differ: low-signal words now go.
        let q = "parse a number from a string";
        assert!(render_query(q, EmbedPreproc::PruneLex).contains("number"));
        let sym = render_query(q, EmbedPreproc::PruneLexSym);
        assert!(!sym.split(' ').any(|t| t == "number"), "{sym}");
        assert!(!sym.split(' ').any(|t| t == "string"), "{sym}");
        assert!(sym.split(' ').any(|t| t == "parse"), "{sym}");

        // Dedupe reaches the query only under the mirrored variant.
        let rep = "cache cache lookup";
        assert_eq!(render_query(rep, EmbedPreproc::PruneUniq).matches("cache").count(), 2);
        assert_eq!(render_query(rep, EmbedPreproc::PruneUniqSym).matches("cache").count(), 1);
    }

    #[test]
    fn positional_keeps_identifier_components_and_still_drops_boilerplate() {
        // The §22.1 regression: `PruneKw`'s table damaged 20.9% of the gold
        // function names agents were hunting, because it fires on a subtoken
        // wherever it appears. Positional fires only on a whole run.
        let kept = |p: EmbedPreproc, text: &str, tok: &str| {
            render_body(text, p).split(' ').any(|t| t == tok)
        };
        // The name of the thing being searched for survives.
        assert!(!kept(EmbedPreproc::PruneKw, "__init__", "init"), "naive kept init");
        assert!(kept(EmbedPreproc::PruneKwPos, "__init__", "init"));
        for (text, tok) in [("from_dict", "from"), ("as_completed", "as"),
                            ("get_object_or_404", "object"), ("for_each", "for")] {
            assert!(kept(EmbedPreproc::PruneKwPos, text, tok), "{text} lost {tok}");
        }
        // ...and real boilerplate still goes.
        for (text, tok) in [("def compute_backoff(x)", "def"), ("class Foo", "class"),
                            ("self.value", "self"), ("x: type = None", "type")] {
            assert!(!kept(EmbedPreproc::PruneKwPos, text, tok), "{text} kept {tok}");
        }
        // The compound survives even when its own subtoken is a keyword.
        let r = render_body("self.default_type", EmbedPreproc::PruneKwPos);
        assert!(r.contains("default") && r.contains("type"), "{r}");
        assert!(!r.split(' ').any(|t| t == "self"), "{r}");
    }

    #[test]
    fn q0_leaves_the_query_alone_but_still_prunes_documents() {
        // §22.1 P3's arm: documents lose standalone keywords, queries lose
        // nothing. The disputed share is 9.1% of agent query tokens either way.
        let q = "def compute backoff class handler";
        assert_eq!(render_query(q, EmbedPreproc::PruneKwPosQ0), q);
        // ...while its documents still drop exactly those words.
        let d = render_body("def compute_backoff()", EmbedPreproc::PruneKwPosQ0);
        assert!(!d.split(' ').any(|t| t == "def"), "{d}");
        assert!(d.contains("compute") && d.contains("backoff"), "{d}");
        // Its symmetric twin renders documents identically - the arms differ
        // on the query side only, or the 2x2 confounds two changes.
        assert_eq!(
            render_body("def compute_backoff()", EmbedPreproc::PruneKwPos),
            d
        );
        assert_ne!(
            render_query(q, EmbedPreproc::PruneKwPos),
            render_query(q, EmbedPreproc::PruneKwPosQ0)
        );
    }

    #[test]
    fn ladder_is_cumulative() {
        // Each rung must be a subset of the one below it, or "T3 is T2 plus a
        // rule" is not true and the campaign cannot attribute a delta to one
        // step. Dedupe and Boost change counts, not membership, so compare sets.
        let d = doc();
        let set = |p: EmbedPreproc| -> std::collections::HashSet<String> {
            render_doc(&d, p, PathRender::Full).split(' ').map(str::to_string).collect()
        };
        let rungs = [
            EmbedPreproc::Split,
            EmbedPreproc::PruneKw,
            EmbedPreproc::PruneLex,
            EmbedPreproc::PruneDecl,
        ];
        for w in rungs.windows(2) {
            let (a, b) = (set(w[0]), set(w[1]));
            assert!(b.is_subset(&a), "{:?} is not a subset of {:?}", w[1], w[0]);
        }
        for p in [EmbedPreproc::PruneSoft, EmbedPreproc::PruneUniq] {
            assert!(set(p).is_subset(&set(EmbedPreproc::PruneLex)), "{p:?}");
        }
    }
}
