//! Bridge-file query expansion (RESEARCH.md §33).
//!
//! The vocabulary-gap failure (§32.4a): the agent describes behavior, the
//! gold implements it, and the two share no words — but some *bridge file*
//! (routes, registration, config, tests) contains both vocabularies, because
//! wiring them together is that file's job. This module finds the files that
//! best cover the query's tokens and mines THEIR distinctive vocabulary as
//! expansion terms.
//!
//! Deliberately not PRF (§9.3, measured 0/86 on the vocabulary-gap set):
//! PRF mines the *ranked top hits*, which on a gapped query are the wrong
//! files, and drifts toward them. Coverage scoring is rank-free — the bridge
//! wins by containing the query's rare tokens together, whether or not any
//! ranking liked it.
//!
//! Reading file texts is the caller's job (same contract as `prf`), so the
//! rank layer stays free of the filesystem.

use super::bm25::Postings;
use crate::text::token as tokenize;
use std::collections::{HashMap, HashSet};

/// How many bridge files are mined. Five matched the prototype; more adds
/// noise faster than coverage (the committee-agreement rule below wants a
/// small committee, not a crowd).
const N_BRIDGES: usize = 5;

/// A token present in more than this share of chunks scores no coverage and
/// no candidate survives it — stopword guard, both directions. The absolute
/// floor keeps tiny scopes honest: 20% of a 30-chunk directory is 6, and a
/// token in 6 of 30 chunks is signal there, not a stopword.
const DF_CEILING: f64 = 0.2;
const DF_CEILING_MIN: usize = 8;

fn df_ceiling(n_docs: usize) -> usize {
    ((n_docs as f64 * DF_CEILING) as usize).max(DF_CEILING_MIN)
}

/// Files (by path) that best cover `query`'s tokens, best first: per file,
/// the sum of idf over the *distinct* query tokens it contains, requiring
/// at least two — one shared rare word is a coincidence, two is wiring.
/// `allow` restricts which chunks may vouch for a file (the warm path's
/// subtree scope; the cold path walks its scope and passes `None`).
/// Deterministic: score desc, then path asc.
fn bridge_files(
    qtokens: &[String],
    store: &impl Postings,
    path_of_chunk: &impl Fn(u32) -> String,
    allow: Option<&dyn Fn(u32) -> bool>,
) -> Vec<String> {
    let n_docs = store.n_docs().max(1);
    let ceiling = df_ceiling(n_docs);
    let mut cover: HashMap<String, (f64, u32)> = HashMap::new();
    for tok in qtokens {
        let Some(tid) = store.term_id(tok) else { continue };
        let df = store.df(tok);
        if df == 0 || df > ceiling {
            continue;
        }
        let idf = ((n_docs as f64) / (df as f64)).ln();
        let mut seen: HashSet<String> = HashSet::new();
        for (chunk_id, _) in store.postings(tid) {
            if allow.is_some_and(|f| !f(chunk_id)) {
                continue;
            }
            seen.insert(path_of_chunk(chunk_id));
        }
        for p in seen {
            let e = cover.entry(p).or_insert((0.0, 0));
            e.0 += idf;
            e.1 += 1;
        }
    }
    let mut scored: Vec<(f64, String)> = cover
        .into_iter()
        .filter(|(_, (_, n))| *n >= 2)
        .map(|(p, (s, _))| (s, p))
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.into_iter().take(N_BRIDGES).map(|(_, p)| p).collect()
}

/// Expansion terms mined from bridge texts, best first, at most `n`. A term
/// must appear in at least two bridge files (committee agreement — one
/// file's fixtures don't get to speak for the repo), must not be in the
/// query, must be >= 3 chars, and must be rarer than the df ceiling.
/// Score = (#bridges containing it) * ln(n_docs / df); ties by term string
/// so the expansion is reproducible.
pub fn terms_from_texts(
    query: &str,
    bridge_texts: &[String],
    store: &impl Postings,
    n: usize,
) -> Vec<String> {
    if n == 0 || bridge_texts.len() < 2 {
        return Vec::new();
    }
    let in_query: HashSet<String> = tokenize::tokens(query).into_iter().collect();
    let n_docs = store.n_docs().max(1);
    let ceiling = df_ceiling(n_docs);

    let mut presence: HashMap<String, u32> = HashMap::new();
    for text in bridge_texts {
        let mut seen: HashSet<String> = HashSet::new();
        tokenize::for_each_token(text, |tok| {
            if tok.len() >= 3 && !in_query.contains(tok) && !seen.contains(tok) {
                seen.insert(tok.to_string());
                *presence.entry(tok.to_string()).or_insert(0) += 1;
            }
        });
    }
    let mut scored: Vec<(String, f64)> = presence
        .into_iter()
        .filter(|(_, c)| *c >= 2)
        .filter_map(|(term, c)| {
            let df = store.df(&term);
            if df == 0 || df > ceiling {
                return None;
            }
            Some((term, c as f64 * ((n_docs as f64) / (df as f64)).ln()))
        })
        .collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    scored.into_iter().take(n).map(|(t, _)| t).collect()
}

