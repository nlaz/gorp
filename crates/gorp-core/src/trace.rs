//! Per-stage timing provenance.
//!
//! `--stats` prints these, and the point of them is that "the query took 115 ms"
//! is unactionable while "load:bm25=84ms rank:brute=12ms" says what to fix. Every
//! measured number in RESEARCH.md that led to a change came from reading one of
//! these lines.
//!
//! Three properties make that trustworthy, and none of them came free:
//!
//! **The set of stages is closed.** [`Stage`] is an enum, not a string, so a
//! stage cannot be invented at a call site or spelled two ways at two call
//! sites. It was `Vec<(String, f64)>` built ad hoc at fourteen sites, which is
//! how "no test can assert the indexed path always reports these stages"
//! (AUDIT.md) became true.
//!
//! **The shape is fixed.** A path declares a [`SCHEDULE_WARM`]-style schedule up
//! front and [`Trace::finish`] emits *every* stage on it, zero-filled if it never
//! ran. So `rank:ann` and `rank:brute` both appear with one of them zero, and
//! `repair:walk` appears whether or not repair fired. A consumer keying on stage
//! names sees the same keys every time, which is what lets a simulation compare
//! two runs without special-casing which optional stages happened to fire.
//! Zero-filling is not new — `LoadTimings` has always emitted its five
//! components unconditionally; this generalizes it.
//!
//! **Every stage is a leaf in exactly one [`Bucket`].** That is what makes the
//! summary fields derivable rather than separately measured, and it is the whole
//! reason the residual means anything: if `rank_ms` is its own `Instant` *and*
//! the `rank:*` stages are summed, the two disagree for legitimate reasons
//! (nesting) and `total - accounted` is noise. Derived, they reconcile by
//! construction, and the one number left over — `total_ms - accounted_ms` — is
//! exactly the work nobody is timing. A test gates it, so the next untimed step
//! fails the build instead of quietly widening the gap.

use std::time::Instant;

/// Coarse cost centers. Each stage belongs to exactly one, so bucket totals are
/// sums of disjoint leaves and the summary fields never double-count.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Hash)]
pub enum Bucket {
    /// Resolving which index answers this query.
    Discover,
    /// Walking the corpus (cold path).
    Walk,
    /// Building an index — the write-through on a cache miss.
    Build,
    /// Reading an index off disk (warm path).
    Load,
    /// The read-repair overlay.
    Repair,
    /// Scoring and fusing.
    Rank,
    /// Turning ranked ids into displayable hits.
    Finalize,
}

impl Bucket {
    pub const ALL: &'static [Bucket] = &[
        Bucket::Discover,
        Bucket::Walk,
        Bucket::Build,
        Bucket::Load,
        Bucket::Repair,
        Bucket::Rank,
        Bucket::Finalize,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Bucket::Discover => "discover",
            Bucket::Walk => "walk",
            Bucket::Build => "build",
            Bucket::Load => "load",
            Bucket::Repair => "repair",
            Bucket::Rank => "rank",
            Bucket::Finalize => "finalize",
        }
    }
}

impl serde::Serialize for Bucket {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.name())
    }
}

/// Declares the stage table once, so a variant cannot exist without a display
/// name and a bucket. `every_stage_has_exactly_one_bucket` is then structural
/// rather than a list someone has to remember to extend.
macro_rules! stages {
    ($($variant:ident => $name:literal, $bucket:ident;)*) => {
        /// One timed step. The name is the wire format — these strings appear in
        /// `--stats` output, in the trace envelope, and quoted in RESEARCH.md, so
        /// renaming one is a breaking change to all three.
        #[derive(Copy, Clone, PartialEq, Eq, Debug, Hash)]
        pub enum Stage { $($variant,)* }

        impl Stage {
            /// Every stage that exists, for exhaustiveness tests.
            pub const ALL: &'static [Stage] = &[$(Stage::$variant,)*];

            pub const fn name(self) -> &'static str {
                match self { $(Stage::$variant => $name,)* }
            }

            pub const fn bucket(self) -> Bucket {
                match self { $(Stage::$variant => Bucket::$bucket,)* }
            }
        }
    };
}

