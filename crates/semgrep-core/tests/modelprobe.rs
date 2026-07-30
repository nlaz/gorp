//! §9.9 probe: is ese's embedding SPACE prose-shaped? Compare synonym
//! similarity for prose pairs vs code-concept pairs, and detect UNK words.
use semgrep_core::semantic::normalize;

fn vec_of(w: &str) -> Vec<f32> {
    let mut v = ese::encode_single(w).to_vec();
    normalize(&mut v);
    v
}
fn sim(a: &str, b: &str) -> f32 {
    vec_of(a).iter().zip(vec_of(b)).map(|(x, y)| x * y).sum()
}

#[test]
fn prose_pairs_vs_code_pairs() {
    let prose = [
        ("delete", "remove"),
        ("start", "begin"),
        ("big", "large"),
        ("error", "mistake"),
        ("fast", "quick"),
    ];
    // bool~boolean deliberately excluded: it scores via shared wordpiece
    // surface form, not learned code semantics (see §9.9).
    let code = [
        ("def", "function"),
        ("fn", "function"),
        ("none", "null"),
        ("str", "string"),
        ("mutex", "lock"),
        ("kmalloc", "allocate"),
        ("regex", "pattern"),
    ];
    println!("-- prose synonym pairs --");
    let mut prose_sum = 0.0;
    for (a, b) in prose {
        let s = sim(a, b);
        prose_sum += s;
        println!("  {a:>10} ~ {b:<10} {s:.3}");
    }
    println!("-- code concept pairs --");
    let mut code_sum = 0.0;
    for (a, b) in code {
        let s = sim(a, b);
        code_sum += s;
        println!("  {a:>10} ~ {b:<10} {s:.3}");
    }
    let (prose_mean, code_mean) =
        (prose_sum / prose.len() as f32, code_sum / code.len() as f32);
    println!("means: prose={prose_mean:.3} code={code_mean:.3}");
    // §9.9 documented finding: the space encodes prose synonymy but not
    // code-concept relations. If this flips (new embedder), revisit the
    // semantic stack's role on code.
    assert!(
        prose_mean > 3.0 * code_mean.max(0.01),
        "code pairs now score comparably to prose — embedding space changed"
    );
    println!("-- UNK detection (vs garbage token) --");
    let garbage = vec_of("qzxjvqk");
    for w in [
        "kmalloc",
        "regridder",
        "semaphore",
        "mutex",
        "goroutine",
        "dataframe",
        "tokenizer",
        "breakfast",
    ] {
        let v = vec_of(w);
        let s: f32 = garbage.iter().zip(&v).map(|(x, y)| x * y).sum();
        println!(
            "  {w:>10} sim-to-UNK={s:.3}{}",
            if s > 0.99 { "  ← OOV (falls to UNK)" } else { "" }
        );
    }
}
