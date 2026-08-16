//! Candidates into printable hits: span overlap, the character budget, and
//! re-reading the chosen line out of the file.
//!
//! The last stage before display, and the only one here that touches the
//! filesystem. Sizing lives with it (`grow_to_budget`, `LINE_OVERHEAD`)
//! because a budget that counts only content does not control output
//! (RESEARCH.md §26.4).

use super::SearchHit;
use super::SearchOptions;
use super::hit::Candidate;
use crate::corpus;
use crate::text::token as tokenize;
use std::collections::HashSet;
use std::path::Path;

/// What one printed line costs beyond its own text: a line number and its
/// separators.
///
/// Charged because a budget that counts only content does not control output.
/// Measured over three corpora at a 600-character content budget, the kernel
/// spent 5,694 bytes a search and Wikipedia 1,826 — the *inverse* of the
/// problem the budget was added to fix, because 30-character C lines buy 20
/// lines of `path:line:` overhead where 180-character prose lines buy 2.
/// Roughly half of real output is this tax, and a content budget is blind to
/// it (RESEARCH.md §26.4).
///
/// A known under-count: when the caller searched a directory the CLI also
/// prefixes each line with its path, which the engine cannot size because it
/// does not know whether the CLI will print one. Under-charging under-fills,
/// which is the safe direction.
const LINE_OVERHEAD: u32 = 12;

/// Grow a window outward from `at` until the next line would exceed `budget`
/// characters, and return its inclusive bounds.
///
/// Line-aligned because half a line of code is not a thing worth showing, and
/// symmetric because §26.1 measured a forward bias and it loses coverage. The
/// matched line is always included even when it alone exceeds the budget: a
/// hit that returns nothing is worse than a hit that returns one long line,
/// and `max_columns` already bounds how long that line can print.
///
/// Costs each line by its length in the *file*, not by what will be printed.
/// The CLI clips long lines separately (`out::MAX_COLUMNS`), so a budget spent
/// on a 5,000-character minified line buys one line here and prints ~200
/// characters. That under-fills rather than over-fills, which is the safe
/// direction: the two caps compose to a bound, never to a surprise. Keeping
/// the print width out of the engine also keeps one display concern in one
/// place instead of two that can disagree.
fn grow_to_budget(lines: &[String], at: usize, budget: u32) -> (usize, usize) {
    let cost = |s: &String| s.chars().count() as u32 + LINE_OVERHEAD;
    let (mut lo, mut hi) = (at, at);
    let mut used = cost(&lines[at]);
    loop {
        let mut grew = false;
        // After first, then before, so an odd character of slack lands below
        // the match — the same asymmetry the line budget uses.
        if hi + 1 < lines.len() && used + cost(&lines[hi + 1]) <= budget {
            hi += 1;
            used += cost(&lines[hi]);
            grew = true;
        }
        if lo > 0 && used + cost(&lines[lo - 1]) <= budget {
            lo -= 1;
            used += cost(&lines[lo]);
            grew = true;
        }
        if !grew {
            break;
        }
    }
    (lo, hi)
}

/// Do two same-file chunks overlap by at least `frac` of the shorter span?
///
/// `frac <= 0` reproduces the original rule exactly — any shared line at all is
/// a duplicate — so the pre-§24 behaviour stays reachable as a control arm.
pub(super) fn overlaps(a: &Candidate, b: &Candidate, frac: f32) -> bool {
    let lo = a.chunk.start_line.max(b.chunk.start_line);
    let hi = a.chunk.end_line.min(b.chunk.end_line);
    if lo > hi {
        return false;
    }
    let shared = (hi - lo + 1) as f32;
    if frac <= 0.0 {
        return true;
    }
    let span = |c: &Candidate| (c.chunk.end_line - c.chunk.start_line + 1) as f32;
    shared >= frac * span(a).min(span(b))
}