stages! {
    // Which index answers. Measured in `search::search`, and on an exact-mode
    // miss the CLI resolves twice more — the envelope's `discover_calls` is
    // what makes that visible.
    Discover            => "discover",              Discover;

    // The write-through build. Previously invisible: a full index build ran
    // inside `search()` and contributed nothing but `total_ms`.
    BuildWalk           => "build:walk",            Build;
    BuildSif            => "build:sif",             Build;
    BuildRead           => "build:read+tokenize",   Build;
    BuildEmbed          => "build:embed",           Build;
    BuildHnsw           => "build:hnsw",            Build;
    BuildWrite          => "build:write",           Build;

    // Reading an index back. `load:sif` was the one component with no timer.
    LoadMeta            => "load:meta",             Load;
    LoadChunks          => "load:chunks",           Load;
    LoadBm25            => "load:bm25",             Load;
    LoadMmap            => "load:mmap",             Load;
    LoadHnsw            => "load:hnsw",             Load;
    LoadSif             => "load:sif",              Load;

    // Read-repair. Both always emitted now; the categorical outcome that says
    // *why* they are zero lives in `RepairOutcome`, because a duration cannot
    // distinguish "the TTL was fresh" from "the walk found no drift".
    RepairWalk          => "repair:walk",           Repair;
    RepairDelta         => "repair:delta",          Repair;

    // Cold path.
    Walk                => "walk",                  Walk;
    PassRead            => "pass:read+tokenize",    Rank;
    PassEmbed           => "pass:embed",            Rank;

    // Scoring.
    KeywordScan         => "keyword:scan",          Rank;
    RankBm25            => "rank:bm25",             Rank;
    RankPrf             => "rank:prf",              Rank;
    RankBridge          => "rank:bridge",           Rank;
    RankEmbedQuery      => "rank:embed-query",      Rank;
    RankAnn             => "rank:ann",              Rank;
    RankBrute           => "rank:brute",            Rank;
    RankMaxsim          => "rank:maxsim",           Rank;
    RankDeclBoost       => "rank:decl-boost",       Rank;
    RankFuse            => "rank:fuse",             Rank;
    Candidates          => "candidates",            Rank;

    // Materialization. Was one `finalize` bucket hiding three cost curves: a
    // quadratic-ish span dedupe, an O(k·n·dims) MMR over dequantized vectors,
    // and a file re-read per hit.
    FinalizeDedupe      => "finalize:dedupe",       Finalize;
    FinalizeFine        => "finalize:fine",         Finalize;
    FinalizeRerank      => "finalize:rerank",       Finalize;
    FinalizeVectors     => "finalize:vectors",      Finalize;
    FinalizeMmr         => "finalize:mmr",          Finalize;
    FinalizeMaterialize => "finalize:materialize",  Finalize;
}

impl serde::Serialize for Stage {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.name())
    }
}

/// The warm path, in emission order.
///
/// `Discover` and the `Build*` stages are on it because `search::search` does
/// that work *before* choosing a path and hands it down: on a cache miss the
/// query pays a full index build, and it belongs in the same report as the
/// query it delayed.
pub const SCHEDULE_WARM: &[Stage] = &[
    Stage::Discover,
    Stage::BuildWalk,
    Stage::BuildSif,
    Stage::BuildRead,
    Stage::BuildEmbed,
    Stage::BuildHnsw,
    Stage::BuildWrite,
    Stage::LoadMeta,
    Stage::LoadChunks,
    Stage::LoadBm25,
    Stage::LoadMmap,
    Stage::LoadHnsw,
    Stage::LoadSif,
    Stage::RepairWalk,
    Stage::RepairDelta,
    Stage::RankBm25,
    Stage::RankPrf,
    Stage::RankBridge,
    Stage::RankEmbedQuery,
    Stage::RankAnn,
    Stage::RankBrute,
    Stage::RankMaxsim,
    Stage::RankFuse,
    Stage::RankDeclBoost,
    Stage::Candidates,
    Stage::FinalizeDedupe,
    Stage::FinalizeFine,
    Stage::FinalizeRerank,
    Stage::FinalizeVectors,
    Stage::FinalizeMmr,
    Stage::FinalizeMaterialize,
];

