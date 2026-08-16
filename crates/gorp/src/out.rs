//! Everything that writes to stdout or stderr.
//!
//! One rule, and the CLI tests enforce it: **stdout is data, stderr is
//! commentary.** Both modes print the unit view (RESEARCH.md §34) — a
//! `path:start-end` header, then `line:␣␣text` rows, blank-line separated —
//! ranked mode one block per hit, exact mode one block per file with every
//! matched line bold. Footers, warnings, stats, and suggestions go to stderr,
//! so a pipeline gets clean data and a human still gets told what happened.
//!
//! Colour is a second, strictly optional layer over that same grammar: every
//! escape wraps a field the plain output already had, so removing them gives
//! back the bytes a parser sees. It is off unless stdout is a terminal, which
//! is what keeps the contract intact under a pipe. What it paints is not a
//! decoration but the engine's own reasoning — see [`palette`] for what each
//! hue answers and [`Emphasis`] for what "the match" means when there is no
//! match to point at.
//!
//! Printing lives here rather than beside the commands so the commands are about
//! deciding what to show, not how.

/// The program name, as it appears in every line this binary writes to
/// stderr. One constant because ~26 call sites spelled it as a literal
/// before, and a rename that misses one produces output signed with two
/// different names.
pub const PROG: &str = "gorp";

use gorp_core::corpus;
use gorp_core::rank::Mode;
use gorp_core::search::{SearchHit, SearchResult, UnitRow};
use std::borrow::Cow;
use std::collections::HashSet;
use std::path::Path;

/// Exact mode prints at most this many matched lines unless --all is given
/// (`-C` context rows ride on top); the footer reports the true total, so
/// enumeration is never silently lossy.
pub const EXACT_PRINT_CAP: usize = 250;

/// Characters of a hit's line that get printed, before `-M` overrides it.
///
/// `EXACT_PRINT_CAP` bounds how many lines are printed and nothing bounded how
/// wide one could be, so the two together bounded nothing: a ranked k=5 search
/// for `add_code` returned 659 KB because one hit was a single-line 374 KB JSON
/// fixture, and `-e equation` returned 12.5 MB the same way. Measured over 366
/// real agent searches, 23 lines over 1,000 characters carried 73% of all bytes
/// ever printed. That is not just cost: the agent's tool-result limit truncates
/// what it is given, so one minified line silently deletes the hits ranked below
/// it — the result is worse, not merely more expensive.
///
/// 200 is chosen to clear the p90 real result line (121 characters) with room,
/// so ordinary code is never cut.
pub const MAX_COLUMNS: usize = 200;

/// Opens the marker appended to a line that `MAX_COLUMNS` cut; the count of
/// characters dropped and a closing `chars]` follow. Carrying the count lets a
/// caller weigh `-M 0` against what it would actually buy before paying for it.
const OMITTED: &str = " [… +";

/// A unit-view row printed where the row numbers jump: lines were elided
/// there. Bare — no count, no gutter — because the numbers on either side
/// already say exactly how many, and a marker must not parse as a hit line.
const ELISION: &str = "⋮";

/// What separates a row's line number from its text when the path is not on
/// the line. Two spaces rather than a tab: a tab renders at whatever the
/// consumer's tab stop happens to be, so the same output was 8 columns of
/// gutter in one terminal and 4 in another, and a line whose own text
/// contains tabs re-aligned everything after it. Two spaces are two columns
/// everywhere, which is the whole requirement — the gutter exists to keep
/// `264:code` from running the number into the code, not to build a table.
const GUTTER: &str = "  ";

/// The palette, as roles rather than colours.
///
/// **Colour lives in the gutter; the code is left alone.** That is the rule
/// the rest follows from. A row's line number says where the row stands in
/// the answer — chosen, scored, or added around the edges — and the source
/// text on that row is the same source text either way. Styling it too made
/// one file's code render in two colours inside one block, which reads as
/// syntax highlighting that has lost its mind.
///
/// So: three cool hues and a neutral, each answering a different question.
/// **Blue names the file.** **Cyan counts the lines**, and goes bold on the
/// one line the engine chose. **Grey marks the numbers of rows the view
/// added** — de-orphaning furniture and `-C` context, there to make the
/// window readable rather than because anything matched in them. **Green is
/// your own words**, and it is the one colour that does reach into the text,
/// because a word of the query is a fact about the text rather than about
/// the row.
///
/// A block therefore reads before a word of it is read: one bold number is
/// the ranking's actual answer, the plain cyan numbers are the window it was
/// scored in, the grey numbers are scaffolding, and green anywhere is you.
/// Weight and hue carry different axes, which is why the chosen line is the
/// *same* cyan as its neighbours rather than a fourth colour — it is not a
/// different kind of thing, it is the one the engine picked.
///
/// Only the sixteen ANSI colours are used, never 256-colour or truecolor
/// codes, so the output takes the terminal's *theme* rather than overriding
/// it: the same escape is a muted teal in one scheme and a bright one in
/// another, which is the behaviour a user configuring their terminal expects
/// and the reason ripgrep does the same. Grey is `90` (bright black) rather
/// than SGR `2`, because dim is the one attribute terminals routinely drop —
/// a palette whose "recedes" state can silently render identical to its
/// "matters" state is not a palette.
mod palette {
    /// The path, on a header line.
    pub const PATH: &str = "\x1b[94m";
    /// A line number, and the `start-end` span in a header.
    pub const LINE: &str = "\x1b[2;36m";
    /// The line the engine actually chose out of the span (`SearchHit::line`)
    /// — the single most useful thing gorp knows that ripgrep does not.
    pub const LINE_BEST: &str = "\x1b[1;36m";
    /// The *number* of a row outside the scored window: de-orphaning
    /// furniture, or a `-C` context row the caller asked for.
    ///
    /// The number only. Greying the text as well was the first version and it
    /// read as nonsense: the same source line came out two different colours
    /// depending on which hit it turned up in, and a block of code became a
    /// gradient. A row's position in the answer is a fact about the gutter —
    /// the code on it is just code.
    pub const LINE_CTX: &str = "\x1b[90m";
    /// A word of the query, found in the text (see [`super::Emphasis`]).
    pub const TERM: &str = "\x1b[1;92m";
    pub const RESET: &str = "\x1b[0m";
}

