//! Real numbers for the pipeline-explorer artifact (RESEARCH.md §14):
//! a fixed 8-chunk demo corpus and 6 scenario queries, scored by the actual
//! engine — BM25 via `rank::bm25`, embeddings via ese through the same
//! `text::` pipeline the index build uses, under three treatments
//! (raw doc_text, `split` rendering, `split` + SIF pooling).
//!
//!     cargo run --release --example pipeline_probe > probe.json
//!
//! The output is data, not a report: every cosine and BM25 score in the
//! artifact traces back to this file, so the page can be regenerated when the
//! engine changes instead of drifting into fiction.

use gorp_core::corpus::doc_text;
use gorp_core::rank::bm25::Bm25Index;
use gorp_core::rank::normalize;
use gorp_core::text::{
    EmbedPreproc, PathRender, SifStats, embed_query, embed_sif, prose_render_body,
    prose_render_doc, prose_render_query,
};
use serde_json::json;

const CHUNKS: [(&str, &str); 8] = [
    (
        "web/retry.ts",
        "export function computeBackoffDelay(attempt: number): number {\n  const jitter = Math.random() * BASE_DELAY_MS;\n  return Math.min(MAX_DELAY_MS, 2 ** attempt * jitter);\n}",
    ),
    (
        "auth/session.py",
        "def validate_session_token(token, max_age_secs=3600):\n    \"\"\"Check whether a session token is still valid.\"\"\"\n    if token is None or _is_expired(token, max_age_secs):\n        return False\n    return hmac.compare_digest(token.sig, _sign(token.payload))",
    ),
    (
        "net/reaper.go",
        "func evictDormantPeers(pool *PeerPool) {\n    for _, p := range pool.members {\n        if time.Since(p.lastSeen) > dormancyTTL {\n            pool.drop(p)\n        }\n    }\n}",
    ),
    (
        "net/socketPool.ts",
        "closeIdleConnections(): void {\n  for (const conn of this.connections) {\n    if (Date.now() - conn.lastUsedAt > this.idleTimeoutMs) conn.close();\n  }\n}",
    ),
    (
        "config/parse.rs",
        "/// Parse a configuration file, ignoring comment lines.\npub fn parse_config(text: &str) -> Config {\n    text.lines()\n        .filter(|l| !l.trim_start().starts_with('#'))\n        .fold(Config::default(), Config::apply_line)\n}",
    ),
    (
        "jobs/cron.py",
        "def expand_cron_field(field, lo, hi):\n    \"\"\"Expand one cron expression field into its allowed values.\"\"\"\n    if field == \"*\":\n        return list(range(lo, hi + 1))\n    return sorted(int(v) for v in field.split(\",\"))",
    ),
    (
        "block/rwstat.c",
        "static inline void blkg_rwstat_add(struct blkg_rwstat *rwstat,\n                                   unsigned int op, uint64_t val)\n{\n        percpu_counter_add(&rwstat->cpu_cnt[op_to_index(op)], val);\n}",
    ),
    (
        "web/client.ts",
        "// HTTP client for the jobs API: timeouts, retries, and typed responses.\nasync request(path: string, init: RequestInit): Promise<Response> {\n  for (let attempt = 0; attempt < this.maxRetries; attempt++) {\n    const res = await fetch(this.base + path, init);\n    if (res.ok) return res;\n  }\n  throw new HttpRetryError(path);\n}",
    ),
];

/// (query, gold chunk index, scenario label)
const SCENARIOS: [(&str, usize, &str); 6] = [
    ("computeBackoffDelay", 0, "exact camelCase identifier"),
    ("compute the backoff delay with jitter", 0, "prose naming the concept"),
    ("blkg_rwstat_add", 6, "rare snake_case identifier"),
    ("close connections that have gone idle", 3, "paraphrase the code's own words can carry"),
    ("shut down clients that went quiet", 2, "true paraphrase — zero shared vocabulary"),
    ("python code to check if a session is still valid", 1, "CoSQA-style real-query prose"),
];

/// Word pairs probed directly in the embedding space (§9.9's method).
const PAIRS: [(&str, &str); 12] = [
    ("delete", "remove"),
    ("start", "begin"),
    ("str", "string"),
    ("mutex", "lock"),
    ("def", "function"),
    ("close", "evict"),
    ("idle", "dormant"),
    ("connections", "peers"),
    ("quiet", "dormant"),
    ("shut", "drop"),
    ("backoff", "delay"),
    ("retry", "attempt"),
];

/// Query-token attribution tables: (scenario idx, chunk idx) worth showing.
const ATTRIBUTIONS: [(usize, usize); 4] = [(1, 0), (1, 7), (4, 2), (4, 7)];

