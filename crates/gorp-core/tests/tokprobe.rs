//! §9.8 forensics: why MaxSim reranking failed on code (kept as documentation
//! of the mechanism — if these assertions ever flip, revisit --maxsim).
use gorp_core::rank::maxsim;
use gorp_core::text::token_vectors;

#[test]
fn identifiers_are_shredded_by_the_tokenizer() {
    let mut toks = Vec::new();
    ese::for_each_token("scalar_None", |t| toks.push(t.to_string()));
    // The identifier does not survive as a matchable unit.
    assert_eq!(toks, ["scalar", "_", "none"]);
}

#[test]
fn per_token_argmax_shows_the_bait_mechanism() {
    let q = "scalar_None function shortcut";
    let gold = "def scalar_None(obj): return obj is None";
    let bait = "regridder_function: Optional[str], if min is None and max is None:";

    let mut qt = Vec::new();
    ese::for_each_token(q, |t| qt.push(t.to_string()));
    let qv = token_vectors(q, None);
    for (doc_name, doc) in [("GOLD", gold), ("BAIT", bait)] {
        let mut dt = Vec::new();
        ese::for_each_token(doc, |t| dt.push(t.to_string()));
        let dv = token_vectors(doc, None);
        println!("-- query tokens vs {doc_name} --");
        for (qtok, (_, qvec)) in qt.iter().zip(&qv) {
            let (mut best, mut best_tok) = (f32::NEG_INFINITY, "");
            for (dtok, (_, dvec)) in dt.iter().zip(&dv) {
                let sim: f32 = qvec.iter().zip(dvec.iter()).map(|(a, b)| a * b).sum();
                if sim > best {
                    best = sim;
                    best_tok = dtok;
                }
            }
            println!("  {qtok:>10} -> {best_tok:>10}  sim={best:.3}");
        }
    }
    let (g, b) = (
        maxsim(&token_vectors(q, None), &token_vectors(gold, None)),
        maxsim(&token_vectors(q, None), &token_vectors(bait, None)),
    );
    println!("total: gold={g:.3} bait={b:.3}");
    // The documented failure: the unrelated chunk outscores the definition.
    assert!(b > g, "bait no longer outscores gold — revisit §9.8");
}
