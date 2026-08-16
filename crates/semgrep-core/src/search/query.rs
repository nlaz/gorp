//! Query parsing: one typed query string into the phrases the ranked paths
//! run, and the interleave that merges their candidate lists back together.
//!
//! Pure text and ordering — no I/O, no scoring. Both ranked paths call
//! `split_phrases` through [`search`](super::search) rather than parsing
//! their own, so a library caller and the CLI cannot disagree about what a
//! pipe means.

use super::hit;

/// The most phrases one query may carry. Bounds the retriever bitmask (`u8`)
/// and the quadratic dedupe; real usage is 2–3 alternates (RESEARCH.md §31).
pub const MAX_PHRASES: usize = 8;

/// Split a ranked query into phrases on `|` and the grep spelling `\|`
/// (RESEARCH.md §31). `||` never splits — in every observed case it was a
/// pasted code line's OR operator, not a separator. Empty parts drop; a query
/// that yields nothing (all pipes) falls back to itself whole, because a
/// worse answer is still better than a panic on `sg "|"`.
///
/// Public and used by the CLI-side never, deliberately: the split happens
/// inside [`search`], so a library caller and the CLI cannot disagree about
/// what a pipe means. Exposed for tests and the eval harness's replay.
pub fn split_phrases(query: &str) -> Vec<String> {
    // No pipe, no parsing: the common case must be byte-preserving, including
    // a legitimate trailing backslash that the splitter below would eat.
    if !query.contains('|') {
        return vec![query.to_string()];
    }
    // Argv strings cannot contain NUL, so it is a safe sentinel.
    const SENTINEL: &str = "\u{0}\u{0}";
    let protected = query.replace("||", SENTINEL);
    let parts: Vec<&str> = protected.split('|').collect();
    let split_happened = parts.len() > 1;
    let mut phrases: Vec<String> = parts
        .into_iter()
        // "a\|b" splits into ["a\", "b"]: the escape rides the left part and
        // is separator syntax, not content. Only stripped when a split
        // actually happened, so a lone "foo\" stays itself.
        .map(|p| if split_happened { p.strip_suffix('\\').unwrap_or(p) } else { p })
        .map(|p| p.replace(SENTINEL, "||"))
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect();
    if phrases.is_empty() {
        phrases.push(query.to_string());
    }
    phrases.truncate(MAX_PHRASES);
    phrases
}

/// A parsed ranked query: the raw string (BM25 tokenization of the whole, the
/// snapshot identity) and its phrases. `phrases.len() == 1` is the promise
/// that everything downstream takes the pre-§31 code path exactly.
pub(crate) struct Query {
    pub raw: String,
    pub phrases: Vec<String>,
}

impl Query {
    pub fn parse(raw: &str) -> Self {
        Self { raw: raw.to_string(), phrases: split_phrases(raw) }
    }

    pub fn is_multi(&self) -> bool {
        self.phrases.len() > 1
    }
}

/// Merge per-phrase candidate lists into one pool: round-robin by per-phrase
/// rank, deduped by chunk id with retriever masks unioned (RESEARCH.md §31).
///
/// Coarse scores min-max normalize *within each phrase's list first*, because
/// they are not comparable across lists — hybrid's RRF scores are pure
/// functions of rank — and both `fine_blend < 1` and the `--no-fine` MMR
/// fallback read `Candidate::score` across the merged pool. Interleaving by
/// rank rather than by score is the same fact from the other side: rank is
/// the only cross-phrase ordering the coarse stage can honestly claim.
pub(crate) fn merge_interleave(mut per_phrase: Vec<Vec<hit::Candidate>>) -> Vec<hit::Candidate> {
    for (p, list) in per_phrase.iter_mut().enumerate() {
        let (lo, hi) = list
            .iter()
            .fold((f32::INFINITY, f32::NEG_INFINITY), |(l, h), c| (l.min(c.score), h.max(c.score)));
        for c in list.iter_mut() {
            c.score = if hi > lo { (c.score - lo) / (hi - lo) } else { 1.0 };
            c.phrases = 1 << p;
        }
    }
    let mut out: Vec<hit::Candidate> = Vec::new();
    let mut seen: std::collections::HashMap<u32, usize> = std::collections::HashMap::new();
    let longest = per_phrase.iter().map(Vec::len).max().unwrap_or(0);
    for rank in 0..longest {
        for list in per_phrase.iter_mut() {
            if rank >= list.len() {
                continue;
            }
            let c = list[rank].clone();
            match seen.get(&c.id) {
                Some(&j) => out[j].phrases |= c.phrases,
                None => {
                    seen.insert(c.id, out.len());
                    out.push(c);
                }
            }
        }
    }
    out
}
