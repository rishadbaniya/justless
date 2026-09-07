//! Retry policy for outbound HTTP calls.

use std::time::Duration;

/// How many times a failed download is retried and how long to wait between attempts.
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub base_delay_ms: u64,
}

impl RetryPolicy {
    /// Exponential backoff: the delay doubles after every failed attempt.
    pub fn backoff_ms(&self, attempt: u32) -> u64 {
        self.base_delay_ms.saturating_mul(1 << attempt.min(16))
    }

    /// The wait before an attempt as a duration.
    pub fn delay(&self, attempt: u32) -> Duration {
        Duration::from_millis(self.backoff_ms(attempt))
    }
}
