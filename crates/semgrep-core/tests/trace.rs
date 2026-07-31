//! The stage report is a contract, and this is what makes it one.
//!
//! AUDIT.md:330 named the gap: "`SearchReport.stages` is `Vec<(String,f64)>`
//! built ad hoc at 14 call sites → no test can assert 'the indexed path always
//! reports these stages'". Every assertion in this file is one that could not be
//! written before the stage set was closed and the shape fixed.
//!
//! The last test is the important one. `unattributed_ms` is wall time no stage
//! claims, and every instrumentation gap this work closed — untimed discovery,
//! an invisible index build, an unmeasured query embedding on the path whose
//! whole job is embedding — showed up there first. Bounding it means the *next*
//! untimed step fails a build instead of quietly widening the gap.

use semgrep_core::search::{SearchOptions, search};
use semgrep_core::trace::{Bucket, SCHEDULE_COLD, SCHEDULE_KEYWORD, SCHEDULE_WARM, Stage};
use semgrep_core::{ChunkParams, rank::Mode, store};
use std::path::Path;

/// A corpus with enough distinct content that ranking has something to do.
fn corpus(dir: &Path) {
    std::fs::create_dir_all(dir.join("src")).unwrap();
    for i in 0..12 {
        std::fs::write(
            dir.join("src").join(format!("m{i}.rs")),
            format!(
                "//! Module {i}: retry and backoff policy.\n\
                 pub fn compute_backoff_{i}(attempt: u32) -> u64 {{\n\
                 \x20   let base = {i} + 1;\n\
                 \x20   base * (1 << attempt)\n\
                 }}\n\
                 pub struct Circuit{i} {{ pub failures: u32 }}\n"
            ),
        )
        .unwrap();
    }
}

/// One cache directory for the binary, and repair never throttled. Same shape as
/// `repair.rs` and `e2e.rs`, for the same reason: `cache_base()` latches the
/// environment into a `OnceLock` on first use (FIXES.md open #3), so cache state
/// is process-global and these tests take it in turn rather than isolating.
/// Setting `SEMGREP_CACHE_DIR` per test would silently do nothing after the
/// first one ran.
fn isolate_cache() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("SEMGREP_CACHE_DIR", dir.path());
            std::env::set_var("SEMGREP_CACHE_TTL_SECS", "0");
        }
        std::mem::forget(dir);
        std::sync::Mutex::new(())
    })
    .lock()
    .unwrap_or_else(|e| e.into_inner())
}

fn names(schedule: &[Stage]) -> Vec<&'static str> {
    schedule.iter().map(|s| s.name()).collect()
}

#[test]
fn warm_path_reports_the_full_schedule_for_every_mode() {
    let _guard = isolate_cache();
    let dir = tempfile::tempdir().unwrap();
    corpus(dir.path());
    let opts = store::BuildOptions::default();
    store::build(dir.path(), &opts, |_, _| {}).unwrap();

    for mode in [Mode::Bm25, Mode::Semantic, Mode::Hybrid] {
        let o = SearchOptions { mode, ..Default::default() };
        let r = search(dir.path(), "retry backoff policy", &o).unwrap();
        assert!(r.report.used_index, "{mode:?} should have been answered warm");
        assert_eq!(
            r.report.stages.names(),
            names(SCHEDULE_WARM),
            "{mode:?}: the warm path must report every scheduled stage, in order, \
             whether or not it ran"
        );
    }
}

#[test]
fn cold_path_reports_the_full_schedule_for_every_mode() {
    let _guard = isolate_cache();
    let dir = tempfile::tempdir().unwrap();
    corpus(dir.path());

    for mode in [Mode::Bm25, Mode::Semantic, Mode::Hybrid] {
        let o = SearchOptions { mode, no_index: true, ..Default::default() };
        let r = search(dir.path(), "retry backoff policy", &o).unwrap();
        assert!(!r.report.used_index);
        assert_eq!(r.report.stages.names(), names(SCHEDULE_COLD), "{mode:?}");
    }
}

#[test]
fn keyword_path_reports_a_schedule() {
    // It reported nothing at all before: `-e --stats` printed no provenance
    // line, so the one mode an agent reaches for first had no cost attribution.
    let _guard = isolate_cache();
    let dir = tempfile::tempdir().unwrap();
    corpus(dir.path());
    let o = SearchOptions { mode: Mode::Keyword, ..Default::default() };
    let r = search(dir.path(), "compute_backoff", &o).unwrap();
    assert!(!r.hits.is_empty());
    assert_eq!(r.report.stages.names(), names(SCHEDULE_KEYWORD));
    assert!(r.report.stages.ms(Stage::KeywordScan) > 0.0, "the scan must be timed");
}

