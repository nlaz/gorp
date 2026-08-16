//! Tests for [`super::prose`].

use super::*;

const PATH: &str = "src/vs/workbench/contrib/searchEditor/browser/searchEditorActions.ts";
const BODY: &str = "export function computeBackoffDelay(attempt: number): number {\n  \
                    const jitter = Math.random() * BASE_DELAY_MS;\n  \
                    return Math.min(MAX_DELAY_MS, 2 ** attempt * jitter);\n}";

fn doc() -> String {
    crate::corpus::doc_text(PATH, BODY)
}

fn render(p: EmbedPreproc) -> String {
    let d = doc();
    render_doc(&d, p, PathRender::Full).into_owned()
}

#[test]
fn none_is_the_identity_and_borrows() {
    let s = "fn get_user_name(&self) -> String";
    assert!(matches!(render_doc(s, EmbedPreproc::None, PathRender::Full), Cow::Borrowed(_)));
}

#[test]
fn split_renders_identifiers_as_prose() {
    let s = "fn get_user_name(&self) { retry_backoff += 1; }";
    assert_eq!(render_body(s, EmbedPreproc::Split), "fn get user name self retry backoff");
}

#[test]
fn split_whole_keeps_the_identifier_too() {
    assert_eq!(
        render_body("getUserName", EmbedPreproc::SplitWhole),
        "get user name getusername"
    );
    // Undecomposed words carry no duplicate.
    assert_eq!(render_body("backoff", EmbedPreproc::SplitWhole), "backoff");
}

#[test]
fn nokw_drops_keywords_and_numbers_but_not_content() {
    let s = "def compute_delay(retries): return delay * 250";
    assert_eq!(render_body(s, EmbedPreproc::SplitNokw), "compute delay retries delay");
}

#[test]
fn as_str_round_trips_and_matches_the_serde_spelling() {
    // meta.json stores the serde (kebab) form and the harness compares it
    // against the flag string, so a variant whose as_str disagrees with its
    // serde name builds an index the readback assertion then rejects.
    for name in EmbedPreproc::ALL {
        let v = EmbedPreproc::parse(name).unwrap_or_else(|| panic!("{name}"));
        assert_eq!(v.as_str(), *name);
        let json = serde_json::to_string(&v).unwrap();
        assert_eq!(json, format!("\"{name}\""), "serde disagrees for {name}");
    }
}

#[test]
fn the_frozen_table_really_was_missing_function_and_export() {
    // The finding that motivated §20. If this ever fails, someone edited a
    // published condition and §14.4's numbers no longer describe it.
    assert!(!is_keyword("function"));
    assert!(!is_keyword("export"));
    assert!(is_keyword_extra("function"));
    assert!(is_keyword_extra("export"));
}

#[test]
fn punctuation_noise_never_survives() {
    // §9.8: `_` matched anything with cosine 1.0. It must not exist here.
    let r = render_body("a[_x] = {_y: *_z}; // _", EmbedPreproc::Split);
    assert!(!r.contains('_'), "{r:?}");
    assert!(!r.contains('['));
}

#[test]
fn kebab_and_snake_separators_are_removed_not_kept() {
    // Hyphens are split chars like any punctuation, so kebab-case CSS/CLI
    // identifiers render as prose too — and no separator reaches ese.
    assert_eq!(
        render_body("--embed-preproc font-size", EmbedPreproc::Split),
        "embed preproc font size"
    );
    assert_eq!(render_body("get_user-name", EmbedPreproc::Split), "get user name");
}

#[test]
fn prune_kw_drops_what_the_frozen_table_missed() {
    let nokw = render(EmbedPreproc::SplitNokw);
    let kw = render(EmbedPreproc::PruneKw);
    assert!(nokw.contains(" export "), "{nokw}");
    assert!(nokw.contains(" function "), "{nokw}");
    assert!(!kw.contains(" export "), "{kw}");
    assert!(!kw.contains(" function "), "{kw}");
}