/// The full pipeline: pick bridges, read them, mine them. A file that
/// cannot be read simply drops out of the committee.
pub fn bridge_terms(
    query: &str,
    store: &impl Postings,
    path_of_chunk: impl Fn(u32) -> String,
    allow: Option<&dyn Fn(u32) -> bool>,
    read: impl Fn(&str) -> Option<String>,
    n: usize,
) -> Vec<String> {
    if n == 0 {
        return Vec::new();
    }
    let qtokens: Vec<String> = {
        let mut seen = HashSet::new();
        tokenize::tokens(query)
            .into_iter()
            .filter(|t| seen.insert(t.clone()))
            .collect()
    };
    if qtokens.len() < 2 {
        return Vec::new(); // a bridge covers >= 2 tokens; one token has none
    }
    let files = bridge_files(&qtokens, store, &path_of_chunk, allow);
    let texts: Vec<String> = files.iter().filter_map(|p| read(p)).collect();
    terms_from_texts(query, &texts, store, n)
}

/// Original query tokens at full weight (with multiplicity, matching what
/// re-tokenizing the string would do) plus expansion terms at `weight`,
/// token-sorted for [`super::top_k_weighted`]'s determinism contract.
pub fn weighted_terms(query: &str, terms: &[String], weight: f32) -> Vec<(String, f32)> {
    let mut out: HashMap<String, f32> = HashMap::new();
    tokenize::for_each_token(query, |tok| {
        *out.entry(tok.to_string()).or_insert(0.0) += 1.0;
    });
    for t in terms {
        *out.entry(t.clone()).or_insert(0.0) += weight;
    }
    let mut v: Vec<(String, f32)> = out.into_iter().collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rank::bm25::Bm25Index;

    /// One chunk per file, so chunk id == file index.
    fn store(docs: &[&str]) -> Bm25Index {
        let mut b = Bm25Index::default();
        for d in docs {
            b.add_doc(d);
        }
        b.finalize();
        b
    }

    #[test]
    fn bridges_mine_the_wiring_file_not_the_ranked_head() {
        // Two bridge files contain BOTH query tokens plus the gold's
        // identifier; the gold contains only its own identifier; a distractor
        // contains one query token amid noise.
        let docs = [
            "zebraq quokkaz hiddenfn route setup",            // bridge 1
            "zebraq quokkaz hiddenfn registry wiring",        // bridge 2
            "hiddenfn implementation detail work",            // gold
            "zebraq unrelated noise elsewhere padding",       // distractor
            "totally different content here",                 // filler
        ];
        let s = store(&docs);
        let terms = bridge_terms(
            "zebraq quokkaz",
            &s,
            |cid| format!("f{cid}"),
            None,
            |p| {
                let i: usize = p[1..].parse().unwrap();
                Some(docs[i].to_string())
            },
            4,
        );
        assert!(
            terms.contains(&"hiddenfn".to_string()),
            "the identifier both bridges agree on must be mined (got {terms:?})",
        );
        // determinism: same call, same answer
        let again = bridge_terms(
            "zebraq quokkaz",
            &s,
            |cid| format!("f{cid}"),
            None,
            |p| {
                let i: usize = p[1..].parse().unwrap();
                Some(docs[i].to_string())
            },
            4,
        );
        assert_eq!(terms, again);
    }

    #[test]
    fn single_token_queries_and_lone_bridges_expand_to_nothing() {
        let docs = ["zebraq hiddenfn only one file has this"];
        let s = store(&docs);
        let one_token = bridge_terms("zebraq", &s, |c| format!("f{c}"), None,
                                     |_| Some(docs[0].to_string()), 4);
        assert!(one_token.is_empty(), "one token cannot be wired to anything");
        let one_bridge = bridge_terms("zebraq hiddenfn", &s, |c| format!("f{c}"), None,
                                      |_| Some(docs[0].to_string()), 4);
        assert!(one_bridge.is_empty(), "a lone bridge has no committee");
    }

    #[test]
    fn weighted_terms_keep_original_multiplicity_and_sort_by_token() {
        let w = weighted_terms("alpha alpha beta", &["gamma".into()], 0.4);
        assert_eq!(
            w,
            vec![("alpha".to_string(), 2.0), ("beta".to_string(), 1.0),
                 ("gamma".to_string(), 0.4)],
        );
    }
}