use palette::RESET;

/// `text` wrapped in an ANSI escape when `on`, untouched when not.
///
/// The un-coloured branch returns the same string, so a caller need not
/// duplicate its `println!` per colour state — which is what keeps colour
/// from forking the output grammar into two.
fn paint(on: bool, style: &str, text: impl std::fmt::Display) -> String {
    if on { format!("{style}{text}{RESET}") } else { text.to_string() }
}

/// The words of the query, for emphasis inside a result's text.
///
/// ripgrep bolds the substring its regex matched. gorp has no such substring:
/// a semantic hit is a hit because two vectors are close, and the answer can
/// share no word at all with the question. So the honest analogue is not
/// "what matched" but **"where your words landed"** — and the emphasis has to
/// be allowed to find nothing, because finding nothing is a true and useful
/// statement about a purely semantic hit.
///
/// Matching runs through the engine's own tokenizer rather than a second idea
/// of what a word is, which is what makes `getUserName` light up for a query
/// of "user name" and `retry_backoff` for "backoff". A *whole* run lights up
/// when any of its subtokens is a query word: painting half an identifier is
/// noisier to read than painting the identifier, and the reader is looking
/// for the line, not the character.
///
/// Two guards keep this from painting the whole screen. Words shorter than
/// three characters and the closed stoplist below are dropped, because a
/// query is usually a sentence and `the` occurs in every row of every result;
/// and `terms` is empty whenever colour is off, so the whole mechanism costs
/// one branch under a pipe.
#[derive(Default)]
pub struct Emphasis {
    terms: Vec<String>,
    how: Match,
}

/// How a term is compared against a run of text — the difference between the
/// two modes gorp searches in.
#[derive(Default, PartialEq)]
pub enum Match {
    /// Ranked mode: a run lights up when the tokenizer finds a query word
    /// *inside* it, so `getUserName` answers "user name". The engine scored
    /// this hit on those same subtokens, so this is what it saw.
    #[default]
    Subtoken,
    /// Exact mode: a run lights up when it contains the literal, and nothing
    /// is decomposed. `-e compute_backoff_delay` must not light up the word
    /// `delay` in a comment three lines up — the pattern did not match there,
    /// and emphasis that outruns the match is a lie about the result.
    Literal,
}

/// Query words that carry no location information. Not the engine's business
/// — BM25 handles these with idf and needs no list — but a *display* one:
/// emphasis that fires on `the` is emphasis nobody can read past.
/// Function words only — nothing that could name a thing in a codebase. `set`
/// and `get` were on this list once and had to come off: they are half the
/// accessors in the tree, and a query that says `get` means it.
const STOPWORDS: &[&str] = &[
    "about", "after", "also", "and", "any", "are", "been", "before", "being", "between",
    "both", "but", "can", "could", "does", "each", "for", "from", "has", "have", "her", "here",
    "him", "his", "how", "into", "its", "just", "more", "most", "much", "must", "not", "now",
    "off", "one", "only", "onto", "other", "our", "out", "over", "same", "she", "should",
    "since", "some", "such", "than", "that", "the", "their", "them", "then", "there", "these",
    "they", "this", "those", "through", "too", "under", "until", "use", "used", "using",
    "very", "was", "were", "what", "when", "where", "which", "why", "will", "with", "would",
    "you", "your",
];

impl Emphasis {
    /// The query's emphasis words, in the engine's tokenization.
    pub fn of(query: &str) -> Self {
        let terms = gorp_core::text::token::tokens(query)
            .into_iter()
            .filter(|t| t.chars().count() >= 3 && !STOPWORDS.contains(&t.as_str()))
            .collect();
        Self { terms, how: Match::Subtoken }.sorted()
    }

    /// The literal words of an exact-mode pattern, undecomposed.
    ///
    /// No stoplist and no length floor here: `-e of` is a search the caller
    /// typed on purpose, and every line printed contains a real match, so
    /// there is nothing to protect them from.
    pub fn literal(pattern: &str) -> Self {
        let terms = pattern
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .filter(|w| !w.is_empty())
            .map(str::to_lowercase)
            .collect();
        Self { terms, how: Match::Literal }.sorted()
    }

    /// Sorted and deduped, which is what makes the per-run lookup a binary
    /// search rather than a scan of a list a long query can make long.
    fn sorted(mut self) -> Self {
        self.terms.sort();
        self.terms.dedup();
        self
    }