#[test]
fn prune_lex_drops_types_namespaces_and_units() {
    let r = render(EmbedPreproc::PruneLex);
    for gone in ["number", "math", "ms"] {
        assert!(!r.split(' ').any(|t| t == gone), "{gone} survived: {r}");
    }
    // The domain words are exactly what must not be touched.
    for kept in ["compute", "backoff", "delay", "jitter", "attempt"] {
        assert!(r.split(' ').any(|t| t == kept), "{kept} was dropped: {r}");
    }
}

#[test]
fn prune_decl_keeps_the_declared_name_and_params_only() {
    let r = render(EmbedPreproc::PruneDecl);
    let body: Vec<&str> = r.split(' ').skip_while(|t| *t != "ts").skip(1).collect();
    assert_eq!(body, ["compute", "backoff", "delay", "attempt", "jitter"]);
}

#[test]
fn prune_soft_weights_declarations_without_deleting_references() {
    let r = render(EmbedPreproc::PruneSoft);
    let n = |t: &str| r.split(' ').filter(|x| *x == t).count();
    // Declared name twice, a pure reference still present exactly once.
    assert_eq!(n("backoff"), 2);
    assert_eq!(n("random"), 1);
    assert!(n("min") >= 1, "call-site tokens must survive: {r}");
}

#[test]
fn prune_uniq_emits_each_token_once() {
    let r = render(EmbedPreproc::PruneUniq);
    let toks: Vec<&str> = r.split(' ').collect();
    let mut sorted = toks.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(toks.len(), sorted.len(), "duplicate survived: {r}");
}

#[test]
fn declared_names_take_the_name_not_the_parameters() {
    // The display scanner (§25.1) is narrower than the scoring one: a
    // header saying `defines: f, g, h` for `def f(g, h)` would be noise.
    assert_eq!(declared_names("    def _format_list(self, frames):"), ["_format_list"]);
    assert_eq!(declared_names("class Controller(QObject):"), ["Controller"]);
    // Modifier chains collapse to the one name they introduce.
    assert_eq!(declared_names("pub async fn read_at(off: u64)"), ["read_at"]);
    assert_eq!(declared_names("export function computeBackoff(attempt) {"), ["computeBackoff"]);
    // Two declarations on one line are two names.
    assert_eq!(declared_names("let a = 1; let b = 2;"), ["a", "b"]);
    // Not declarations: a call, and a keyword with a number after it.
    assert!(declared_names("self.update_sources()").is_empty());
    assert!(declared_names("const 4").is_empty());
}

#[test]
fn declaration_sites_ignore_comparison_and_arrow() {
    // `==` and `=>` are not assignments; treating them as such would
    // declare half of every conditional.
    let s = "if (retryCount == maxRetries) { items.map(x => x.id); }";
    let r = render_body(s, EmbedPreproc::PruneDecl);
    assert!(!r.contains("retry"), "{r}");
    assert!(!r.contains("max"), "{r}");
}

#[test]
fn declaration_sites_survive_c_style_parameter_order() {
    // `int attempt` puts the type first; erring toward declaring keeps the
    // name, and LOW_SIGNAL removes the type.
    let s = "static int compute_backoff(int attempt, long base) { return attempt; }";
    let r = render_body(s, EmbedPreproc::PruneDecl);
    for kept in ["compute", "backoff", "attempt", "base"] {
        assert!(r.split(' ').any(|t| t == kept), "{kept} missing: {r}");
    }
}

#[test]
fn path_dedupe_collapses_the_repeated_segment() {
    let d = doc();
    let full = render_doc(&d, EmbedPreproc::PruneLex, PathRender::Full);
    let dd = render_doc(&d, EmbedPreproc::PruneLex, PathRender::Dedupe);
    // `searchEditor/` and `searchEditorActions` say it twice, and the body
    // says it not at all.
    assert_eq!(full.split(' ').filter(|t| *t == "editor").count(), 2);
    assert_eq!(dd.split(' ').filter(|t| *t == "editor").count(), 1);
}

#[test]
fn path_tail_keeps_only_the_last_two_segments() {
    let d = doc();
    let r = render_doc(&d, EmbedPreproc::PruneLex, PathRender::Tail);
    assert!(!r.contains("workbench"), "{r}");
    assert!(!r.contains("contrib"), "{r}");
    assert!(r.contains("browser"), "{r}");
    assert!(r.contains("actions"), "{r}");
}