/// The cold path. Carries the `Build*` stages too: a write-through that *failed*
/// still falls through to streaming, and the work it did before failing would
/// otherwise vanish into `total_ms`.
pub const SCHEDULE_COLD: &[Stage] = &[
    Stage::Discover,
    Stage::BuildWalk,
    Stage::BuildSif,
    Stage::BuildRead,
    Stage::BuildEmbed,
    Stage::BuildHnsw,
    Stage::BuildWrite,
    Stage::Walk,
    Stage::RankEmbedQuery,
    Stage::PassRead,
    Stage::PassEmbed,
    Stage::RankBm25,
    Stage::RankPrf,
    Stage::RankBridge,
    Stage::RankMaxsim,
    Stage::RankFuse,
    Stage::RankDeclBoost,
    Stage::Candidates,
    Stage::FinalizeDedupe,
    Stage::FinalizeFine,
    Stage::FinalizeRerank,
    Stage::FinalizeVectors,
    Stage::FinalizeMmr,
    Stage::FinalizeMaterialize,
];

/// Exact mode, which reported nothing at all before — `--stats -e` printed
/// `chunks=0` and no provenance line.
pub const SCHEDULE_KEYWORD: &[Stage] = &[Stage::KeywordScan, Stage::FinalizeMaterialize];

/// A standalone `gorp index`. The same stages the write-through folds into a
/// query's report, which is what makes the two comparable.
pub const SCHEDULE_BUILD: &[Stage] = &[
    Stage::BuildWalk,
    Stage::BuildSif,
    Stage::BuildRead,
    Stage::BuildEmbed,
    Stage::BuildHnsw,
    Stage::BuildWrite,
];

/// Accumulates stage timings against a declared schedule.
#[derive(Debug)]
pub struct Trace {
    schedule: &'static [Stage],
    recorded: Vec<(Stage, f64)>,
}

impl Trace {
    pub fn new(schedule: &'static [Stage]) -> Self {
        Self { schedule, recorded: Vec::with_capacity(schedule.len()) }
    }

    /// Run `f`, recording how long it took.
    pub fn time<T>(&mut self, stage: Stage, f: impl FnOnce() -> T) -> T {
        let start = Instant::now();
        let out = f();
        self.record(stage, elapsed_ms(start));
        out
    }

    /// Record a duration measured elsewhere — for work whose timing is reported
    /// by the thing that did it, like the per-component index load.
    pub fn record(&mut self, stage: Stage, ms: f64) {
        debug_assert!(
            self.schedule.contains(&stage),
            "stage {:?} recorded off-schedule; add it to the path's SCHEDULE_* or it \
             will be silently dropped from the report",
            stage
        );
        self.recorded.push((stage, ms));
    }

    /// Replay stages measured before the path was chosen (discovery, and the
    /// write-through build) into this path's report.
    pub fn replay(&mut self, stages: &[(Stage, f64)]) {
        for &(stage, ms) in stages {
            self.record(stage, ms);
        }
    }