/// Turn a ranked chunk into a displayable hit: re-read the file and pick the
/// line with the highest query-token overlap (first non-empty line as
/// fallback). Skips chunks whose file vanished since ranking.
///
/// When the fine rerank chose a window, the hit's span *is* that window and
/// the best-line search runs inside it — the agent-facing span must be the
/// tight one (RESEARCH.md §28.2). The whole chunk is still read and collected,
/// because a caller who explicitly asked for a wider passage
/// (`passage_override`) gets it cut from the chunk, anchored at the window.
pub(super) fn materialize(
    root: &Path,
    c: &Candidate,
    query_tokens: &HashSet<String>,
    opts: &SearchOptions,
) -> Option<SearchHit> {
    let chunk = c.chunk;
    let text = corpus::read_text(&corpus::resolve(root, &c.path))?;
    // The span the best-line argmax runs over: the fine window when one was
    // chosen, else the whole chunk.
    let (span_start, span_end) = match &c.fine {
        Some(f) => (f.start_line, f.end_line),
        None => (chunk.start_line, chunk.end_line),
    };
    // Whether display shows exactly the fine window (the default with fine on)
    // or a cut of the chunk (no fine, or an explicit passage request).
    let window_is_passage = c.fine.is_some() && !opts.passage_override;
    // Both display extras are collected in this loop rather than by re-reading
    // the file in the CLI: the chunk's text is already in hand exactly once
    // here, and a second reader would be a second chance to disagree about
    // which lines a chunk covers.
    // Every line of the chunk is collected even when only a window will be
    // shown: the window is centred on the best-matching line, and which line
    // that is only becomes known once the loop below has finished.
    let want_lines = window_is_passage
        || opts.passage_lines > 1
        || (opts.passage_lines == 0 && opts.passage_chars > 0);
    let mut lines: Option<Vec<String>> = want_lines.then(Vec::new);
    let mut defines: Option<Vec<String>> = opts.defines.then(Vec::new);
    // Ranked by (query-token overlap, carries a word at all), first line
    // winning ties. The second term exists because the fine rerank made the
    // first one tie far more often: over a 32-line chunk some line almost
    // always shared a token with the query, but inside a 4-line window the
    // overlap is frequently zero everywhere, and the old first-wins fallback
    // then anchored the hit on whatever led the window — a bare `{` or `)`
    // in 8.3% of recorded snapshot hits. Overlap still dominates, so a line
    // that genuinely matches is never passed over for a prettier one.
    let mut best: Option<((usize, bool), u32, &str)> = None;
    for (i, line) in text.lines().enumerate() {
        let line_no = i as u32 + 1;
        if line_no < chunk.start_line {
            continue;
        }
        if line_no > chunk.end_line {
            break;
        }
        if let Some(v) = lines.as_mut() {
            v.push(line.to_string());
        }
        if let Some(v) = defines.as_mut() {
            v.extend(crate::text::declared_names(line));
        }
        if line.trim().is_empty() || line_no < span_start || line_no > span_end {
            continue;
        }
        let mut overlap = 0usize;
        tokenize::for_each_token(line, |tok| {
            if query_tokens.contains(tok) {
                overlap += 1;
            }
        });
        let rank = (overlap, line.chars().any(|c| c.is_alphanumeric()));
        match &best {
            Some((b, _, _)) if *b >= rank => {}
            _ => best = Some((rank, line_no, line)),
        }
    }
    let (_, line, line_text) = best?;
    // Cut the collected chunk down to the passage, and report where the cut
    // starts. `out.rs` numbers printed lines from `lines_from`, not from
    // `start_line` — without that the whole passage is misnumbered, which is
    // worse than showing nothing, since the line number is what the caller
    // navigates by.
    let (lines, lines_from) = match lines {
        None => (None, None),
        Some(all) => {
            let first = chunk.start_line;
            let (lo, hi) = if window_is_passage {
                ((span_start - first) as usize, (span_end - first) as usize)
            } else if opts.passage_lines > 0 {
                // Legacy line budget, kept so §26's campaign arms reproduce
                // under their own flag. 8 before / 9 after at 18: measured,
                // not chosen — a stronger forward bias loses coverage (§26.1).
                let at = (line - first) as usize;
                let before = ((opts.passage_lines - 1) / 2) as usize;
                let lo = at.saturating_sub(before);
                (lo, (lo + opts.passage_lines as usize - 1).min(all.len() - 1))
            } else {
                let at = (line - first) as usize;
                grow_to_budget(&all, at, opts.passage_chars)
            };
            let hi = hi.min(all.len().saturating_sub(1));
            let lo = lo.min(hi);
            let cut: Vec<String> = all[lo..=hi].to_vec();
            (Some(cut), Some(first + lo as u32))
        }
    };
    let (start_line, end_line) = match &c.fine {
        Some(f) => (f.start_line, f.end_line),
        None => (chunk.start_line, chunk.end_line),
    };
    // The unit view (RESEARCH.md §34), computed here and not in the CLI for
    // the same one-reader reason as `lines`/`defines` above: the whole file
    // is already in hand. Three gates, each keeping a measured surface
    // byte-identical: `passage_override` (an asked-for passage shape wins,
    // which also pins the snapshot's `--passage-lines 1` recording),
    // `fine.is_some()` (`--no-fine` documents itself as pre-§28.2 output
    // byte for byte), and the option itself (`--no-unit`, the A/B control).
    let unit_rows = (opts.unit_view && !opts.passage_override && c.fine.is_some()).then(|| {
        let all: Vec<&str> = text.lines().collect();
        super::unit::compute(&all, &c.path, start_line, end_line)
    });
    Some(SearchHit {
        path: c.path.clone(),
        start_line,
        end_line,
        line,
        text: line_text.to_string(),
        score: c.fine.map_or(c.score, |f| f.score),
        chunk_start_line: c.fine.map(|_| chunk.start_line),
        chunk_end_line: c.fine.map(|_| chunk.end_line),
        // The caller (finalize) overwrites this for a multi-phrase query; a
        // single-phrase hit stays None so its JSON is unchanged.
        phrase: None,
        lines,
        lines_from,
        // Dedupe late rather than while collecting: a name declared twice in
        // one window says nothing a header should repeat, and the order the
        // file declares them in is the order worth showing.
        defines: defines.map(|mut v| {
            let mut seen = HashSet::new();
            v.retain(|n| seen.insert(n.clone()));
            v
        }),
        unit_rows,
        features: opts.debug_features.then(|| super::HitFeatures {
            coarse: c.score,
            fine: c.fine.map(|f| f.score),
            bm25_rank: c.bm25_rank,
            phrases: c.phrases,
            decl_share: c.decl_share,
            path_share: c.path_share,
            chunk_lines: chunk.end_line.saturating_sub(chunk.start_line) + 1,
        }),
    })
}
