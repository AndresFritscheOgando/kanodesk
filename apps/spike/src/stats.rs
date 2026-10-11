//! Small timing statistics for the spike's per-second and summary reports.

/// A list of samples (milliseconds) with summary statistics.
#[derive(Debug, Default, Clone)]
pub struct Samples {
    values: Vec<f64>,
}

impl Samples {
    /// Adds a sample.
    pub fn push(&mut self, value: f64) {
        self.values.push(value);
    }

    /// Number of samples.
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Whether there are no samples.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Removes all samples.
    pub fn clear(&mut self) {
        self.values.clear();
    }

    /// Mean, or 0 when empty.
    pub fn avg(&self) -> f64 {
        if self.values.is_empty() {
            return 0.0;
        }
        self.values.iter().sum::<f64>() / self.values.len() as f64
    }

    /// Value at quantile `q` in 0..=1 (nearest rank), or 0 when empty.
    pub fn quantile(&self, q: f64) -> f64 {
        if self.values.is_empty() {
            return 0.0;
        }
        let mut sorted = self.values.clone();
        sorted.sort_by(f64::total_cmp);
        let rank = ((sorted.len() - 1) as f64 * q.clamp(0.0, 1.0)).round() as usize;
        sorted[rank]
    }

    /// 95th percentile.
    pub fn p95(&self) -> f64 {
        self.quantile(0.95)
    }

    /// Largest sample, or 0 when empty.
    pub fn max(&self) -> f64 {
        self.values.iter().copied().fold(0.0, f64::max)
    }
}

/// Milliseconds between two microsecond timestamps; negative if `to < from`.
pub fn ms_between(from_us: u64, to_us: u64) -> f64 {
    (to_us as i64 - from_us as i64) as f64 / 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_is_all_zero() {
        let s = Samples::default();
        assert!(s.is_empty());
        assert_eq!((s.avg(), s.p95(), s.max()), (0.0, 0.0, 0.0));
    }

    #[test]
    fn summary_values() {
        let mut s = Samples::default();
        for v in 1..=100 {
            s.push(v as f64);
        }
        assert_eq!(s.len(), 100);
        assert_eq!(s.avg(), 50.5);
        assert_eq!(s.max(), 100.0);
        assert_eq!(s.p95(), 95.0);
        assert_eq!(s.quantile(0.0), 1.0);
        s.clear();
        assert!(s.is_empty());
    }

    #[test]
    fn ms_between_handles_order() {
        assert_eq!(ms_between(1_000, 3_500), 2.5);
        assert_eq!(ms_between(3_500, 1_000), -2.5);
    }
}
