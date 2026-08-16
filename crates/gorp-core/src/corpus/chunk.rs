//! Cutting file text into chunks, and reading a chunk's lines back.

use super::read_text;
use crate::{Chunk, ChunkParams};
use std::path::Path;

/// Split file text into chunks, yielding the chunk record and its text slice
/// bounds so callers can avoid copying. Line windows by default; a character
/// budget under [`ChunkParams::budget`]; definition-boundary cuts under
/// [`ChunkParams::function`], which wins over both and falls back to windows
/// for anything a grammar cannot cut (RESEARCH.md §11, §29).
///
/// `rel_path` exists for the function mode's language dispatch and is
/// otherwise unused; it is threaded from every caller regardless so all three
/// paths that cut — build, cold search, read-repair — stay one function of
/// the same inputs.
pub fn chunk_lines<'a>(
    file_id: u32,
    rel_path: &str,
    text: &'a str,
    params: &ChunkParams,
) -> Vec<(Chunk, &'a str)> {
    let mut line_starts: Vec<usize> = vec![0];
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            line_starts.push(i + 1);
        }
    }
    // Trailing entry marks EOF so slicing line i..j is uniform.
    if *line_starts.last().unwrap() != text.len() {
        line_starts.push(text.len());
    }
    let n_lines = line_starts.len() - 1;
    if n_lines == 0 {
        return Vec::new();
    }

    #[cfg(feature = "func-chunk")]
    if let Some(cap) = params.function
        && let Some(ranges) =
            super::funcchunk::cut(rel_path, text, n_lines, cap.max(1), params.window)
    {
        return ranges
            .into_iter()
            .filter_map(|(s, e)| {
                let slice = &text[line_starts[s]..line_starts[e]];
                (!slice.trim().is_empty()).then(|| {
                    (Chunk { file_id, start_line: s as u32 + 1, end_line: e as u32 }, slice)
                })
            })
            .collect();
    }
    #[cfg(not(feature = "func-chunk"))]
    let _ = rel_path;

    if let Some(budget) = params.budget.filter(|_| params.function.is_none()) {
        return chunk_budgeted(file_id, text, &line_starts, n_lines, params, budget);
    }

    let window = params.window.max(1) as usize;
    let stride = window.saturating_sub(params.overlap as usize).max(1);
    let mut chunks = Vec::new();
    let mut start = 0usize;
    loop {
        let end = (start + window).min(n_lines);
        let slice = &text[line_starts[start]..line_starts[end]];
        if !slice.trim().is_empty() {
            chunks.push((
                Chunk { file_id, start_line: start as u32 + 1, end_line: end as u32 },
                slice,
            ));
        }
        if end == n_lines {
            break;
        }
        start += stride;
    }
    chunks
}

/// Line-aligned windows cut to a non-whitespace character budget.
///
/// Overlap is carried over as a *fraction* — `overlap/window`, 25% at the
/// defaults — rather than as a line count, so `--chunk-budget 800` is a
/// reparameterization of the shipped chunking rather than a second, silently
/// different overlap policy.
///
/// One line may exceed the budget on its own (minified sources, generated
/// tables). That line still becomes a chunk: the alternative is splitting
/// mid-line, and `Chunk` cannot express it.
fn chunk_budgeted<'a>(
    file_id: u32,
    text: &'a str,
    line_starts: &[usize],
    n_lines: usize,
    params: &ChunkParams,
    budget: u32,
) -> Vec<(Chunk, &'a str)> {
    let budget = budget.max(1) as usize;
    let overlap = (budget as u64 * params.overlap as u64
        / u64::from(params.window.max(1))) as usize;

    // Cumulative non-whitespace characters through the end of each line, so a
    // window's content cost is one subtraction and its end one binary search.
    let mut cum: Vec<usize> = Vec::with_capacity(n_lines + 1);
    cum.push(0);
    for i in 0..n_lines {
        let line = &text[line_starts[i]..line_starts[i + 1]];
        let n = line.chars().filter(|c| !c.is_whitespace()).count();
        cum.push(cum[i] + n);
    }

    let mut chunks = Vec::new();
    let mut start = 0usize;
    loop {
        // First line index whose cumulative count reaches the budget. Clamped
        // to at least one line so a run of blank lines cannot stall.
        let target = cum[start] + budget;
        let end = cum.partition_point(|&c| c < target).clamp(start + 1, n_lines);
        let slice = &text[line_starts[start]..line_starts[end]];
        if !slice.trim().is_empty() {
            chunks.push((
                Chunk { file_id, start_line: start as u32 + 1, end_line: end as u32 },
                slice,
            ));
        }
        if end == n_lines {
            break;
        }
        // Step back far enough to leave `overlap` characters behind.
        let keep = cum[end].saturating_sub(overlap);
        let next = cum.partition_point(|&c| c < keep);
        start = next.clamp(start + 1, end);
    }
    chunks
}

/// Re-read a chunk's lines from disk, by root-relative path.
///
/// Chunk text is never stored, so anything that needs the body — displaying a
/// hit, mining PRF terms, scoring MaxSim — comes back here. Returns `None` if
/// the file has become unreadable or vanished since it was indexed, which is
/// normal on a tree that moves under you.
pub fn lines(root: &Path, rel_path: &str, chunk: &Chunk) -> Option<String> {
    let text = read_text(&super::resolve(root, rel_path))?;
    let mut out = String::new();
    for (i, line) in text.lines().enumerate() {
        let line_no = i as u32 + 1;
        if line_no > chunk.end_line {
            break;
        }
        if line_no >= chunk.start_line {
            out.push_str(line);
            out.push('\n');
        }
    }
    Some(out)
}
