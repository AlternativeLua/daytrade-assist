//! Adaptive rule selection for next-candle direction.
//!
//! OTC prices come from each broker's own generator, so no fixed indicator
//! recipe can be trusted to carry over between brokers, pairs or days. Instead,
//! a pool of simple rules (each in a "follow" and a "fade" version) votes on
//! every candle. Every vote is scored against the next candle whether or not it
//! is traded, and a rule is only traded while the lower confidence bound of its
//! recent win rate clears the payout's break-even rate.

use crate::candle::Candle;
use crate::indicators::{Bollinger, Ema, Rsi, Stochastic};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Call,
    Put,
}

pub const BASES: [&str; 12] = [
    "last-candle",
    "streak-3",
    "last-10s",
    "rsi2",
    "rsi14",
    "band-2σ",
    "band-1σ",
    "stoch",
    "ema-trend",
    "wick",
    "trend-pullback",
    "close-in-range",
];
pub const EXPERTS: usize = BASES.len() * 2;

/// Rules alternate follow/fade: rule `2i` trades base signal `i`, `2i+1` trades against it.
pub fn expert_name(i: usize) -> (&'static str, &'static str) {
    (if i % 2 == 0 { "follow" } else { "fade" }, BASES[i / 2])
}

const TREND_MIN: f64 = 1.0;
/// When qualified rules disagree, hold unless one side's bound leads by this much.
const CONFLICT_GAP: f64 = 0.01;

#[derive(Clone, Copy, Debug)]
pub struct Params {
    /// Votes after which an old result counts half as much as a new one.
    pub half_life: f64,
    /// Confidence z for the Wilson lower bound; 0 trades on the raw win rate.
    pub z: f64,
    /// Effective votes a rule needs before it can be traded.
    pub min_samples: f64,
    /// Extra edge required above break-even.
    pub margin: f64,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            half_life: 400.0,
            z: 1.0,
            min_samples: 50.0,
            margin: 0.0,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Record {
    w: f64,
    n: f64,
    pub wins: u32,
    pub total: u32,
}

impl Record {
    #[inline]
    fn record(&mut self, win: bool, decay: f64) {
        self.w = self.w * decay + win as u8 as f64;
        self.n = self.n * decay + 1.0;
        self.wins += win as u32;
        self.total += 1;
    }

    /// Recency-weighted wins.
    pub fn recent_wins(&self) -> f64 {
        self.w
    }

    /// Recency-weighted vote count.
    pub fn weight(&self) -> f64 {
        self.n
    }
}

pub fn wilson_lower(wins: f64, n: f64, z: f64) -> f64 {
    if n <= 0.0 {
        return 0.0;
    }
    let p = wins / n;
    let z2 = z * z;
    (p + z2 / (2.0 * n) - z * (p * (1.0 - p) / n + z2 / (4.0 * n * n)).sqrt()) / (1.0 + z2 / n)
}

#[derive(Clone, Copy, Debug)]
pub struct Pick {
    pub expert: usize,
    pub side: Side,
    /// Recency-weighted win rate.
    pub rate: f64,
    pub lower: f64,
    pub n: f64,
}

#[derive(Clone, Copy, Debug)]
pub struct Signal {
    pub side: Option<Side>,
    /// The traded rule, or the strongest candidate when holding.
    pub best: Option<Pick>,
    pub voting: u8,
    /// Most effective votes any rule has, while still below `min_samples`.
    pub learning: Option<f64>,
    pub rsi: f64,
    pub z: f64,
    /// Base signals in `BASES` order (+1 up, -1 down, 0 none), for the decision log.
    pub bases: [i8; BASES.len()],
}

#[derive(Clone, Copy)]
struct Indicators {
    ema_fast: Ema,
    ema_slow: Ema,
    rsi14: Rsi,
    rsi2: Rsi,
    bands: Bollinger<20>,
    stoch: Stochastic<5, 3>,
    prev_fast: Option<f64>,
    streak: i32,
}

#[inline]
fn sign(x: f64) -> i8 {
    (x > 0.0) as i8 - (x < 0.0) as i8
}

#[inline]
fn extreme(x: f64, low: f64, high: f64) -> i8 {
    if x >= high {
        1
    } else if x <= low {
        -1
    } else {
        0
    }
}

impl Indicators {
    fn new() -> Self {
        Self {
            ema_fast: Ema::new(9),
            ema_slow: Ema::new(21),
            rsi14: Rsi::new(14),
            rsi2: Rsi::new(2),
            bands: Bollinger::new(),
            stoch: Stochastic::new(),
            prev_fast: None,
            streak: 0,
        }
    }