    /// Nothing to emphasise — either the caller built it from a pattern it
    /// could not read as words (a regex), or every word was a stopword.
    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }

    /// Does this run of `[alphanumeric_]` characters carry a query word?
    fn hits_run(&self, run: &str) -> bool {
        match self.how {
            Match::Subtoken => gorp_core::text::token::tokens(run)
                .iter()
                .any(|t| self.terms.binary_search(t).is_ok()),
            // Lowercased once per run rather than per term: a `-i` search is
            // the common one, and a case-sensitive comparison would leave
            // half of a case-insensitive match unpainted.
            Match::Literal => {
                let run = run.to_lowercase();
                self.terms.iter().any(|t| run.contains(t.as_str()))
            }
        }
    }

    /// `text` with its query words emphasised and everything else in `base`.
    ///
    /// `base` is re-opened after every emphasised run rather than assumed:
    /// the reset that closes an emphasis closes the surrounding style too, so
    /// a greyed context row would silently stop being grey at its first
    /// query word. Returned unchanged when there is nothing to paint, so the
    /// no-colour path allocates a plain copy and no escapes at all.
    fn apply(&self, text: &str, base: &str, on: bool) -> String {
        if !on {
            return text.to_string();
        }
        if self.is_empty() {
            return if base.is_empty() {
                text.to_string()
            } else {
                format!("{base}{text}{RESET}")
            };
        }
        let mut out = String::with_capacity(text.len() + 16);
        out.push_str(base);
        // Runs are split exactly as the tokenizer splits them, so what is
        // tested is what is painted.
        for (is_word, part) in runs(text) {
            if is_word && self.hits_run(part) {
                out.push_str(palette::TERM);
                out.push_str(part);
                out.push_str(RESET);
                out.push_str(base);
            } else {
                out.push_str(part);
            }
        }
        if !base.is_empty() {
            out.push_str(RESET);
        }
        out
    }
}

/// `text` cut into alternating word / non-word runs, a word being the
/// `[alphanumeric_]+` run `text::token` tokenizes.
fn runs(text: &str) -> impl Iterator<Item = (bool, &str)> {
    let mut rest = text;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let word = |c: char| c.is_alphanumeric() || c == '_';
        let is_word = rest.starts_with(word);
        let end = rest.find(|c: char| word(c) != is_word).unwrap_or(rest.len());
        let (head, tail) = rest.split_at(end);
        rest = tail;
        Some((is_word, head))
    })
}

/// Whether to colour, from `--color`'s three values.
///
/// `auto` is the default and means *a human is watching*: stdout is a
/// terminal, `NO_COLOR` is unset (the informal standard every colouring tool
/// honours), and `TERM` is not `dumb`. Under a pipe, a harness, or `--json`
/// this is false and stdout is the same bytes it has always been — the
/// `path:line:text` contract is not something a display preference gets to
/// change.
pub fn color_enabled(when: &str) -> bool {
    match when {
        "always" => true,
        "never" => false,
        _ => {
            use std::io::IsTerminal;
            std::io::stdout().is_terminal()
                && std::env::var_os("NO_COLOR").is_none()
                && std::env::var_os("TERM").is_none_or(|t| t != "dumb")
        }
    }
}

/// A line as it gets printed: at most `max` characters, marked when anything was
/// dropped. `max == 0` is no limit.
///
/// Cutting on a character index rather than a byte one keeps multi-byte text
/// from panicking here, and means `-M 200` is 200 characters of every language
/// rather than 200 bytes of some of them.
fn clip(text: &str, max: usize) -> Cow<'_, str> {
    if max == 0 {
        return Cow::Borrowed(text);
    }
    match text.char_indices().nth(max) {
        None => Cow::Borrowed(text),
        Some((end, _)) => {
            let cut = text[end..].chars().count();
            Cow::Owned(format!("{}{OMITTED}{cut} chars]", &text[..end]))
        }
    }
}

/// How many leading bytes of `line` are whitespace, counting no further than
/// `limit` and never stopping mid-character.
///
/// The bound is what makes this a *dedent* rather than a trim: context lines cut
/// by the hit's indentation keep whatever nesting they have beyond it.
fn indent_within(line: &str, limit: usize) -> usize {
    line.char_indices()
        .take_while(|(i, c)| c.is_whitespace() && *i < limit)
        .map(|(i, c)| i + c.len_utf8())
        .last()
        .unwrap_or(0)
}