fn cos(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn unit(mut v: [f32; gorp_core::EMBED_DIM]) -> Vec<f32> {
    normalize(&mut v);
    v.to_vec()
}

/// Per-token unit vectors of `text` as ese tokenizes it, with the token strings.
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

fn main() {
    let docs: Vec<String> = CHUNKS.iter().map(|(p, c)| doc_text(p, c)).collect();
    let rendered: Vec<String> = docs
        .iter()
        .map(|d| prose_render_doc(d, EmbedPreproc::Split, PathRender::Full).into_owned())
        .collect();

    // SIF stats over the rendered demo corpus PLUS the frozen tests/corpus
    // fixture: eight chunks alone give degenerate frequencies (every token is
    // rare), and the artifact would then show SIF artifacts instead of SIF.
    let mut sif = SifStats::default();
    for r in &rendered {
        sif.count(r);
    }
    let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/corpus");
    let mut stack = vec![std::path::PathBuf::from(fixture)];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir).expect("tests/corpus exists").flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if let Ok(text) = std::fs::read_to_string(&p) {
                sif.count(&prose_render_body(&text, EmbedPreproc::Split));
            }
        }
    }
    sif.a = 1e-3;

    // Treatments: [raw, split, split-sif, split-idf]. The last two share one
    // stats object — only the weighting curve flips (§14.7).
    let mut chunk_vecs: Vec<Vec<Vec<f32>>> = docs
        .iter()
        .zip(&rendered)
        .map(|(d, r)| {
            vec![unit(embed_query(d)), unit(embed_query(r)), unit(embed_sif(r, &sif))]
        })
        .collect();
    let query_texts: Vec<(String, String)> = SCENARIOS
        .iter()
        .map(|&(q, _, _)| {
            (q.to_string(), prose_render_query(q, EmbedPreproc::Split).into_owned())
        })
        .collect();
    let mut query_vecs: Vec<Vec<Vec<f32>>> = query_texts
        .iter()
        .map(|(q, qr)| {
            vec![unit(embed_query(q)), unit(embed_query(qr)), unit(embed_sif(qr, &sif))]
        })
        .collect();
    sif.idf = true;
    for (cv, r) in chunk_vecs.iter_mut().zip(&rendered) {
        cv.push(unit(embed_sif(r, &sif)));
    }
    for (qv, (_, qr)) in query_vecs.iter_mut().zip(&query_texts) {
        qv.push(unit(embed_sif(qr, &sif)));
    }

    let mut bm25 = Bm25Index::new();
    for d in &docs {
        bm25.add_doc(d);
    }
    bm25.finalize();

    let scenarios: Vec<_> = SCENARIOS
        .iter()
        .enumerate()
        .map(|(si, &(q, gold, label))| {
            let qv = &query_vecs[si];
            let cosines: Vec<Vec<f32>> = chunk_vecs
                .iter()
                .map(|cv| (0..4).map(|i| cos(&qv[i], &cv[i])).collect())
                .collect();
            let bm: Vec<(u32, f32)> = bm25.query(q, CHUNKS.len());
            json!({
                "query": q, "gold": gold, "label": label,
                "cosines": cosines,   // per chunk: [raw, split, split_sif, split_idf]
                "bm25": bm,           // engine truth: (chunk_id, score) desc
            })
        })
        .collect();

    let pairs: Vec<_> = PAIRS
        .iter()
        .map(|&(a, b)| {
            let (va, vb) = (unit(embed_query(a)), unit(embed_query(b)));
            json!({ "a": a, "b": b, "cos": cos(&va, &vb) })
        })
        .collect();

    // Per-query-token argmax over chunk tokens (§9.8's table), raw vs rendered.
    let attributions: Vec<_> = ATTRIBUTIONS
        .iter()
        .map(|&(si, ci)| {
            let q = SCENARIOS[si].0;
            let table = |qtext: &str, ctext: &str| -> Vec<serde_json::Value> {
                let ct = toks(ctext);
                toks(qtext)
                    .iter()
                    .map(|(qt, qv)| {
                        let best = ct
                            .iter()
                            .map(|(t, v)| (t.clone(), cos(qv, v)))
                            .max_by(|a, b| a.1.total_cmp(&b.1));
                        let (bt, bc) = best.unwrap_or(("".into(), 0.0));
                        json!({ "q": qt, "best": bt, "cos": bc })
                    })
                    .collect()
            };
            json!({
                "scenario": si, "chunk": ci,
                "raw": table(q, &docs[ci]),
                "split": table(
                    &prose_render_query(q, EmbedPreproc::Split),
                    &prose_render_doc(&docs[ci], EmbedPreproc::Split, PathRender::Full),
                ),
            })
        })
        .collect();

    let out = json!({
        "chunks": CHUNKS.iter().map(|(p, c)| json!({"path": p, "code": c})).collect::<Vec<_>>(),
        "scenarios": scenarios,
        "pairs": pairs,
        "attributions": attributions,
        "bm25_params": { "k1": 1.2, "b": 0.75 },
    });
    println!("{}", serde_json::to_string_pretty(&out).unwrap());
}
