//! Per-stage timing provenance.
//!
//! `--stats` prints these, and the point of them is that "the query took 115 ms"
//! is unactionable while "load:bm25=84ms rank:brute=12ms" says what to fix. Every
//! measured number in RESEARCH.md that led to a change came from reading one of
//! these lines.
//!
//! The type exists so recording a stage is one call rather than the
//! `Instant::now()` / compute / `push((name, elapsed))` triple that appeared at
//! fourteen sites, each free to name its stage differently or forget it.

use std::time::Instant;

#[derive(Debug, Default)]
pub struct Trace {
    stages: Vec<(String, f64)>,
}

impl Trace {
    pub fn new() -> Self {
        Self::default()
    }

    /// Run `f`, recording how long it took.
    pub fn time<T>(&mut self, stage: &str, f: impl FnOnce() -> T) -> T {
        let start = Instant::now();
        let out = f();
        self.record(stage, elapsed_ms(start));
        out
    }

    /// Record a duration measured elsewhere — for work whose timing is reported
    /// by the thing that did it, like the per-component index load.
    pub fn record(&mut self, stage: &str, ms: f64) {
        self.stages.push((stage.to_string(), ms));
    }

    pub fn into_stages(self) -> Vec<(String, f64)> {
        self.stages
    }
}

pub fn elapsed_ms(since: Instant) -> f64 {
    since.elapsed().as_secs_f64() * 1e3
}