/// A path as it can safely appear in the `path:line:text` contract.
///
/// Returned untouched unless the name would break that contract, which two
/// kinds of character do: a control character ends the record early — a
/// filename containing a newline turned one hit into two output lines, so six
/// files on disk produced seven lines — and a `:` makes the field split
/// ambiguous, so `od:d.py` parses as path `od`, line `d.py`. Both are quoted
/// in git's `core.quotePath` style, and a leading `"` is the consumer's signal
/// to unquote.
///
/// Ordinary paths pass through byte for byte, deliberately. This must not move
/// `tools/snapshot.sh`, and the common case should not get noisier to read to
/// pay for a case almost nobody has. `--json` never arrives here: serde escapes
/// what `println!` does not, which is why it was the one output mode that came
/// through the simulation intact (SIMULATION.md §1.5).
pub fn quote_path(path: &str) -> Cow<'_, str> {
    if !path.chars().any(|c| c.is_control() || c == '"' || c == ':') {
        return Cow::Borrowed(path);
    }
    let mut out = String::with_capacity(path.len() + 2);
    out.push('"');
    for c in path.chars() {
        match c {
            '"' => out.push_str("\\\""),
            // Only meaningful once we are quoting: an unquoted backslash is
            // unambiguous, but inside quotes it has to escape itself or the
            // unquoting is not reversible.
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\{:03o}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    Cow::Owned(out)
}

/// Bytes as a short human string. Decimal units, because that is what disk
/// tooling reports and the numbers are compared against `du`.
pub fn human(bytes: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let (mut v, mut i) = (bytes as f64, 0);
    while v >= 1000.0 && i < U.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    if i == 0 { format!("{bytes} B") } else { format!("{v:.1} {}", U[i]) }
}

/// One result block per hit — or, in exact mode, per file. The only writer of
/// result data, so the output grammar has a single home — and the only place
/// a hit's text is shaped, so ranked mode, exact mode and `--json` cannot drift
/// into three different ideas of how wide a line may be.
///
/// Two shapings, both applied to `text` and neither to `path` or `line`:
/// indentation is stripped, and the rest is clipped to `opts.max_columns`. The
/// text field stops being the file's bytes as a result. That is a real loss and
/// it is the intended trade — `line` still says where to Read for the original,
/// while indentation is the one part of a line that is pure position, already
/// carried by the line number, and reconstructible from the file.
pub fn hits(root: &Path, hits: &[SearchHit], shown: usize, opts: &Print) {
    // `-l`: paths only, deduped, in rank order. grep's meaning exactly, and a
    // cheap answer for a caller that wants to know *where* before it reads.
    if opts.paths_only && !opts.json {
        let mut seen = HashSet::new();
        for hit in &hits[..shown] {
            if seen.insert(hit.path.as_str()) {
                println!("{}", opts.path(&hit.path));
            }
        }
        return;
    }
    // Exact mode's human display: the unit-view grammar, one block per file
    // (`keyword_blocks`). `--json` stays on the flat per-hit loop below — the
    // keyword hit schema is a contract the snapshot and gorp-bench read.
    if opts.exact && !opts.json {
        keyword_blocks(root, &hits[..shown], opts);
        return;
    }
    for hit in &hits[..shown] {
        if opts.json {
            // Shaped here too, rather than left raw for a machine consumer: the
            // 374 KB line costs the same in either format, and a schema whose
            // `text` means something different per flag is the worse contract.
            // Unwrap: SearchHit is plain data with no map keys, so it cannot fail
            // to serialize.
            let mut shaped = hit.clone();
            shaped.text = clip(hit.text.trim_start(), opts.max_columns).into_owned();
            println!("{}", serde_json::to_string(&shaped).expect("SearchHit serializes"));
            continue;
        }
        // A header naming the region and what it declares, before the hit
        // (RESEARCH.md §25.1). `#`-prefixed so a line-oriented consumer can
        // skip it, and printed only when the caller asked for `defines` —
        // stdout is still data, just data with a comment in it.
        if let Some(defs) = hit.defines.as_ref().filter(|d| !d.is_empty()) {
            let span = format!("{}-{}", hit.start_line, hit.end_line);
            println!(
                "# {}:{}  defines: {}",
                opts.path(&hit.path),
                paint(opts.color, palette::LINE, span),
                defs.join(", ")
            );
        }
        // The unit view (RESEARCH.md §34), the ranked default: the path once
        // as a `path:start-end` header, then `line:␣␣text` rows dedented as a
        // BLOCK — the shallowest row reaches the margin and every other row
        // keeps its offset from it, the same rule `frame` applies to `-C`
        // context and for the same reason: per-line trimming flattens nesting
        // into noise. A jump in row numbers prints as a bare `⋮` row rather
        // than a counted marker, because the gutter numbers already state the
        // gap precisely and a marker row that repeats them is a row spent on
        // arithmetic the reader can do. `clip` still applies per row, and a
        // blank source row prints as `N:` with nothing after it — never as a
        // bare blank line, which would end the hit's block early for every
        // consumer that splits records on blank lines.
        if let Some(rows) = hit.unit_rows.as_ref().filter(|r| !r.is_empty()) {
            // `-C/-A/-B` join the unit view as ordinary rows, not a second
            // grammar: a separator or `-`-gutter line would break every
            // consumer that parses the `line:␣␣text` row shape. The header span
            // widens with them, so first/last row == header span stays true,
            // and the added rows join the block dedent below — the same rule
            // `frame` applies to exact-mode context, for the same reason.
            let widened = (opts.before > 0 || opts.after > 0)
                .then(|| widen_rows(root, &hit.path, rows, opts.before, opts.after))
                .flatten();
            let rows = widened.as_deref().unwrap_or(rows);
            let (first, last) = (rows[0].line, rows[rows.len() - 1].line);
            println!(
                "{}:{}",
                opts.path(&hit.path),
                paint(opts.color, palette::LINE, format_args!("{first}-{last}")),
            );
            let dedent = rows
                .iter()
                .filter(|r| !r.text.trim().is_empty())
                .map(|r| r.text.len() - r.text.trim_start().len())
                .min()
                .unwrap_or(0);
            let mut prev: Option<u32> = None;
            for row in rows {
                if prev.is_some_and(|p| row.line > p + 1) {
                    println!("{ELISION}");
                }
                let cut = indent_within(&row.text, dedent);
                // Three roles, one per row, and the engine already decided
                // all three (see [`palette`]): the line it scored best, the
                // window it scored, and everything the unit view added around
                // that window to keep it readable. All three live in the
                // gutter — the text of a row is never restyled for its role.
                let gutter = match row.line {
                    n if n == hit.line => palette::LINE_BEST,
                    n if n < hit.start_line || n > hit.end_line => palette::LINE_CTX,
                    _ => palette::LINE,
                };
                let text = clip(&row.text[cut..], opts.max_columns);
                println!(
                    "{}:{GUTTER}{}",
                    paint(opts.color, gutter, row.line),
                    opts.emphasis.apply(&text, "", opts.color),
                );
                prev = Some(row.line);
            }
            println!();
            continue;
        }
        // The passage: each line in the same `path:line:text` shape, so the
        // block is still parseable line by line rather than being a second
        // format. `clip` still applies per line — a passage multiplies the
        // 374 KB-line problem rather than escaping it, and at 18 lines x 10
        // results the worst case is ~36 KB.
        //
        // Numbered from `lines_from`, NOT `start_line`: the passage is a window
        // cut around the matched line, so it usually begins partway into the
        // chunk. Numbering from the chunk start would misnumber every line of
        // every result, and the line number is the thing a caller navigates by.
        if let Some(body) = hit.lines.as_ref() {
            let mut from = hit.lines_from.unwrap_or(hit.start_line);
            // `-C/-A/-B` extend the passage in its own shape — wider, not
            // reframed — so the block stays parseable line by line.
            let widened = (opts.before > 0 || opts.after > 0)
                .then(|| widen_passage(root, &hit.path, body, from, opts.before, opts.after))
                .flatten();
            let body = match &widened {
                Some((b, f)) => {
                    from = *f;
                    b
                }
                None => body,
            };
            for (i, line) in body.iter().enumerate() {
                let n = from + i as u32;
                let text = clip(line.trim_start(), opts.max_columns);
                let gutter = if n == hit.line {
                    palette::LINE_BEST
                } else if n < hit.start_line || n > hit.end_line {
                    palette::LINE_CTX
                } else {
                    palette::LINE
                };
                let text = opts.emphasis.apply(&text, "", opts.color);
                if opts.with_path {
                    println!(
                        "{}:{}:{text}",
                        opts.path(&hit.path),
                        paint(opts.color, gutter, n),
                    );
                } else {
                    println!("{}:{GUTTER}{text}", paint(opts.color, gutter, n));
                }
            }
            println!();
            continue;
        }
        // The frame is read before the hit line is printed because the amount to
        // dedent by is a property of the whole block, and the hit line is the
        // first line of it out the door.
        let framed = (opts.before > 0 || opts.after > 0)
            .then(|| frame(root, hit, opts.before, opts.after))
            .flatten();
        let dedent = framed
            .as_ref()
            .map_or_else(|| hit.text.len() - hit.text.trim_start().len(), |f| f.dedent);
        let cut = indent_within(&hit.text, dedent);
        let text = clip(&hit.text[cut..], opts.max_columns);
        // The hit's own line is the best line by construction here, so it
        // takes the bold gutter and its text keeps the foreground; the `-C`
        // rows below it are context and step back to grey.
        let text = opts.emphasis.apply(&text, "", opts.color);
        // A `GUTTER` after the line number when the path is suppressed:
        // without a path in front, `264:code` runs the number into the code
        // and the eye has nothing to anchor on. With a path, the compact
        // `p:l:t` form is what every grep consumer already parses, so it is
        // left alone.
        if opts.with_path {
            println!(
                "{}:{}:{text}",
                opts.path(&hit.path),
                paint(opts.color, palette::LINE_BEST, hit.line),
            );
        } else {
            println!("{}:{GUTTER}{text}", paint(opts.color, palette::LINE_BEST, hit.line));
        }
        if let Some(f) = framed {
            for (i, line) in &f.lines {
                let cut = indent_within(line, dedent);
                let text = clip(&line[cut..], opts.max_columns);
                let text = opts.emphasis.apply(&text, "", opts.color);
                if opts.with_path {
                    println!(
                        "{}-{}-{text}",
                        opts.path(&hit.path),
                        paint(opts.color, palette::LINE_CTX, i),
                    );
                } else {
                    println!("{}-{GUTTER}{text}", paint(opts.color, palette::LINE_CTX, i));
                }
            }
            println!("--");
        }
    }
}

/// `-c`: one `path:count` line per file with matches, in the same order the
/// hits themselves would print. Bare count when the caller named exactly one
/// file — grep's rule, same as [`Print::with_path`] — and `-H` restores the
/// path. Under `--json`, one `{"path", "count"}` object per line, so stdout
/// stays machine-parseable in both formats.
pub fn counts(per_file: &[(String, usize)], opts: &Print) {
    for (path, n) in per_file {
        if opts.json {
            println!("{}", serde_json::json!({ "path": path, "count": n }));
        } else if opts.with_path {
            println!("{}:{n}", opts.path(path));
        } else {
            println!("{n}");
        }
    }
}

/// How to render hits. Grouped rather than passed as five positionals: the
/// `path:line:text` contract has one writer, and its options should arrive as
/// one thing.
#[derive(Clone, Copy)]
pub struct Print<'a> {
    pub json: bool,
    pub paths_only: bool,
    pub before: usize,
    pub after: usize,
    /// Characters of each line to print; 0 is no limit.
    pub max_columns: usize,
    /// Prefix each line with its path.
    ///
    /// False only when the caller named exactly one file, which is grep's own
    /// rule: `grep -n p f` prints `12:text` and `grep -n p a b` prints
    /// `a:12:text`, because with one file the path is on every line and tells
    /// the reader nothing they did not just type. `-H` forces it back on, which
    /// is what a caller piping into something that splits on `:` wants.
    pub with_path: bool,
    /// Wrap paths and line numbers in ANSI colour ([`color_enabled`]).
    ///
    /// Decided once by the caller rather than re-probed per line: a search
    /// whose first hit is coloured and whose last is not would be a stranger
    /// bug than no colour at all.
    pub color: bool,
    /// The query's words, emphasised wherever they appear in a result.
    ///
    /// Borrowed rather than owned so `Print` stays `Copy` — it is passed by
    /// reference to every writer here and copied into none of them. Empty
    /// (`&NO_EMPHASIS`) is the honest default: no query to speak of, or a
    /// pattern this cannot read as words.
    pub emphasis: &'a Emphasis,
    /// Exact keyword mode: render one unit-view block per *file*, every
    /// matched line bold, instead of one block per ranked hit. An explicit
    /// flag rather than sniffing a keyword hit's absent fields, because a
    /// ranked hit can legitimately arrive without them too. Set from the
    /// resolved mode, so `-e` and `--mode keyword` print identically.
    pub exact: bool,
}

