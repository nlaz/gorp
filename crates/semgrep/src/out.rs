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
/// data, so the `path:line:text` contract has a single home.
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
            // Unwrap: SearchHit is plain data with no map keys, so it cannot fail
            // to serialize.
            println!("{}", serde_json::to_string(hit).expect("SearchHit serializes"));
        } else {
            println!("{}:{}:{}", quote_path(&hit.path), hit.line, hit.text);
            if opts.before > 0 || opts.after > 0 {
                context(root, hit, opts.before, opts.after);
            }
        }
    }
}

/// How to render hits. Grouped rather than passed as five positionals: the
/// `path:line:text` contract has one writer, and its options should arrive as
/// one thing.
#[derive(Default, Clone, Copy)]
pub struct Print {
    pub json: bool,
    pub paths_only: bool,
    pub before: usize,
    pub after: usize,
}

pub fn footer(mode: Mode, result: &SearchResult, shown: usize, suggested: bool) {
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

/// Frame a hit with `n` lines either side. Context lines use `path-line-text`
/// so a consumer can tell a match from its surroundings — grep's convention.
pub fn context(root: &Path, hit: &SearchHit, before: usize, after: usize) {
    // `resolve`, not `root.join`: a file-as-root records the file's own name as
    // its relative path, so a plain join looks for `<file>/<file>` and reads
    // nothing — context would silently vanish exactly when the scope is a single
    // file (RESEARCH.md §16.11). Same defect as the four engine-side sites,
    // living in the CLI crate where that sweep did not reach.
    let Some(text) = corpus::read_text(&corpus::resolve(root, &hit.path)) else { return };
    let lines: Vec<&str> = text.lines().collect();
    let center = hit.line as usize;
    let lo = center.saturating_sub(before).max(1);
    let hi = (center + after).min(lines.len());
    for i in lo..=hi {
        if i != center {
            println!("{}-{}-{}", quote_path(&hit.path), i, lines[i - 1]);
        }
    }
    println!("--");
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
