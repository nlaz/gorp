//! Pseudo-relevance feedback (RESEARCH.md §9.3).
//!
//! Take the first lexical pass at its word: whatever it put on top is probably
//! near the target, so mine those chunks for terms that are frequent in them and
//! rare in the corpus, add those to the query, and search again. Query expansion
//! without an LLM in the loop — the question only has to land in the right
//! neighbourhood once, and the neighbourhood's own vocabulary does the rest.
//!
//! Reading the chunk texts is the caller's job; this scores what it is handed,
//! so the rank layer stays free of the filesystem.

use super::bm25::Postings;
use crate::text::token as tokenize;
use std::collections::{HashMap, HashSet};

/// Terms worth adding to `query`, best first, at most `n`.
///
/// A candidate must not already be in the query and must be at least three
/// characters — shorter tokens are almost always structural noise that would
/// swamp the expansion. Ranking is tf across the feedback texts times idf over
/// the corpus, so a term common in the top hits but common everywhere else
/// contributes nothing.
pub fn expansion_terms(
    query: &str,
    texts: &[String],
    store: &impl Postings,
    n: usize,
) -> Vec<String> {
    if n == 0 || texts.is_empty() {
        return Vec::new();
    }
    let in_query: HashSet<String> = tokenize::tokens(query).into_iter().collect();

    let mut tf: HashMap<String, f32> = HashMap::new();
    for text in texts {
        tokenize::for_each_token(text, |tok| {
            if !in_query.contains(tok) && tok.len() >= 3 {
                *tf.entry(tok.to_string()).or_insert(0.0) += 1.0;
            }
        });
    }

    let n_docs = store.n_docs() as f32;
    let mut scored: Vec<(String, f32)> = tf
        .into_iter()
        .map(|(term, freq)| {
            let df = store.df(&term).max(1) as f32;
            let idf = ((n_docs - df + 0.5) / (df + 0.5) + 1.0).ln();
            (term, freq * idf)
        })
        .collect();
    // Ties broken by term so the expansion — and therefore the second pass — is
    // reproducible.
    scored.sort_unstable_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    scored.into_iter().take(n).map(|(term, _)| term).collect()
}

/// The query to run the second pass with.
pub fn expand(query: &str, terms: &[String]) -> String {
    if terms.is_empty() {
        return query.to_string();
    }
    format!("{query} {}", terms.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rank::bm25::Bm25Index;

    fn index(docs: &[&str]) -> Bm25Index {
        let mut i = Bm25Index::new();
        for d in docs {
            i.add_doc(d);
        }
        i.finalize();
        i
    }

    #[test]
    fn picks_terms_frequent_in_feedback_and_rare_in_corpus() {
        // "backoff" appears in one corpus doc; "the" is everywhere.
        let store = index(&[
            "the retry backoff jitter",
            "the queue drains",
            "the pool connects",
            "the handler runs",
        ]);
        let feedback = vec!["the retry backoff jitter backoff".to_string()];
        let terms = expansion_terms("retry", &feedback, &store, 2);
        assert!(terms.contains(&"backoff".to_string()), "got {terms:?}");
        assert!(!terms.contains(&"the".to_string()), "corpus-wide term must not win");
    }

    #[test]
    fn never_re_adds_a_query_term() {
        let store = index(&["alpha beta gamma"]);
        let feedback = vec!["alpha alpha alpha beta".to_string()];
        let terms = expansion_terms("alpha", &feedback, &store, 3);
        assert!(!terms.contains(&"alpha".to_string()), "got {terms:?}");
    }

    #[test]
    fn skips_tokens_shorter_than_three_characters() {
        let store = index(&["fn x of a b"]);
        let feedback = vec!["fn x of a b".to_string()];
        for t in expansion_terms("query", &feedback, &store, 10) {
            assert!(t.len() >= 3, "short token {t:?} should not be an expansion term");
        }
    }

    #[test]
    fn is_a_no_op_when_disabled_or_starved() {
        let store = index(&["alpha beta"]);
        assert!(expansion_terms("alpha", &["beta gamma".into()], &store, 0).is_empty());
        assert!(expansion_terms("alpha", &[], &store, 5).is_empty());
        assert_eq!(expand("alpha", &[]), "alpha");
    }

    #[test]
    fn expansion_is_deterministic_across_equal_scores() {
        let store = index(&["one two three four five six"]);
        let feedback = vec!["seven eight nine".to_string()];
        let first = expansion_terms("query", &feedback, &store, 3);
        for _ in 0..5 {
            assert_eq!(expansion_terms("query", &feedback, &store, 3), first);
        }
    }

    #[test]
    fn expand_appends_terms_to_the_query() {
        assert_eq!(
            expand("retry", &["backoff".into(), "jitter".into()]),
            "retry backoff jitter"
        );
    }
}
