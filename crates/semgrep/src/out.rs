//! Everything that writes to stdout or stderr.
//!
//! One rule, and the CLI tests enforce it: **stdout is data, stderr is
//! commentary.** Results go to stdout in `path:line:text`, parseable by anything
//! that parses grep. Footers, warnings, stats, and suggestions go to stderr, so
//! a pipeline gets clean data and a human still gets told what happened.
//!
//! Printing lives here rather than beside the commands so the commands are about
//! deciding what to show, not how.

use semgrep_core::corpus;
use semgrep_core::rank::Mode;
use semgrep_core::search::{SearchHit, SearchResult};
use std::borrow::Cow;
use std::collections::HashSet;
use std::path::Path;

/// Exact mode prints at most this many matches unless --all is given; the footer
/// reports the true total, so enumeration is never silently lossy.
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

/// Appended to a line that `MAX_COLUMNS` cut. ripgrep's own wording for its
/// `-M/--max-columns`, so a caller that has read one has read both.
const OMITTED: &str = " [... omitted end of long line]";

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
        Some((end, _)) => Cow::Owned(format!("{}{OMITTED}", &text[..end])),
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

/// One result line per hit, plus optional context. The only writer of result
/// data, so the `path:line:text` contract has a single home — and the only place
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
                println!("{}", quote_path(&hit.path));
            }
        }
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
            println!(
                "# {}:{}-{}  defines: {}",
                quote_path(&hit.path),
                hit.start_line,
                hit.end_line,
                defs.join(", ")
            );
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
            for (i, line) in body.iter().enumerate() {
                let n = hit.lines_from.unwrap_or(hit.start_line) + i as u32;
                let text = clip(line.trim_start(), opts.max_columns);
                if opts.with_path {
                    println!("{}:{}:{}", quote_path(&hit.path), n, text);
                } else {
                    println!("{n}:\t{text}");
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
        // A tab after the line number when the path is suppressed: without a
        // path in front, `264:code` runs the number into the code and the eye
        // has nothing to anchor on. With a path, the compact `p:l:t` form is
        // what every grep consumer already parses, so it is left alone.
        if opts.with_path {
            println!("{}:{}:{}", quote_path(&hit.path), hit.line, text);
        } else {
            println!("{}:\t{}", hit.line, text);
        }
        if let Some(f) = framed {
            for (i, line) in &f.lines {
                let cut = indent_within(line, dedent);
                let text = clip(&line[cut..], opts.max_columns);
                if opts.with_path {
                    println!("{}-{}-{}", quote_path(&hit.path), i, text);
                } else {
                    println!("{}-\t{}", i, text);
                }
            }
            println!("--");
        }
    }
}

/// How to render hits. Grouped rather than passed as five positionals: the
/// `path:line:text` contract has one writer, and its options should arrive as
/// one thing.
#[derive(Clone, Copy)]
pub struct Print {
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
}

/// Hand-written rather than derived so that the default width is `MAX_COLUMNS`
/// and not `usize`'s zero, which would read as "no limit" and quietly make a
/// caller that built a `Print` by `..Default::default()` the one unbounded
/// writer in the process.
impl Default for Print {
    fn default() -> Self {
        Self {
            json: false,
            paths_only: false,
            before: 0,
            after: 0,
            max_columns: MAX_COLUMNS,
            with_path: true,
        }
    }
}

pub fn footer(mode: Mode, result: &SearchResult, shown: usize, suggested: bool) {
    // A floored refusal is a RESULT, not a hint, so it outranks
    // `SEMGREP_NO_HINTS` below. Without this the score floor is silent under
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
            "semgrep: no matches · nothing here scored above {best:.2}, which is \
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
                    "semgrep: nothing matched '{shown}' strongly enough — that \
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
    if std::env::var_os("SEMGREP_NO_HINTS").is_some() {
        return;
    }
    let n = result.hits.len();
    if mode == Mode::Keyword {
        let n_files = result.hits.iter().map(|h| h.path.as_str()).collect::<HashSet<_>>().len();
        let files = if n_files == 1 { "file" } else { "files" };
        if n == 0 {
            if !suggested {
                // Both recovery moves, not just one: a 0-hit -e can mean the
                // path was wrong just as often as the vocabulary was.
                eprintln!(
                    "semgrep: 0 matches · wrong path? broaden it · searching for a \
                     concept? drop -e and ask in plain language"
                );
            }
        } else if shown < n {
            eprintln!(
                "semgrep: showing {shown} of {n} matches in {n_files} {files} \
                 (--all for every match) · locating a concept? drop -e and ask in plain language"
            );
        } else {
            eprintln!("semgrep: {n} matches in {n_files} {files}");
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
            eprintln!("semgrep: no results · nothing to search under this path");
        } else if result.report.n_chunks_considered == 0 {
            let files = if walked == 1 { "file" } else { "files" };
            eprintln!(
                "semgrep: no results · found {walked} {files} here but could read none of \
                 them (binary, unreadable, or over the size cap)"
            );
        } else {
            eprintln!("semgrep: no results · try broader phrasing or a nearby concept");
        }
    } else {
        eprintln!(
            "semgrep: ranked top {n} of {} candidates · not it? rephrase the query",
            result.report.n_chunks_considered.max(n)
        );
    }
}

pub fn stats(mode: Mode, result: &SearchResult) {
    let r = &result.report;
    eprintln!(
        "semgrep: mode={:?} hits={} index={} hnsw={} chunks={} walk/load={:.1}ms rank={:.1}ms total={:.1}ms{}",
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
        eprintln!("semgrep: provenance: {line}");
        // The residual. Printed always, including when it is small, because a
        // number that only appears when it is bad teaches nobody what normal
        // looks like.
        eprintln!(
            "semgrep: accounted={:.1}ms unattributed={:.1}ms ({:.0}%)",
            r.accounted_ms(),
            r.unattributed_ms(),
            100.0 * r.unattributed_ms() / r.total_ms.max(f64::EPSILON),
        );
    }
    if r.stale_files > 0 {
        eprintln!(
            "semgrep: warning: {} files changed since indexing (run `semgrep index`)",
            r.stale_files
        );
    }
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

/// One JSON object on stderr. Stderr, not stdout: `--json` owns stdout and the
/// two must compose without corrupting the hit stream.
pub fn stats_json(line: &str) {
    eprintln!("{line}");
}

#[cfg(test)]
mod tests {
    use super::quote_path;

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