/// The empty emphasis, for a `Print` built by `..Default::default()`.
static NO_EMPHASIS: Emphasis = Emphasis { terms: Vec::new(), how: Match::Subtoken };

impl Print<'_> {
    /// A path as it prints: quoted if it has to be, painted if colour is on,
    /// and with any query word in it emphasised.
    ///
    /// Path emphasis is not decoration. BM25 counts path tokens as evidence,
    /// so a file can rank because of its *name*, and that is precisely the
    /// case where a reader wonders why a hit is there — the answer belongs
    /// where the question is asked.
    fn path(&self, path: &str) -> String {
        let quoted = quote_path(path);
        self.emphasis.apply(&quoted, palette::PATH, self.color)
    }
}

/// Hand-written rather than derived so that the default width is `MAX_COLUMNS`
/// and not `usize`'s zero, which would read as "no limit" and quietly make a
/// caller that built a `Print` by `..Default::default()` the one unbounded
/// writer in the process.
impl Default for Print<'_> {
    fn default() -> Self {
        Self {
            json: false,
            paths_only: false,
            before: 0,
            after: 0,
            max_columns: MAX_COLUMNS,
            with_path: true,
            // Off, matching every non-terminal consumer: a `Print` built by
            // `..Default::default()` is one in a test or a harness.
            color: false,
            emphasis: &NO_EMPHASIS,
            exact: false,
        }
    }
}

