//! Provenance for a single semantic hit or miss (RESEARCH.md §15.9).
//!
//! Given a query and a gold span (and optionally a distractor span), print
//! where the similarity actually comes from: pooled cosines under the three
//! treatments, per-query-token attribution against each span, the SIF weight
//! each query token carries, and which tokens dominate each span's pooled
//! vector. Reads SIF stats from the corpus's live `.semgrep/sif.bin`, so run
//! it with the index built the way the condition under study builds it.
//!
//!     cargo run --release --example why_miss -- <corpus_root> "<query>" \
//!         gold.rs:10:20 [distractor.rs:5:9]

use semgrep_core::corpus::doc_text;
use semgrep_core::rank::normalize;
use semgrep_core::text::{EmbedPreproc, SifStats, embed_query, embed_sif, prose_render};
use serde_json::json;
use std::path::Path;

fn unit(mut v: [f32; semgrep_core::EMBED_DIM]) -> Vec<f32> {
    normalize(&mut v);
    v.to_vec()
}

fn cos(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn toks(text: &str) -> Vec<(String, Vec<f32>)> {
    let mut out = Vec::new();
    ese::for_each_token_vector(text, |tok, v| {
        let mut v = v.to_vec();
        let n: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if n > 0.0 {
            for x in v.iter_mut() {
                *x /= n;
            }
            out.push((tok.to_string(), v));
        }
    });
    out
}

fn span_of(root: &Path, spec: &str) -> (String, String) {
    let mut it = spec.rsplitn(3, ':');
    let end: usize = it.next().unwrap().parse().expect("end line");
    let start: usize = it.next().unwrap().parse().expect("start line");
    let rel = it.next().unwrap().to_string();
    let text = std::fs::read_to_string(root.join(&rel)).expect("read gold file");
    let lines: Vec<&str> = text.lines().collect();
    (rel.clone(), lines[start - 1..end.min(lines.len())].join("\n"))
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let root = Path::new(&args[1]);
    let query = &args[2];
    let spans: Vec<(String, String)> = args[3..].iter().map(|s| span_of(root, s)).collect();

    let sif_path = root.join(".semgrep/sif.bin");
    let sif: Option<SifStats> = std::fs::read(&sif_path)
        .ok()
        .and_then(|b| postcard::from_bytes(&b).ok());

    let q_render = prose_render(query, EmbedPreproc::Split).into_owned();
    let qv = [
        unit(embed_query(query)),
        unit(embed_query(&q_render)),
        sif.as_ref().map(|s| unit(embed_sif(&q_render, s))).unwrap_or_default(),
    ];
    let q_toks = toks(&q_render);

    let mut out_spans = Vec::new();
    for (rel, text) in &spans {
        let doc = doc_text(rel, text);
        let rendered = prose_render(&doc, EmbedPreproc::Split).into_owned();
        let cv = [
            unit(embed_query(&doc)),
            unit(embed_query(&rendered)),
            sif.as_ref().map(|s| unit(embed_sif(&rendered, s))).unwrap_or_default(),
        ];
        let ct = toks(&rendered);

        // Per query token: best chunk token, its cosine, and the token's SIF
        // weight — the weight decides how much of the pooled query vector
        // this token even is.
        let attribution: Vec<_> = q_toks
            .iter()
            .map(|(qt, qvec)| {
                let (bt, bc) = ct
                    .iter()
                    .map(|(t, v)| (t.as_str(), cos(qvec, v)))
                    .max_by(|a, b| a.1.total_cmp(&b.1))
                    .unwrap_or(("", 0.0));
                json!({
                    "q": qt, "w": sif.as_ref().map(|s| s.weight(qt)),
                    "best": bt, "cos": bc,
                })
            })
            .collect();

        // Where the chunk's pooled mass sits: its tokens sorted by SIF weight
        // (dedup, keep max-weight first 10).
        let mut seen = std::collections::HashSet::new();
        let mut mass: Vec<(String, f32)> = ct
            .iter()
            .filter(|(t, _)| seen.insert(t.clone()))
            .map(|(t, _)| (t.clone(), sif.as_ref().map_or(1.0, |s| s.weight(t))))
            .collect();
        mass.sort_by(|a, b| b.1.total_cmp(&a.1));
        mass.truncate(10);

        out_spans.push(json!({
            "span": rel,
            "cos_raw": cos(&qv[0], &cv[0]),
            "cos_split": cos(&qv[1], &cv[1]),
            "cos_split_sif": if sif.is_some() { Some(cos(&qv[2], &cv[2])) } else { None },
            "attribution_split": attribution,
            "top_weighted_tokens": mass,
        }));
    }

    println!("{}", serde_json::to_string_pretty(&json!({
        "query": query,
        "rendered": q_render,
        "sif_loaded": sif.is_some(),
        "spans": out_spans,
    })).unwrap());
}
