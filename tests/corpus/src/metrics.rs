//! Counters and latency histograms, exported in Prometheus text format.

use std::collections::BTreeMap;

#[derive(Default)]
pub struct Registry {
    counters: BTreeMap<String, u64>,
    histograms: BTreeMap<String, Histogram>,
}

/// Fixed exponential buckets: cheap, allocation-free, and good enough for
/// alerting on tail latency without a full t-digest.
pub struct Histogram {
    bounds_ms: Vec<u64>,
    counts: Vec<u64>,
    sum_ms: u64,
}

impl Default for Histogram {
    fn default() -> Self {
        let bounds_ms = vec![1, 5, 10, 50, 100, 500, 1_000, 5_000];
        Self { counts: vec![0; bounds_ms.len() + 1], bounds_ms, sum_ms: 0 }
    }
}

impl Histogram {
    pub fn observe(&mut self, value_ms: u64) {
        let slot = self.bounds_ms.iter().position(|&b| value_ms <= b).unwrap_or(self.counts.len() - 1);
        self.counts[slot] += 1;
        self.sum_ms += value_ms;
    }

    /// Linear interpolation within the bucket that crosses the quantile.
    pub fn quantile(&self, q: f64) -> u64 {
        let total: u64 = self.counts.iter().sum();
        if total == 0 {
            return 0;
        }
        let target = (total as f64 * q).ceil() as u64;
        let mut seen = 0u64;
        for (i, &count) in self.counts.iter().enumerate() {
            seen += count;
            if seen >= target {
                return self.bounds_ms.get(i).copied().unwrap_or(u64::MAX);
            }
        }
        u64::MAX
    }
}

impl Registry {
    pub fn increment(&mut self, name: &str) {
        *self.counters.entry(name.to_string()).or_insert(0) += 1;
    }

    pub fn observe_latency(&mut self, name: &str, ms: u64) {
        self.histograms.entry(name.to_string()).or_default().observe(ms);
    }

    pub fn render_prometheus(&self) -> String {
        let mut out = String::new();
        for (name, value) in &self.counters {
            out.push_str(&format!("{name}_total {value}\n"));
        }
        for (name, hist) in &self.histograms {
            out.push_str(&format!("{name}_p99_ms {}\n", hist.quantile(0.99)));
        }
        out
    }
}