    /// Base signals in `BASES` order: +1 points up, -1 down, 0 no opinion.
    #[inline]
    fn update(&mut self, c: &Candle) -> ([i8; BASES.len()], f64, f64) {
        let fast = self.ema_fast.update(c.close);
        let slow = self.ema_slow.update(c.close);
        let rsi14 = self.rsi14.update(c.close);
        let rsi2 = self.rsi2.update(c.close);
        let bands = self.bands.update(c.close);
        let stoch = self.stoch.update(c.high, c.low, c.close);
        let prev_fast = std::mem::replace(&mut self.prev_fast, fast);

        let color = sign(c.close - c.open);
        self.streak = match color {
            0 => 0,
            _ if self.streak.signum() == color as i32 => self.streak + color as i32,
            _ => color as i32,
        };
        let z = bands.map(|(mean, sd)| if sd > 0.0 { (c.close - mean) / sd } else { 0.0 });

        let mut b = [0i8; BASES.len()];
        b[0] = color;
        b[1] = if self.streak.abs() >= 3 { color } else { 0 };
        b[2] = c.late.map_or(0, |late| sign(c.close - late));
        b[3] = rsi2.map_or(0, |r| extreme(r, 10.0, 90.0));
        b[4] = rsi14.map_or(0, |r| extreme(r, 30.0, 70.0));
        b[5] = z.map_or(0, |z| extreme(z, -2.0, 2.0));
        b[6] = z.map_or(0, |z| extreme(z, -1.0, 1.0));
        b[7] = stoch.map_or(0, |(_, d)| extreme(d, 20.0, 80.0));

        if let (Some(f), Some(s), Some(pf)) = (fast, slow, prev_fast) {
            let dir = sign(f - s);
            let sloping = sign(f - pf) == dir;
            if sloping {
                b[8] = dir;
            }
            if let (Some(r), Some(z), Some((_, sd))) = (rsi14, z, bands) {
                let trending = sd > 0.0 && ((f - s) / sd).abs() >= TREND_MIN;
                let pulled_back = if dir > 0 { r < 50.0 || z < 0.0 } else { r > 50.0 || z > 0.0 };
                if trending && sloping && pulled_back && color == dir {
                    b[10] = dir;
                }
            }
        }

        let range = c.high - c.low;
        if range > 0.0 {
            let upper = c.high - c.open.max(c.close);
            let lower = c.open.min(c.close) - c.low;
            if upper >= 0.5 * range && upper > 2.0 * lower {
                b[9] = -1;
            } else if lower >= 0.5 * range && lower > 2.0 * upper {
                b[9] = 1;
            }
            b[11] = extreme((c.close - c.low) / range, 0.2, 0.8);
        }

        (b, rsi14.unwrap_or(f64::NAN), z.unwrap_or(f64::NAN))
    }
}

/// Fixed-size and `Copy`, so a decision never allocates and `peek` is a memcpy.
#[derive(Clone, Copy)]
pub struct Engine {
    p: Params,
    decay: f64,
    threshold: f64,
    ind: Indicators,
    records: [Record; EXPERTS],
    pending: [i8; EXPERTS],
}

impl Engine {
    pub fn new(p: Params, breakeven: f64) -> Self {
        Self {
            p,
            decay: 0.5f64.powf(1.0 / p.half_life.max(1.0)),
            threshold: breakeven + p.margin,
            ind: Indicators::new(),
            records: [Record::default(); EXPERTS],
            pending: [0; EXPERTS],
        }
    }

    /// Break-even win rate a rule must beat, for when the payout changes.
    pub fn set_breakeven(&mut self, breakeven: f64) {
        self.threshold = breakeven + self.p.margin;
    }

    pub fn set_z(&mut self, z: f64) {
        self.p.z = z;
    }

    pub fn records(&self) -> &[Record; EXPERTS] {
        &self.records
    }

    /// What `on_close` would say if `forming` closed now, without committing it.
    #[inline]
    pub fn peek(&self, forming: &Candle) -> Signal {
        let mut e = *self;
        e.on_close(forming)
    }