#[test]
fn optional_stages_are_present_and_zero_rather_than_absent() {
    // The whole point of the fixed shape: a consumer keys on stage names and
    // gets the same keys every run. `rank:ann` and `rank:brute` are the sharp
    // case — the *name* used to change with graph size.
    let _guard = isolate_cache();
    let dir = tempfile::tempdir().unwrap();
    corpus(dir.path());
    store::build(dir.path(), &store::BuildOptions::default(), |_, _| {}).unwrap();

    let o = SearchOptions { mode: Mode::Hybrid, ..Default::default() };
    let r = search(dir.path(), "circuit failures", &o).unwrap();
    let s = &r.report.stages;

    // No HNSW graph was built, so brute force answered and `rank:ann` is zero —
    // but it is still reported.
    assert_eq!(s.ms(Stage::RankAnn), 0.0);
    assert!(s.ms(Stage::RankBrute) > 0.0);
    assert!(s.names().contains(&"rank:ann"));
    // PRF and MaxSim are off by default in hybrid; present, and zero.
    assert_eq!(s.ms(Stage::RankPrf), 0.0);
    assert!(s.names().contains(&"rank:prf"));
    assert!(s.names().contains(&"rank:maxsim"));
}

#[test]
fn buckets_partition_the_accounted_time() {
    let _guard = isolate_cache();
    let dir = tempfile::tempdir().unwrap();
    corpus(dir.path());
    let o = SearchOptions::default();
    let r = search(dir.path(), "backoff", &o).unwrap();

    let summed: f64 = Bucket::ALL.iter().map(|&b| r.report.stages.bucket_ms(b)).sum();
    let accounted = r.report.accounted_ms();
    assert!(
        (summed - accounted).abs() < 1e-9,
        "every stage must land in exactly one bucket: {summed} vs {accounted}"
    );
    // And the derived summary fields are those bucket sums, not separate clocks.
    assert_eq!(r.report.rank_ms(), r.report.stages.bucket_ms(Bucket::Rank));
}

#[test]
fn walk_and_load_are_no_longer_the_same_field() {
    // One field meant "corpus walk" cold and "index load" warm, which is why the
    // human line had to say `walk/load=`. Exactly one is nonzero per path.
    let _guard = isolate_cache();
    let dir = tempfile::tempdir().unwrap();
    corpus(dir.path());

    let cold = search(
        dir.path(),
        "backoff",
        &SearchOptions { no_index: true, ..Default::default() },
    )
    .unwrap();
    assert!(cold.report.walk_ms() > 0.0, "the cold path walks");
    assert_eq!(cold.report.load_ms(), 0.0, "the cold path loads nothing");

    store::build(dir.path(), &store::BuildOptions::default(), |_, _| {}).unwrap();
    let warm = search(dir.path(), "backoff", &SearchOptions::default()).unwrap();
    assert!(warm.report.used_index);
    assert_eq!(warm.report.walk_ms(), 0.0, "the warm path does not walk the corpus");
    assert!(warm.report.load_ms() > 0.0, "the warm path loads an index");
}

#[test]
fn the_write_through_build_is_attributed_to_the_query_that_paid_for_it() {
    // A full index build ran inside `search()` and contributed nothing but
    // `total_ms`, so the dominant cost of a first search was unattributable.
    let _guard = isolate_cache();
    let dir = tempfile::tempdir().unwrap();
    corpus(dir.path());

    let r = search(dir.path(), "retry backoff", &SearchOptions::default()).unwrap();
    assert!(r.report.wrote_cache, "the first ranked search of a scope writes through");
    assert!(r.report.build_ms() > 0.0, "and the build it paid for must be visible");
    assert!(
        r.report.build_ms() > r.report.rank_ms(),
        "on a cold start the build dominates the query it delayed: build={} rank={}",
        r.report.build_ms(),
        r.report.rank_ms()
    );
}