pub fn footer(mode: Mode, result: &SearchResult, shown: usize, suggested: bool) {
    // A floored refusal is a RESULT, not a hint, so it outranks
    // `GORP_NO_HINTS` below. Without this the score floor is silent under
    // every harness that sets that variable: empty stdout, empty stderr,
    // exit 1 — the agent cannot tell "nothing here scored well enough" from
    // "this scope is empty" from a crash. That is the §16.11 shape exactly,
    // and shim.py already carries the scar ("silence that looks like a real
    // answer"). A floor whose signal is suppressed only ever subtracts.
    if result.report.floored {
        // Names no flag, deliberately. The first version ended "or pass
        // --min-score 0 to see weak results", and §30's R1 caught an agent
        // doing exactly that — typing `--min-score` with no value, exiting 2,
        // and spiralling into three consecutive empty searches. That is
        // §16.10 again (naming `-e` in a footer moved ranked share 7% → 98%):
        // a footer is a treatment, and an agent will act on any flag it
        // names. The behaviour this message exists to induce is *rephrasing*,
        // so it says only that. `--min-score` stays discoverable in --help,
        // where a human who wants it will look.
        let best = result.report.best_signal.unwrap_or(0.0);
        eprintln!(
            "{PROG}: no matches · nothing here scored above {best:.2}, which is \
             too weak to be worth reading · this scope may not cover it — try \
             different wording, or a wider scope"
        );
        return;
    }
    // Partial floor on a multi-phrase query (§31): some phrases answered,
    // these did not. Naming the dead branch is the whole point — the agent
    // learns WHICH candidate to abandon, which a bare result list cannot say.
    // Printed above the hint suppression for the same reason the full refusal
    // is: a per-phrase verdict is a result. Verbatim, clipped — the footer
    // echoes what the agent can rephrase, not a normalization it never typed.
    if result.report.floored_mask != 0
        && let Some(phrases) = &result.report.phrases
    {
        for (i, p) in phrases.iter().enumerate() {
            if result.report.floored_mask & (1 << i) != 0 {
                let shown: String = p.chars().take(60).collect();
                eprintln!(
                    "{PROG}: nothing matched '{shown}' strongly enough — that \
                     part of the query may not be covered here"
                );
            }
        }
    }
    // Ranked footers do not name `-e`. They used to, with the caller's own
    // query interpolated, and that one line was the strongest posture lever
    // measured in this project: suppressing it moved an agent's ranked share
    // from 7% to 98% (RESEARCH.md §16.10) — a larger dose than any tool
    // description. Ranked search is the tool; `-e` stays in --help for the
    // caller who goes looking. The keyword-mode footers below still mention
    // it, but only to steer off it, which is the same posture.
    if std::env::var_os("GORP_NO_HINTS").is_some() {
        return;
    }
    let n = result.hits.len();
    if mode == Mode::Keyword {
        // True totals from the scan, not `hits.len()`: retention may hold only
        // the printable head of a huge match set, and the count is the part of
        // `-e`'s answer that must never shrink with it.
        let n = result.report.keyword_total;
        let n_files = result.report.keyword_files;
        let files = if n_files == 1 { "file" } else { "files" };
        if n == 0 {
            if !suggested {
                // Both recovery moves, not just one: a 0-hit -e can mean the
                // path was wrong just as often as the vocabulary was.
                eprintln!(
                    "{PROG}: 0 matches · wrong path? broaden it · searching for a \
                     concept? drop -e and ask in plain language"
                );
            }
            walk_errors_note(result);
        } else if shown < n {
            eprintln!(
                "{PROG}: showing {shown} of {n} matches in {n_files} {files} \
                 (--all for every match) · locating a concept? drop -e and ask in plain language"
            );
        } else {
            eprintln!("{PROG}: {n} matches in {n_files} {files}");
        }
    } else if n == 0 {
        // Why nothing came back, when the reason is structural rather than a
        // vocabulary miss. A ranked search over a scope that has anything to
        // rank essentially cannot return zero — top-k always has k candidates —
        // so "no results" almost always means the corpus was empty, and saying
        // "rephrase" sends the caller to fix the one thing that is not wrong.
        //
        // This is the guard the §16.11 file-scope bug needed and did not have:
        // for the life of that bug the tool walked exactly one file, failed to
        // read it, and reported an ordinary miss. Agents responded by retrying
        // the same query, then reading `--help` — 27 of 27 help probes in the
        // §16.10 campaign followed three or more empty searches (§17). One line
        // here ends that spiral.
        // The floored case never reaches here — it is answered at the top of
        // this function, above the hint suppression.
        let walked = result.report.files_walked;
        if walked == 0 {
            eprintln!("{PROG}: no results · nothing to search under this path");
        } else if result.report.n_chunks_considered == 0 {
            let files = if walked == 1 { "file" } else { "files" };
            eprintln!(
                "{PROG}: no results · found {walked} {files} here but could read none of \
                 them (binary, unreadable, or over the size cap)"
            );
        } else {
            eprintln!("{PROG}: no results · try broader phrasing or a nearby concept");
        }
        walk_errors_note(result);
    } else {
        eprintln!(
            "{PROG}: top {n} of {}",
            result.report.n_chunks_considered.max(n)
        );
    }
}

