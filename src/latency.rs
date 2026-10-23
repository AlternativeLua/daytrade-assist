use std::fmt;
use std::time::Duration;

/// Power-of-two bucketed latency histogram; recording is a few instructions and never allocates.
pub struct Histogram {
    buckets: [u64; 64],
    count: u64,
    max_ns: u64,
}

impl Default for Histogram {
    fn default() -> Self {
        Self {
            buckets: [0; 64],
            count: 0,
            max_ns: 0,
        }
    }
}

impl Histogram {
    #[inline]
    pub fn record(&mut self, d: Duration) {
        let ns = d.as_nanos().min(u64::MAX as u128) as u64;
        let b = (64 - ns.leading_zeros() as usize).min(63);
        self.buckets[b] += 1;
        self.count += 1;
        self.max_ns = self.max_ns.max(ns);
    }

    pub fn count(&self) -> u64 {
        self.count
    }

    /// Upper bound of the bucket holding the `q` quantile.
    pub fn quantile_ns(&self, q: f64) -> u64 {
        let target = ((self.count as f64 * q).ceil() as u64).max(1);
        let mut seen = 0;
        for (b, &n) in self.buckets.iter().enumerate() {
            seen += n;
            if seen >= target {
                return if b == 0 { 0 } else { ((1u64 << b) - 1).min(self.max_ns) };
            }
        }
        self.max_ns
    }

    pub fn max_ns(&self) -> u64 {
        self.max_ns
    }
}

pub struct Ns(pub u64);

impl fmt::Display for Ns {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ns = self.0;
        if ns < 1_000 {
            write!(f, "{ns}ns")
        } else if ns < 1_000_000 {
            write!(f, "{:.1}µs", ns as f64 / 1e3)
        } else {
            write!(f, "{:.2}ms", ns as f64 / 1e6)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantiles_bound_samples() {
        let mut h = Histogram::default();
        for ns in [100, 200, 300, 5_000] {
            h.record(Duration::from_nanos(ns));
        }
        assert!(h.quantile_ns(0.5) >= 200);
        assert!(h.quantile_ns(0.5) < 512);
        assert!(h.quantile_ns(1.0) >= 5_000);
        assert_eq!(h.max_ns(), 5_000);
    }
}