    /// Every scheduled stage, in schedule order, summing repeats and zero-filling
    /// what never ran. Anything recorded off-schedule is dropped — `record`
    /// debug-asserts against that, so it cannot happen in a test build.
    pub fn finish(self) -> Stages {
        let rows = self
            .schedule
            .iter()
            .map(|&stage| {
                let ms = self
                    .recorded
                    .iter()
                    .filter(|(s, _)| *s == stage)
                    .map(|(_, ms)| *ms)
                    .sum();
                StageMs { stage, bucket: stage.bucket(), ms }
            })
            .collect();
        Stages { rows }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct StageMs {
    pub stage: Stage,
    pub bucket: Bucket,
    pub ms: f64,
}

/// A completed, fixed-shape stage report.
///
/// Serializes as a list of `{stage, bucket, ms}` objects. Deliberately not the
/// `Vec<(String, f64)>` it replaced: that derives into arrays-of-arrays, which
/// is a hostile thing to hand a consumer.
#[derive(Debug, Default, Clone, serde::Serialize)]
#[serde(transparent)]
pub struct Stages {
    rows: Vec<StageMs>,
}

impl Stages {
    pub fn iter(&self) -> impl Iterator<Item = &StageMs> {
        self.rows.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The schedule this report was built from, in order. What a schedule test
    /// asserts against.
    pub fn names(&self) -> Vec<&'static str> {
        self.rows.iter().map(|r| r.stage.name()).collect()
    }

    pub fn ms(&self, stage: Stage) -> f64 {
        self.rows.iter().find(|r| r.stage == stage).map(|r| r.ms).unwrap_or(0.0)
    }

    /// Total for one cost center. Disjoint by construction, so summing across
    /// every bucket equals [`Stages::accounted_ms`].
    pub fn bucket_ms(&self, bucket: Bucket) -> f64 {
        self.rows.iter().filter(|r| r.bucket == bucket).map(|r| r.ms).sum()
    }

    /// Everything this report accounts for. The gap to the measured total is
    /// work that nothing is timing.
    pub fn accounted_ms(&self) -> f64 {
        self.rows.iter().map(|r| r.ms).sum()
    }
}

pub fn elapsed_ms(since: Instant) -> f64 {
    since.elapsed().as_secs_f64() * 1e3
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finish_zero_fills_the_whole_schedule_in_order() {
        let mut t = Trace::new(SCHEDULE_KEYWORD);
        t.record(Stage::KeywordScan, 5.0);
        let stages = t.finish();
        assert_eq!(stages.names(), vec!["keyword:scan", "finalize:materialize"]);
        assert_eq!(stages.ms(Stage::FinalizeMaterialize), 0.0);
    }

    #[test]
    fn repeats_of_one_stage_sum() {
        // The warm path records `rank:bm25` once for the base query and again
        // inside PRF; a report that kept only the last would understate it.
        let mut t = Trace::new(SCHEDULE_KEYWORD);
        t.record(Stage::KeywordScan, 2.0);
        t.record(Stage::KeywordScan, 3.0);
        assert_eq!(t.finish().ms(Stage::KeywordScan), 5.0);
    }

    #[test]
    fn buckets_partition_accounted_time() {
        let mut t = Trace::new(SCHEDULE_WARM);
        t.record(Stage::LoadBm25, 10.0);
        t.record(Stage::RankBrute, 4.0);
        t.record(Stage::FinalizeMmr, 1.0);
        let s = t.finish();
        let by_bucket: f64 = Bucket::ALL.iter().map(|&b| s.bucket_ms(b)).sum();
        assert_eq!(by_bucket, s.accounted_ms());
        assert_eq!(s.bucket_ms(Bucket::Load), 10.0);
        assert_eq!(s.bucket_ms(Bucket::Rank), 4.0);
    }

    #[test]
    fn every_stage_appears_on_at_least_one_schedule() {
        // A stage no schedule emits is dead weight that looks like coverage.
        for &stage in Stage::ALL {
            let on_any = [SCHEDULE_WARM, SCHEDULE_COLD, SCHEDULE_KEYWORD, SCHEDULE_BUILD]
                .iter()
                .any(|s| s.contains(&stage));
            assert!(on_any, "{stage:?} is on no schedule");
        }
    }

    #[test]
    fn no_schedule_lists_a_stage_twice() {
        // finish() sums duplicates, so a doubled schedule entry would emit the
        // same total under the same name twice and inflate accounted_ms.
        for schedule in [SCHEDULE_WARM, SCHEDULE_COLD, SCHEDULE_KEYWORD, SCHEDULE_BUILD] {
            let mut seen = std::collections::HashSet::new();
            for &stage in schedule {
                assert!(seen.insert(stage), "{stage:?} listed twice");
            }
        }
    }

    #[test]
    fn stage_names_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for &stage in Stage::ALL {
            assert!(seen.insert(stage.name()), "duplicate stage name {}", stage.name());
        }
    }
}