#[test]
fn path_scaled_pins_the_paths_share_as_the_body_shrinks() {
    // The problem Scaled exists for: at the aggressive tier Full leaves the
    // path holding 11 of 16 tokens, so the vector mostly says "where this
    // file lives" and every window in the file converges. Scaled keeps the
    // share near PATH_SHARE at every rung instead of letting it climb.
    let d = doc();
    let n_full = render_doc(&d, EmbedPreproc::PruneDecl, PathRender::Full).split(' ').count();
    assert_eq!(n_full, 16);

    for tier in [EmbedPreproc::PruneLex, EmbedPreproc::PruneDecl] {
        let r = render_doc(&d, tier, PathRender::Scaled).into_owned();
        let total = r.split(' ').count() as f32;
        let body = render_body(BODY, tier).split(' ').count() as f32;
        let share = (total - body) / total;
        assert!(
            (0.1..=0.35).contains(&share),
            "{tier:?} path share {share:.2} outside the band: {r}"
        );
    }
}

#[test]
fn queries_are_never_pruned_only_normalized() {
    // A query has no declarations; PruneDecl on the query side would empty
    // it, and the low-signal table would eat real query words.
    let q = "parse a number from a string";
    for p in [EmbedPreproc::PruneLex, EmbedPreproc::PruneDecl, EmbedPreproc::PruneUniq] {
        let r = render_query(q, p);
        assert!(r.contains("number"), "{p:?} ate a query word: {r}");
        assert!(r.contains("string"), "{p:?} ate a query word: {r}");
        assert!(r.contains("parse"), "{p:?}: {r}");
    }
}

#[test]
fn keyword_pruning_is_symmetric_and_the_rest_is_not() {
    // §20.6, pinned, because it is the counter-intuitive half. Keyword
    // pruning applies to queries too — it costs real query words and wins
    // anyway, on all four corpora, because matching the corpus vocabulary
    // beats keeping the word. Anything a query cannot mirror stays
    // document-side.
    let q = "python logging can not create file from a type of object";
    // PruneKwPosQ0 is deliberately excluded: it is the arm that does NOT
    // mirror, and §22.1 P3 is the experiment that decides whether §20.6's
    // rule survives in the agent regime.
    for p in [EmbedPreproc::PruneKw, EmbedPreproc::PruneLex, EmbedPreproc::PruneDecl] {
        let r = render_query(q, p);
        for gone in ["not", "from", "type", "of", "object"] {
            assert!(!r.split(' ').any(|t| t == gone), "{p:?} kept {gone:?}: {r}");
        }
        // ...and the content words are all still there.
        for kept in ["python", "logging", "can", "create", "file"] {
            assert!(r.split(' ').any(|t| t == kept), "{p:?} ate {kept:?}: {r}");
        }
    }
    // The asymmetric half: a query has no declarations and the low-signal
    // table would eat it, so neither reaches the query side.
    let r = render_query("parse a number from a string", EmbedPreproc::PruneDecl);
    for kept in ["parse", "number", "string"] {
        assert!(r.split(' ').any(|t| t == kept), "{kept:?} lost: {r}");
    }
}

#[test]
fn the_sym_variants_mirror_their_tier_onto_the_query() {
    // §20.7: same documents as the tiers they mirror, different queries.
    // If the doc side moved too, the arm would confound symmetry with a
    // rendering change and could not discriminate anything.
    let d = doc();
    for (asym, sym) in [
        (EmbedPreproc::PruneLex, EmbedPreproc::PruneLexSym),
        (EmbedPreproc::PruneUniq, EmbedPreproc::PruneUniqSym),
    ] {
        assert_eq!(
            render_doc(&d, asym, PathRender::Full),
            render_doc(&d, sym, PathRender::Full),
            "{sym:?} changed the document side"
        );
    }
    // The query side is where they differ: low-signal words now go.
    let q = "parse a number from a string";
    assert!(render_query(q, EmbedPreproc::PruneLex).contains("number"));
    let sym = render_query(q, EmbedPreproc::PruneLexSym);
    assert!(!sym.split(' ').any(|t| t == "number"), "{sym}");
    assert!(!sym.split(' ').any(|t| t == "string"), "{sym}");
    assert!(sym.split(' ').any(|t| t == "parse"), "{sym}");

    // Dedupe reaches the query only under the mirrored variant.
    let rep = "cache cache lookup";
    assert_eq!(render_query(rep, EmbedPreproc::PruneUniq).matches("cache").count(), 2);
    assert_eq!(render_query(rep, EmbedPreproc::PruneUniqSym).matches("cache").count(), 1);
}

