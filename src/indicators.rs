#[derive(Clone, Copy, Debug)]
pub struct Ema {
    k: f64,
    period: u32,
    n: u32,
    seed_sum: f64,
    value: f64,
}

impl Ema {
    pub fn new(period: u32) -> Self {
        Self {
            k: 2.0 / (period as f64 + 1.0),
            period,
            n: 0,
            seed_sum: 0.0,
            value: 0.0,
        }
    }

    /// Seeded with an SMA of the first `period` values.
    #[inline]
    pub fn update(&mut self, x: f64) -> Option<f64> {
        if self.n < self.period {
            self.seed_sum += x;
            self.n += 1;
            if self.n < self.period {
                return None;
            }
            self.value = self.seed_sum / self.period as f64;
        } else {
            self.value += self.k * (x - self.value);
        }
        Some(self.value)
    }
}

/// Wilder's RSI.
#[derive(Clone, Copy, Debug)]
pub struct Rsi {
    period: u32,
    n: u32,
    prev: f64,
    avg_gain: f64,
    avg_loss: f64,
}

impl Rsi {
    pub fn new(period: u32) -> Self {
        Self {
            period,
            n: 0,
            prev: 0.0,
            avg_gain: 0.0,
            avg_loss: 0.0,
        }
    }

    #[inline]
    pub fn update(&mut self, x: f64) -> Option<f64> {
        if self.n == 0 {
            self.prev = x;
            self.n = 1;
            return None;
        }
        let change = x - self.prev;
        self.prev = x;
        let gain = change.max(0.0);
        let loss = (-change).max(0.0);
        let p = self.period as f64;

        if self.n <= self.period {
            self.avg_gain += gain;
            self.avg_loss += loss;
            self.n += 1;
            if self.n <= self.period {
                return None;
            }
            self.avg_gain /= p;
            self.avg_loss /= p;
        } else {
            self.avg_gain = (self.avg_gain * (p - 1.0) + gain) / p;
            self.avg_loss = (self.avg_loss * (p - 1.0) + loss) / p;
        }

        Some(if self.avg_loss == 0.0 {
            if self.avg_gain == 0.0 { 50.0 } else { 100.0 }
        } else {
            100.0 - 100.0 / (1.0 + self.avg_gain / self.avg_loss)
        })
    }
}

/// Rolling mean and population standard deviation over the last `N` values.
#[derive(Clone, Copy, Debug)]
pub struct Bollinger<const N: usize> {
    buf: [f64; N],
    idx: usize,
    len: usize,
}

impl<const N: usize> Bollinger<N> {
    pub fn new() -> Self {
        Self {
            buf: [0.0; N],
            idx: 0,
            len: 0,
        }
    }

    #[inline]
    pub fn update(&mut self, x: f64) -> Option<(f64, f64)> {
        self.buf[self.idx] = x;
        self.idx = (self.idx + 1) % N;
        if self.len < N {
            self.len += 1;
            if self.len < N {
                return None;
            }
        }
        // Two-pass on purpose: FX prices are ~1.0 with variances near 1e-8, so a
        // running sum-of-squares loses most of its precision.
        let mean = self.buf.iter().sum::<f64>() / N as f64;
        let var = self.buf.iter().map(|v| (v - mean) * (v - mean)).sum::<f64>() / N as f64;
        Some((mean, var.sqrt()))
    }
}

/// Fast stochastic: %K over `N` candles, %D as the `S`-period mean of %K.
#[derive(Clone, Copy, Debug)]
pub struct Stochastic<const N: usize, const S: usize> {
    highs: [f64; N],
    lows: [f64; N],
    ks: [f64; S],
    idx: usize,
    len: usize,
    kidx: usize,
    klen: usize,
}

impl<const N: usize, const S: usize> Stochastic<N, S> {
    pub fn new() -> Self {
        Self {
            highs: [0.0; N],
            lows: [0.0; N],
            ks: [0.0; S],
            idx: 0,
            len: 0,
            kidx: 0,
            klen: 0,
        }
    }

    /// Returns `(%K, %D)`.
    #[inline]
    pub fn update(&mut self, high: f64, low: f64, close: f64) -> Option<(f64, f64)> {
        self.highs[self.idx] = high;
        self.lows[self.idx] = low;
        self.idx = (self.idx + 1) % N;
        self.len = (self.len + 1).min(N);
        if self.len < N {
            return None;
        }
        let hh = self.highs.iter().copied().fold(f64::MIN, f64::max);
        let ll = self.lows.iter().copied().fold(f64::MAX, f64::min);
        let k = if hh > ll { 100.0 * (close - ll) / (hh - ll) } else { 50.0 };
        self.ks[self.kidx] = k;
        self.kidx = (self.kidx + 1) % S;
        self.klen = (self.klen + 1).min(S);
        if self.klen < S {
            return None;
        }
        Some((k, self.ks.iter().sum::<f64>() / S as f64))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ema_seeds_with_sma_then_smooths() {
        let mut e = Ema::new(3);
        assert_eq!(e.update(1.0), None);
        assert_eq!(e.update(2.0), None);
        assert_eq!(e.update(3.0), Some(2.0));
        assert_eq!(e.update(4.0), Some(3.0));
    }

    #[test]
    fn rsi_extremes() {
        let mut up = Rsi::new(14);
        let mut last = None;
        for i in 0..20 {
            last = up.update(i as f64);
        }
        assert_eq!(last, Some(100.0));

        let mut flat = Rsi::new(14);
        for _ in 0..20 {
            last = flat.update(1.0);
        }
        assert_eq!(last, Some(50.0));
    }

    #[test]
    fn rsi_matches_reference() {
        // Classic Wilder example series; first RSI(14) is ~70.46.
        let closes = [
            44.34, 44.09, 44.15, 43.61, 44.33, 44.83, 45.10, 45.42, 45.84, 46.08, 45.89, 46.03,
            45.61, 46.28, 46.28,
        ];
        let mut r = Rsi::new(14);
        let mut last = None;
        for c in closes {
            last = r.update(c);
        }
        assert!((last.unwrap() - 70.46).abs() < 0.05, "{last:?}");
    }

    #[test]
    fn stochastic_range() {
        let mut s = Stochastic::<3, 2>::new();
        assert_eq!(s.update(2.0, 1.0, 1.5), None);
        assert_eq!(s.update(3.0, 1.0, 2.0), None);
        assert_eq!(s.update(4.0, 2.0, 4.0), None);
        let (k, d) = s.update(4.0, 2.0, 1.0).unwrap();
        assert_eq!(k, 0.0);
        assert_eq!(d, 50.0);
    }

    #[test]
    fn bollinger_window() {
        let mut b = Bollinger::<4>::new();
        for x in [9.0, 2.0, 4.0, 4.0] {
            b.update(x);
        }
        let (mean, sd) = b.update(6.0).unwrap();
        assert_eq!(mean, 4.0);
        assert!((sd - 2f64.sqrt()).abs() < 1e-12);
    }
}
