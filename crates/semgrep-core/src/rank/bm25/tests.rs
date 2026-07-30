//! Scorer and in-memory store tests.

use super::*;

fn idx(docs: &[&str]) -> Bm25Index {
    let mut i = Bm25Index::new();
    for d in docs {
        i.add_doc(d);
    }
    i.finalize();
    i
}

#[test]
fn ranks_matching_doc_first() {
    let i = idx(&[
        "the cat sat on the mat",
        "dogs chase cats in the yard",
        "quantum entanglement of photons",
    ]);
    let hits = i.query("cat mat", 10);
    assert_eq!(hits[0].0, 0);
    assert!(hits[0].1 > hits.get(1).map(|h| h.1).unwrap_or(0.0));
}

#[test]
fn idf_prefers_rare_terms() {
    let i = idx(&["alpha beta beta beta", "alpha gamma", "alpha delta"]);
    // "gamma" is rarer than "alpha"; doc 1 should win a mixed query.
    let hits = i.query("alpha gamma", 10);
    assert_eq!(hits[0].0, 1);
}

#[test]
fn code_identifiers_match_nl_query() {
    let i = idx(&[
        "fn parse_config_file(path: &Path) -> Config",
        "fn render_frame(buf: &mut Buffer)",
    ]);
    let hits = i.query("parse the config file", 10);
    assert_eq!(hits[0].0, 0);
}

#[test]
fn roundtrips_through_postcard() {
    let i = idx(&["hello world", "goodbye world"]);
    let bytes = postcard::to_allocvec(&i).unwrap();
    let j: Bm25Index = postcard::from_bytes(&bytes).unwrap();
    assert_eq!(j.query("hello", 5)[0].0, 0);
    assert_eq!(j.n_docs(), 2);
}

/// Scores must be bit-identical across queries built in a different token
/// order. They were not: accumulation followed `HashMap` iteration order,
/// which is seeded per process, so near-tied chunks swapped rank at random
/// between runs and no output could be snapshot-compared.
#[test]
fn scores_do_not_depend_on_term_order() {
    let i = idx(&[
        "alpha beta gamma delta epsilon",
        "alpha alpha beta zeta",
        "gamma delta delta eta theta",
        "epsilon zeta eta alpha beta gamma",
    ]);
    let baseline = i.query("alpha beta gamma delta epsilon zeta eta", 10);
    for permuted in [
        "eta zeta epsilon delta gamma beta alpha",
        "gamma alpha eta beta delta zeta epsilon",
        "delta epsilon alpha eta beta gamma zeta",
    ] {
        let got = i.query(permuted, 10);
        assert_eq!(got, baseline, "query {permuted:?} scored differently");
    }
}

#[test]
fn empty_query_and_empty_index() {
    let i = idx(&["something"]);
    assert!(i.query("зздщ", 5).is_empty());
    let e = Bm25Index::new();
    assert!(e.query("anything", 5).is_empty());
}
