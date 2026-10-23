use crate::feed::Tick;

pub const MIN_PERIOD_MS: i64 = 1_000;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Candle {
    /// `ts_ms / period_ms` of the candle's start.
    pub slot: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    /// First price in the last sixth of the candle, for reading its final move.
    pub late: Option<f64>,
    pub ticks: u32,
}

/// Builds fixed-period candles from a tick stream.
pub struct Aggregator {
    period_ms: i64,
    late_ms: i64,
    cur: Option<Candle>,
}

impl Aggregator {
    pub fn new(period_ms: i64) -> Self {
        let period_ms = period_ms.max(MIN_PERIOD_MS);
        Self {
            period_ms,
            late_ms: period_ms * 5 / 6,
            cur: None,
        }
    }

    pub fn period_ms(&self) -> i64 {
        self.period_ms
    }

    #[inline]
    pub fn slot(&self, ts_ms: i64) -> i64 {
        ts_ms.div_euclid(self.period_ms)
    }

    /// Whether `ts_ms` falls in the last sixth of its candle.
    #[inline]
    pub fn is_late(&self, ts_ms: i64) -> bool {
        ts_ms.rem_euclid(self.period_ms) >= self.late_ms
    }

    #[inline]
    fn open(&self, slot: i64, tick: Tick) -> Candle {
        Candle {
            slot,
            open: tick.price,
            high: tick.price,
            low: tick.price,
            close: tick.price,
            late: self.is_late(tick.ts_ms).then_some(tick.price),
            ticks: 1,
        }
    }

    /// Returns the previous candle when `tick` is the first of a new one.
    /// Ticks from a candle that has already closed are dropped.
    #[inline]
    pub fn push(&mut self, tick: Tick) -> Option<Candle> {
        let slot = self.slot(tick.ts_ms);
        let late = self.is_late(tick.ts_ms);
        let Some(c) = &mut self.cur else {
            self.cur = Some(self.open(slot, tick));
            return None;
        };
        if slot == c.slot {
            c.high = c.high.max(tick.price);
            c.low = c.low.min(tick.price);
            c.close = tick.price;
            c.ticks += 1;
            if c.late.is_none() && late {
                c.late = Some(tick.price);
            }
            None
        } else if slot > c.slot {
            let done = *c;
            self.cur = Some(self.open(slot, tick));
            Some(done)
        } else {
            None
        }
    }

    pub fn current(&self) -> Option<&Candle> {
        self.cur.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(ts_ms: i64, price: f64) -> Tick {
        Tick { ts_ms, price }
    }

    #[test]
    fn closes_on_period_boundary() {
        let mut a = Aggregator::new(60_000);
        assert_eq!(a.push(t(0, 1.0)), None);
        assert_eq!(a.push(t(10_000, 3.0)), None);
        assert_eq!(a.push(t(20_000, 0.5)), None);
        assert_eq!(a.push(t(59_999, 2.0)), None);
        let c = a.push(t(60_000, 9.0)).unwrap();
        assert_eq!((c.slot, c.open, c.high, c.low, c.close, c.ticks), (0, 1.0, 3.0, 0.5, 2.0, 4));
        assert_eq!(c.late, Some(2.0));
        assert_eq!(a.current().unwrap().open, 9.0);
    }

    #[test]
    fn short_periods() {
        let mut a = Aggregator::new(15_000);
        a.push(t(0, 1.0));
        a.push(t(12_600, 2.0));
        let c = a.push(t(15_000, 3.0)).unwrap();
        assert_eq!((c.slot, c.close, c.late), (0, 2.0, Some(2.0)));
        assert_eq!(a.current().unwrap().slot, 1);
    }

    #[test]
    fn drops_late_ticks() {
        let mut a = Aggregator::new(60_000);
        a.push(t(60_000, 1.0));
        assert_eq!(a.push(t(59_000, 5.0)), None);
        assert_eq!(a.current().unwrap().high, 1.0);
    }
}