#[test]
fn build_reports_its_own_schedule() {
    let _guard = isolate_cache();
    let dir = tempfile::tempdir().unwrap();
    corpus(dir.path());
    let stats = store::build(dir.path(), &store::BuildOptions::default(), |_, _| {}).unwrap();

    assert_eq!(
        stats.stages.names(),
        names(semgrep_core::trace::SCHEDULE_BUILD),
        "`BuildStats` carried counts and no timings at all"
    );
    assert!(stats.stages.ms(Stage::BuildEmbed) > 0.0, "embedding is the build's main cost");
    assert!(stats.total_ms > 0.0);
}

#[test]
fn stages_account_for_nearly_all_of_the_measured_total() {
    // The tripwire. `unattributed_ms` is wall time no stage claims; it is where
    // every gap showed up, and bounding it is what makes the next one fail a
    // build. The absolute floor keeps a sub-millisecond query on a tiny fixture
    // from being flaky — 10% of 2ms is noise, 10% of 200ms is a missing stage.
    let _guard = isolate_cache();
    let dir = tempfile::tempdir().unwrap();
    corpus(dir.path());

    let mut checked = 0;
    for (label, opts) in [
        ("cold", SearchOptions { no_index: true, ..Default::default() }),
        ("write-through", SearchOptions::default()),
        ("warm", SearchOptions::default()),
        (
            "warm+prf+maxsim",
            SearchOptions { prf_terms: 8, rerank_maxsim: true, ..Default::default() },
        ),
        ("keyword", SearchOptions { mode: Mode::Keyword, ..Default::default() }),
    ] {
        let r = search(dir.path(), "compute backoff attempt", &opts).unwrap();
        let budget = 2.0_f64.max(0.10 * r.report.total_ms);
        assert!(
            r.report.unattributed_ms() <= budget,
            "{label}: {:.2}ms of {:.2}ms is unattributed (budget {budget:.2}ms) — \
             something in this path is not timed",
            r.report.unattributed_ms(),
            r.report.total_ms,
        );
        checked += 1;
    }
    assert_eq!(checked, 5);
}

#[test]
fn repair_outcome_distinguishes_the_three_reasons_for_a_zero() {
    // `repair:walk = 0` means the TTL was fresh, or the walk failed, or nothing
    // drifted. Those are a deliberate throttle, silently degraded correctness,
    // and the healthy case. A duration cannot tell them apart.
    use semgrep_core::cache::repair::RepairOutcome;

    let _guard = isolate_cache();
    let dir = tempfile::tempdir().unwrap();
    corpus(dir.path());
    store::build(dir.path(), &store::BuildOptions::default(), |_, _| {}).unwrap();

    // TTL 0: always validate, and the tree matches.
    let r = search(dir.path(), "backoff", &SearchOptions::default()).unwrap();
    assert_eq!(r.report.repair, RepairOutcome::NoDrift);

    // Change a file, by enough that size differs — `corpus::diff` compares
    // (size, mtime) and mtime has whole-second resolution.
    std::fs::write(
        dir.path().join("src/m0.rs"),
        "pub fn totally_different_symbol_here() -> u64 { 7 }\n".repeat(4),
    )
    .unwrap();
    let r = search(dir.path(), "backoff", &SearchOptions::default()).unwrap();
    match r.report.repair {
        RepairOutcome::Repaired { modified, delta_chunks, .. } => {
            assert_eq!(modified, 1);
            assert!(delta_chunks > 0);
        }
        other => panic!("expected Repaired, got {other:?}"),
    }
}

#[test]
fn a_scope_with_no_index_reports_not_applicable() {
    let _guard = isolate_cache();
    let dir = tempfile::tempdir().unwrap();
    corpus(dir.path());
    let r = search(
        dir.path(),
        "backoff",
        &SearchOptions { no_index: true, ..Default::default() },
    )
    .unwrap();
    assert_eq!(r.report.repair, semgrep_core::cache::repair::RepairOutcome::NotApplicable);
}

#[test]
fn chunk_params_do_not_change_the_reported_shape() {
    // A `--window` sweep must not produce reports that cannot be compared with
    // ordinary ones; that class of mismatch is FIXES.md #10's shape.
    let _guard = isolate_cache();
    let dir = tempfile::tempdir().unwrap();
    corpus(dir.path());
    for window in [8u32, 32, 64] {
        let opts = SearchOptions {
            no_index: true,
            params: ChunkParams { window, overlap: 2, ..Default::default() },
            ..Default::default()
        };
        let r = search(dir.path(), "backoff", &opts).unwrap();
        assert_eq!(r.report.stages.names(), names(SCHEDULE_COLD), "window={window}");
    }
}
