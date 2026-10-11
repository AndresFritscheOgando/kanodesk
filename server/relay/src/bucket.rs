//! Byte-rate token bucket for per-session bandwidth limits.

use std::time::Instant;

/// Allows `rate` bytes per second with a burst of one second's worth.
#[derive(Debug, Clone)]
pub struct ByteBucket {
    rate: f64,
    tokens: f64,
    updated: Instant,
}

impl ByteBucket {
    /// A full bucket.
    pub fn new(bytes_per_second: u64, now: Instant) -> Self {
        let rate = bytes_per_second.max(1) as f64;
        ByteBucket {
            rate,
            tokens: rate,
            updated: now,
        }
    }

    /// Spends `bytes` if available; returns whether the datagram may pass.
    pub fn take(&mut self, bytes: usize, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.updated).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.rate).min(self.rate);
        self.updated = now;
        if self.tokens >= bytes as f64 {
            self.tokens -= bytes as f64;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn burst_then_rate() {
        let t0 = Instant::now();
        let mut b = ByteBucket::new(1000, t0);
        assert!(b.take(600, t0));
        assert!(b.take(400, t0));
        assert!(!b.take(1, t0), "burst used up");
        assert!(b.take(500, t0 + Duration::from_millis(500)));
        assert!(!b.take(100, t0 + Duration::from_millis(500)));
    }

    #[test]
    fn refill_is_capped() {
        let t0 = Instant::now();
        let mut b = ByteBucket::new(1000, t0);
        assert!(
            !b.take(1001, t0 + Duration::from_secs(60)),
            "never more than one second's worth"
        );
    }
}