    /// Scores the previous votes against `c`, then votes on the candle after it.
    #[inline]
    pub fn on_close(&mut self, c: &Candle) -> Signal {
        let color = sign(c.close - c.open);
        if color != 0 {
            for (rec, &vote) in self.records.iter_mut().zip(&self.pending) {
                if vote != 0 {
                    rec.record(vote == color, self.decay);
                }
            }
        }

        let (bases, rsi, z) = self.ind.update(c);
        let most = self.records.iter().map(|r| r.n).fold(0.0, f64::max);
        let mut sig = Signal {
            side: None,
            best: None,
            voting: 0,
            learning: (most < self.p.min_samples).then_some(most),
            rsi,
            z,
            bases,
        };
        let mut best_call: Option<Pick> = None;
        let mut best_put: Option<Pick> = None;

        for i in 0..EXPERTS {
            let vote = if i % 2 == 0 { bases[i / 2] } else { -bases[i / 2] };
            self.pending[i] = vote;
            if vote == 0 {
                continue;
            }
            sig.voting += 1;
            let rec = &self.records[i];
            if rec.n < self.p.min_samples {
                continue;
            }
            let pick = Pick {
                expert: i,
                side: if vote > 0 { Side::Call } else { Side::Put },
                rate: rec.w / rec.n,
                lower: wilson_lower(rec.w, rec.n, self.p.z),
                n: rec.n,
            };
            if sig.best.is_none_or(|b| pick.lower > b.lower) {
                sig.best = Some(pick);
            }
            if pick.lower > self.threshold {
                let slot = if vote > 0 { &mut best_call } else { &mut best_put };
                if slot.is_none_or(|b| pick.lower > b.lower) {
                    *slot = Some(pick);
                }
            }
        }

        let chosen = match (best_call, best_put) {
            (Some(c), None) => Some(c),
            (None, Some(p)) => Some(p),
            (Some(c), Some(p)) if (c.lower - p.lower).abs() > CONFLICT_GAP => {
                Some(if c.lower > p.lower { c } else { p })
            }
            _ => None,
        };
        if let Some(pick) = chosen {
            sig.side = Some(pick.side);
            sig.best = Some(pick);
        }
        sig
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candle(minute: i64, open: f64, close: f64) -> Candle {
        Candle {
            slot: minute,
            open,
            high: open.max(close),
            low: open.min(close),
            close,
            late: None,
            ticks: 1,
        }
    }

    #[test]
    fn wilson_reference_values() {
        assert!((wilson_lower(60.0, 100.0, 1.96) - 0.502).abs() < 0.001);
        assert_eq!(wilson_lower(60.0, 100.0, 0.0), 0.6);
    }

    #[test]
    fn learns_to_fade_an_alternating_feed() {
        let mut e = Engine::new(Params::default(), 1.0 / 1.85);
        let mut price = 1.0;
        let mut last = None;
        for i in 0..120 {
            let next = if i % 2 == 0 { price + 1e-4 } else { price - 1e-4 };
            last = Some((e.on_close(&candle(i, price, next)), next > price));
            price = next;
        }
        let (sig, last_green) = last.unwrap();
        assert!(sig.learning.is_none());
        assert_eq!(sig.side, Some(if last_green { Side::Put } else { Side::Call }), "{sig:?}");
        assert_eq!(expert_name(sig.best.unwrap().expert).0, "fade");
    }

    #[test]
    fn holds_while_learning() {
        let mut e = Engine::new(Params::default(), 0.54);
        for i in 0..40 {
            let sig = e.on_close(&candle(i, 1.0, 1.0 + if i % 2 == 0 { 1e-4 } else { -1e-4 }));
            assert!(sig.side.is_none());
            assert!(sig.learning.is_some());
        }
    }

    #[test]
    fn peek_does_not_mutate() {
        let mut e = Engine::new(Params::default(), 0.54);
        for i in 0..80 {
            e.on_close(&candle(i, 1.0, 1.0 + (i % 3) as f64 * 1e-4));
        }
        let before = e.records()[0].total;
        let probe = candle(80, 1.0, 0.99);
        let (a, b) = (e.peek(&probe), e.peek(&probe));
        assert_eq!(e.records()[0].total, before);
        assert_eq!(a.side, b.side);
        assert_eq!(a.z, b.z);
    }
}