/// On a zero-hit search whose walk skipped entries it could not read, say so:
/// without this a permission-denied subtree is simply invisible, and "no
/// results" reads as a claim about the code rather than about the readable
/// part of it — the §16.11 shape, silence that looks like a real answer.
fn walk_errors_note(result: &SearchResult) {
    let n = result.report.walk_errors;
    if n > 0 {
        let paths = if n == 1 { "path" } else { "paths" };
        eprintln!(
            "{PROG}: note: {n} {paths} could not be read (permissions?) and were skipped"
        );
    }
}

pub fn stats(mode: Mode, result: &SearchResult) {
    let r = &result.report;
    eprintln!(
        "{PROG}: mode={:?} hits={} index={} hnsw={} chunks={} walk/load={:.1}ms rank={:.1}ms total={:.1}ms{}",
        mode,
        result.hits.len(),
        r.used_index,
        r.used_hnsw,
        r.n_chunks_considered,
        r.walk_ms() + r.load_ms(),
        r.rank_ms(),
        r.total_ms,
        peak_rss_mb().map(|m| format!(" peak_rss={m:.0}MB")).unwrap_or_default(),
    );
    // Zero-valued stages are filtered here and only here: the report carries the
    // whole schedule so machine consumers see a fixed shape, while a human
    // reading a line does not want fifteen `=0.0ms` entries to scan past.
    let line = r
        .stages
        .iter()
        .filter(|s| s.ms > 0.0)
        .map(|s| format!("{}={:.1}ms", s.stage.name(), s.ms))
        .collect::<Vec<_>>()
        .join(" ");
    if !line.is_empty() {
        eprintln!("{PROG}: provenance: {line}");
        // The residual. Printed always, including when it is small, because a
        // number that only appears when it is bad teaches nobody what normal
        // looks like.
        eprintln!(
            "{PROG}: accounted={:.1}ms unattributed={:.1}ms ({:.0}%)",
            r.accounted_ms(),
            r.unattributed_ms(),
            100.0 * r.unattributed_ms() / r.total_ms.max(f64::EPSILON),
        );
    }
    if r.stale_files > 0 {
        eprintln!(
            "{PROG}: warning: {} files changed since indexing (run `gorp index`)",
            r.stale_files
        );
    }
}

/// Ranked `-C/-A/-B` on the unit view: the rows re-read from the file, up to
/// `before` above the first row and `after` below the last, the originals in
/// between. `None` when the file cannot be read (moved since indexing), which
/// falls back to the unwidened rows rather than losing the hit.
fn widen_rows(
    root: &Path,
    path: &str,
    rows: &[UnitRow],
    before: usize,
    after: usize,
) -> Option<Vec<UnitRow>> {
    let text = corpus::read_text(&corpus::resolve(root, path))?;
    let lines: Vec<&str> = text.lines().collect();
    let (first, last) = (rows[0].line as usize, rows[rows.len() - 1].line as usize);
    // The file may have shrunk since the rows were materialized; widening
    // would then attach the wrong lines, so show the rows as they are.
    if first > lines.len() + 1 {
        return None;
    }
    let lo = first.saturating_sub(before).max(1);
    let hi = (last + after).min(lines.len());
    let row = |i: usize| UnitRow { line: i as u32, text: lines[i - 1].to_string() };
    let mut out: Vec<UnitRow> = (lo..first).map(row).collect();
    out.extend(rows.iter().cloned());
    out.extend(((last + 1)..=hi).map(row));
    Some(out)
}

/// Ranked `-C/-A/-B` on a passage (`--no-unit` or a passage override): the
/// body extended in place, plus the line number it now starts from.
fn widen_passage(
    root: &Path,
    path: &str,
    body: &[String],
    from: u32,
    before: usize,
    after: usize,
) -> Option<(Vec<String>, u32)> {
    if body.is_empty() {
        return None;
    }
    let text = corpus::read_text(&corpus::resolve(root, path))?;
    let lines: Vec<&str> = text.lines().collect();
    let first = from as usize;
    let last = first + body.len() - 1;
    // Same shrunk-file guard as `widen_rows`.
    if first > lines.len() + 1 {
        return None;
    }
    let lo = first.saturating_sub(before).max(1);
    let hi = (last + after).min(lines.len());
    let mut out: Vec<String> = (lo..first).map(|i| lines[i - 1].to_string()).collect();
    out.extend(body.iter().cloned());
    out.extend(((last + 1)..=hi).map(|i| lines[i - 1].to_string()));
    Some((out, lo as u32))
}

/// The lines `-C` prints around a hit, and the indentation the block shares.
struct Frame {
    /// Line number and text, in file order, the hit's own line excluded — it is
    /// printed by `hits` in the `path:line:text` form before these arrive.
    lines: Vec<(usize, String)>,
    /// Leading whitespace common to every non-blank line of the block, the hit's
    /// included. Stripping *this* is what makes the result a dedent rather than
    /// a flattening: the shallowest line reaches the margin and every other line
    /// keeps its offset from it, so the nesting `-C` was asked for survives.
    /// Dedenting by the hit's own indentation instead collapses the whole block,
    /// because the matched line is usually the deepest one in it.
    dedent: usize,
}

