//! Retry policy: exponential backoff with jitter and a circuit breaker.

use std::time::Duration;

const MAX_DELAY_MS: u64 = 30_000;
const MAX_ATTEMPTS: u32 = 5;

/// Delay before attempt `n`, doubling from `base_ms` and capped.
pub fn compute_backoff_delay(attempt: u32, base_ms: u64) -> Duration {
    let scaled = base_ms.saturating_mul(2u64.saturating_pow(attempt));
    Duration::from_millis(scaled.min(MAX_DELAY_MS))
}

/// Spread retries so a fleet of workers does not stampede a recovering service.
pub fn apply_jitter(delay: Duration, seed: u64) -> Duration {
    let span = delay.as_millis() as u64 / 4;
    if span == 0 {
        return delay;
    }
    let offset = seed % (span * 2);
    Duration::from_millis(delay.as_millis() as u64 - span + offset)
}

pub fn should_retry(status: u16, attempt: u32) -> bool {
    attempt < MAX_ATTEMPTS && (status == 429 || status >= 500)
}

/// Trips after repeated failures so callers fail fast instead of queueing.
pub struct CircuitBreaker {
    failures: u32,
    threshold: u32,
}

impl CircuitBreaker {
    pub fn new(threshold: u32) -> Self {
        Self { failures: 0, threshold }
    }

    pub fn record_failure(&mut self) {
        self.failures = self.failures.saturating_add(1);
    }

    pub fn record_success(&mut self) {
        self.failures = 0;
    }

    pub fn is_open(&self) -> bool {
        self.failures >= self.threshold
    }
}
