//! Rate limits: token buckets keyed by source IP or by target ID (16 §5 item 7).

use std::collections::HashMap;
use std::hash::Hash;
use std::time::{Duration, Instant};

/// A bucket's size and refill speed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rate {
    /// Requests allowed in a burst.
    pub burst: f64,
    /// Tokens added per second.
    pub per_second: f64,
}

impl std::str::FromStr for Rate {
    type Err = String;

    /// `BURST/SECONDS`: a burst of `BURST` requests, then one every
    /// `SECONDS` (fractions allowed), e.g. `60/0.1` or `5/600`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (burst, every) = s
            .split_once('/')
            .ok_or_else(|| format!("expected BURST/SECONDS, got {s:?}"))?;
        let burst: u32 = burst.trim().parse().map_err(|e| format!("burst: {e}"))?;
        let every: f64 = every.trim().parse().map_err(|e| format!("seconds: {e}"))?;
        if burst == 0 || !every.is_finite() || every <= 0.0 {
            return Err("burst and seconds must be positive".into());
        }
        Ok(Rate::new(burst, Duration::from_secs_f64(every)))
    }
}

impl Rate {
    /// `burst` requests at once, then one every `every`.
    pub fn new(burst: u32, every: Duration) -> Self {
        Rate {
            burst: burst as f64,
            per_second: 1.0 / every.as_secs_f64(),
        }
    }
}

/// The server's limits.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Limits {
    /// Every request from one IP before its connection has registered.
    pub per_ip: Rate,
    /// Every request from one registered device. Registered requests are
    /// charged here instead of to the IP, so devices behind one NAT do not
    /// share a budget.
    pub per_device: Rate,
    /// New IDs created from one IP (stops ID-space squatting).
    pub new_ids_per_ip: Rate,
    /// Lookups and connect requests aimed at one ID (stops enumeration and
    /// connect-request spam against one host).
    pub per_target: Rate,
    /// Connect requests aimed at one ID.
    pub connects_per_target: Rate,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            per_ip: Rate::new(60, Duration::from_millis(100)),
            per_device: Rate::new(30, Duration::from_millis(200)),
            new_ids_per_ip: Rate::new(5, Duration::from_secs(600)),
            per_target: Rate::new(30, Duration::from_millis(500)),
            connects_per_target: Rate::new(5, Duration::from_secs(2)),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: f64,
    updated: Instant,
}

/// Token buckets per key.
#[derive(Debug)]
pub struct Buckets<K> {
    rate: Rate,
    buckets: HashMap<K, Bucket>,
}

/// Buckets kept before idle (full) ones are dropped.
const PRUNE_ABOVE: usize = 10_000;

impl<K: Eq + Hash + Clone> Buckets<K> {
    /// Buckets with one rate.
    pub fn new(rate: Rate) -> Self {
        Buckets {
            rate,
            buckets: HashMap::new(),
        }
    }

    fn refill(&self, bucket: &mut Bucket, now: Instant) {
        let elapsed = now.saturating_duration_since(bucket.updated).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * self.rate.per_second).min(self.rate.burst);
        bucket.updated = now;
    }

    /// Takes `cost` tokens from `key`'s bucket, or says how long until it could.
    pub fn take(&mut self, key: &K, cost: f64, now: Instant) -> Result<(), Duration> {
        if self.buckets.len() > PRUNE_ABOVE {
            self.prune(now);
        }
        let mut bucket = self.buckets.get(key).copied().unwrap_or(Bucket {
            tokens: self.rate.burst,
            updated: now,
        });
        self.refill(&mut bucket, now);
        let result = if bucket.tokens >= cost {
            bucket.tokens -= cost;
            Ok(())
        } else {
            let missing = cost - bucket.tokens;
            Err(Duration::from_secs_f64(missing / self.rate.per_second))
        };
        self.buckets.insert(key.clone(), bucket);
        result
    }

    /// Drops buckets that have refilled completely.
    fn prune(&mut self, now: Instant) {
        let rate = self.rate;
        self.buckets.retain(|_, b| {
            let elapsed = now.saturating_duration_since(b.updated).as_secs_f64();
            b.tokens + elapsed * rate.per_second < rate.burst
        });
    }

    /// Number of tracked keys.
    pub fn len(&self) -> usize {
        self.buckets.len()
    }

    /// Whether no key is tracked.
    pub fn is_empty(&self) -> bool {
        self.buckets.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rates_parse_from_text() {
        let r: Rate = "60/0.1".parse().unwrap();
        assert_eq!(r.burst, 60.0);
        assert!((r.per_second - 10.0).abs() < 1e-9);
        assert_eq!(
            "5/600".parse::<Rate>().unwrap(),
            Rate::new(5, Duration::from_secs(600))
        );
        for bad in ["", "60", "0/1", "5/0", "5/-1", "x/1", "5/inf"] {
            assert!(bad.parse::<Rate>().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn burst_then_refill() {
        let mut b = Buckets::new(Rate::new(3, Duration::from_secs(1)));
        let t0 = Instant::now();
        for _ in 0..3 {
            assert!(b.take(&"ip", 1.0, t0).is_ok());
        }
        let wait = b.take(&"ip", 1.0, t0).unwrap_err();
        assert!((wait.as_secs_f64() - 1.0).abs() < 1e-6);
        assert!(b.take(&"ip", 1.0, t0 + Duration::from_secs(1)).is_ok());
        assert!(b.take(&"other", 1.0, t0).is_ok(), "keys are independent");
    }

    #[test]
    fn refill_is_capped_at_burst() {
        let mut b = Buckets::new(Rate::new(2, Duration::from_secs(1)));
        let t0 = Instant::now();
        let later = t0 + Duration::from_secs(1000);
        assert!(b.take(&1, 2.0, later).is_ok());
        assert!(b.take(&1, 1.0, later).is_err());
    }

    #[test]
    fn cost_can_exceed_one() {
        let mut b = Buckets::new(Rate::new(10, Duration::from_secs(1)));
        let t0 = Instant::now();
        assert!(b.take(&1, 8.0, t0).is_ok());
        assert!(b.take(&1, 3.0, t0).is_err());
    }

    #[test]
    fn idle_buckets_are_pruned() {
        let mut b = Buckets::new(Rate::new(1, Duration::from_secs(1)));
        let t0 = Instant::now();
        for i in 0..=PRUNE_ABOVE {
            b.take(&i, 1.0, t0).unwrap();
        }
        // All refilled after a second; the next call prunes them.
        b.take(&usize::MAX, 1.0, t0 + Duration::from_secs(2))
            .unwrap();
        assert_eq!(b.len(), 1);
    }
}