#[test]
fn positional_keeps_identifier_components_and_still_drops_boilerplate() {
    // The §22.1 regression: `PruneKw`'s table damaged 20.9% of the gold
    // function names agents were hunting, because it fires on a subtoken
    // wherever it appears. Positional fires only on a whole run.
    let kept = |p: EmbedPreproc, text: &str, tok: &str| {
        render_body(text, p).split(' ').any(|t| t == tok)
    };
    // The name of the thing being searched for survives.
    assert!(!kept(EmbedPreproc::PruneKw, "__init__", "init"), "naive kept init");
    assert!(kept(EmbedPreproc::PruneKwPos, "__init__", "init"));
    for (text, tok) in [
        ("from_dict", "from"),
        ("as_completed", "as"),
        ("get_object_or_404", "object"),
        ("for_each", "for"),
    ] {
        assert!(kept(EmbedPreproc::PruneKwPos, text, tok), "{text} lost {tok}");
    }
    // ...and real boilerplate still goes.
    for (text, tok) in [
        ("def compute_backoff(x)", "def"),
        ("class Foo", "class"),
        ("self.value", "self"),
        ("x: type = None", "type"),
    ] {
        assert!(!kept(EmbedPreproc::PruneKwPos, text, tok), "{text} kept {tok}");
    }
    // The compound survives even when its own subtoken is a keyword.
    let r = render_body("self.default_type", EmbedPreproc::PruneKwPos);
    assert!(r.contains("default") && r.contains("type"), "{r}");
    assert!(!r.split(' ').any(|t| t == "self"), "{r}");
}

#[test]
fn q0_leaves_the_query_alone_but_still_prunes_documents() {
    // §22.1 P3's arm: documents lose standalone keywords, queries lose
    // nothing. The disputed share is 9.1% of agent query tokens either way.
    let q = "def compute backoff class handler";
    assert_eq!(render_query(q, EmbedPreproc::PruneKwPosQ0), q);
    // ...while its documents still drop exactly those words.
    let d = render_body("def compute_backoff()", EmbedPreproc::PruneKwPosQ0);
    assert!(!d.split(' ').any(|t| t == "def"), "{d}");
    assert!(d.contains("compute") && d.contains("backoff"), "{d}");
    // Its symmetric twin renders documents identically - the arms differ
    // on the query side only, or the 2x2 confounds two changes.
    assert_eq!(render_body("def compute_backoff()", EmbedPreproc::PruneKwPos), d);
    assert_ne!(
        render_query(q, EmbedPreproc::PruneKwPos),
        render_query(q, EmbedPreproc::PruneKwPosQ0)
    );
}

#[test]
fn ladder_is_cumulative() {
    // Each rung must be a subset of the one below it, or "T3 is T2 plus a
    // rule" is not true and the campaign cannot attribute a delta to one
    // step. Dedupe and Boost change counts, not membership, so compare sets.
    let d = doc();
    let set = |p: EmbedPreproc| -> std::collections::HashSet<String> {
        render_doc(&d, p, PathRender::Full).split(' ').map(str::to_string).collect()
    };
    let rungs = [
        EmbedPreproc::Split,
        EmbedPreproc::PruneKw,
        EmbedPreproc::PruneLex,
        EmbedPreproc::PruneDecl,
    ];
    for w in rungs.windows(2) {
        let (a, b) = (set(w[0]), set(w[1]));
        assert!(b.is_subset(&a), "{:?} is not a subset of {:?}", w[1], w[0]);
    }
    for p in [EmbedPreproc::PruneSoft, EmbedPreproc::PruneUniq] {
        assert!(set(p).is_subset(&set(EmbedPreproc::PruneLex)), "{p:?}");
    }
}