/// Read the `before`/`after` window around a hit. Returned rather than printed
/// so `hits` can dedent the hit's line by the same amount: the shared
/// indentation is a property of the block, and the hit is part of the block.
fn frame(root: &Path, hit: &SearchHit, before: usize, after: usize) -> Option<Frame> {
    // `resolve`, not `root.join`: a file-as-root records the file's own name as
    // its relative path, so a plain join looks for `<file>/<file>` and reads
    // nothing — context would silently vanish exactly when the scope is a single
    // file (RESEARCH.md §16.11). Same defect as the four engine-side sites,
    // living in the CLI crate where that sweep did not reach.
    let text = corpus::read_text(&corpus::resolve(root, &hit.path))?;
    let lines: Vec<&str> = text.lines().collect();
    let center = hit.line as usize;
    let lo = center.saturating_sub(before).max(1);
    let hi = (center + after).min(lines.len());
    // Blank lines are skipped, not counted as zero-indented: one empty line in
    // the window would otherwise pin the shared indentation to nothing and
    // silently turn the dedent off.
    let dedent = (lo..=hi)
        .map(|i| lines[i - 1])
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);
    let lines =
        (lo..=hi).filter(|i| *i != center).map(|i| (i, lines[i - 1].to_string())).collect();
    Some(Frame { lines, dedent })
}

/// Exact mode's human display: the same header/row/elision grammar as the
/// ranked unit view, grouped one block per FILE rather than per hit — a
/// keyword hit is one line, and a header per line would drown the matches in
/// their own addresses. Every matched line takes the bold gutter (exact mode
/// has no single chosen line; each match is one), and `-C/-A/-B` rows join
/// the block in grey, overlapping windows merged by [`keyword_rows`]. The
/// header always carries the path, even for one named file — the ranked
/// branch's rule, for the same reason: once per block is not per-line noise.
fn keyword_blocks(root: &Path, hits: &[SearchHit], opts: &Print) {
    for file in hits.chunk_by(|a, b| a.path == b.path) {
        let matched: HashSet<u32> = file.iter().map(|h| h.line).collect();
        let rows = keyword_rows(root, file, opts.before, opts.after);
        let (first, last) = (rows[0].line, rows[rows.len() - 1].line);
        println!(
            "{}:{}",
            opts.path(&file[0].path),
            paint(opts.color, palette::LINE, format_args!("{first}-{last}")),
        );
        // Block dedent, elision, and clipping: the same rules as the ranked
        // unit branch, so the two displays cannot drift into two grammars.
        let dedent = rows
            .iter()
            .filter(|r| !r.text.trim().is_empty())
            .map(|r| r.text.len() - r.text.trim_start().len())
            .min()
            .unwrap_or(0);
        let mut prev: Option<u32> = None;
        for row in &rows {
            if prev.is_some_and(|p| row.line > p + 1) {
                println!("{ELISION}");
            }
            let cut = indent_within(&row.text, dedent);
            let gutter = if matched.contains(&row.line) {
                palette::LINE_BEST
            } else {
                palette::LINE_CTX
            };
            let text = clip(&row.text[cut..], opts.max_columns);
            println!(
                "{}:{GUTTER}{}",
                paint(opts.color, gutter, row.line),
                opts.emphasis.apply(&text, "", opts.color),
            );
            prev = Some(row.line);
        }
        println!();
    }
}

/// One file's rows for [`keyword_blocks`]: the matched lines, plus the
/// `-C/-A/-B` window around each, re-read from the file in one read and
/// merged — a set of line numbers, so overlapping windows dedupe and the rows
/// come out sorted. Falls back to the matches' own scanned text when the file
/// cannot be re-read or has shrunk past a match (the `widen_rows` guard, per
/// file), so context can vanish but a match never does.
fn keyword_rows(
    root: &Path,
    file_hits: &[SearchHit],
    before: usize,
    after: usize,
) -> Vec<UnitRow> {
    let bare =
        || file_hits.iter().map(|h| UnitRow { line: h.line, text: h.text.clone() }).collect();
    if before == 0 && after == 0 {
        return bare();
    }
    let Some(text) = corpus::read_text(&corpus::resolve(root, &file_hits[0].path)) else {
        return bare();
    };
    let lines: Vec<&str> = text.lines().collect();
    let mut wanted = std::collections::BTreeSet::new();
    for h in file_hits {
        let lo = (h.line as usize).saturating_sub(before).max(1);
        let hi = (h.line as usize + after).min(lines.len());
        wanted.extend((lo..=hi).map(|i| i as u32));
        wanted.insert(h.line);
    }
    wanted
        .into_iter()
        .map(|n| UnitRow {
            line: n,
            text: lines
                .get(n as usize - 1)
                .map(|l| (*l).to_string())
                .or_else(|| file_hits.iter().find(|h| h.line == n).map(|h| h.text.clone()))
                .unwrap_or_default(),
        })
        .collect()
}

/// One JSON object on stderr. Stderr, not stdout: `--json` owns stdout and the
/// two must compose without corrupting the hit stream.
pub fn stats_json(line: &str) {
    eprintln!("{line}");
}

pub fn peak_rss_mb() -> Option<f64> {
    let mut ru = std::mem::MaybeUninit::<libc::rusage>::uninit();
    let ok = unsafe { libc::getrusage(libc::RUSAGE_SELF, ru.as_mut_ptr()) } == 0;
    if !ok {
        return None;
    }
    let ru = unsafe { ru.assume_init() };
    // macOS reports bytes; Linux reports KiB.
    let bytes = if cfg!(target_os = "macos") {
        ru.ru_maxrss as f64
    } else {
        ru.ru_maxrss as f64 * 1024.0
    };
    Some(bytes / 1e6)
}

#[cfg(test)]
#[path = "out_tests.rs"]
mod tests;
