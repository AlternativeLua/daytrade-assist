//! Adaptive rule selection for next-candle direction.
//!
//! OTC prices come from each broker's own generator, so no fixed indicator
//! recipe can be trusted to carry over between brokers, pairs or days. Instead,
//! a pool of simple rules (each in a "follow" and a "fade" version) votes on
//! every candle. Every vote is scored against the next candle whether or not it
//! is traded, and a rule is only traded while the lower confidence bound of its
//! recent win rate clears the payout's break-even rate.
//!
//! Picking the best of 24 rules flatters whichever one got lucky lately, and on
//! a feed with no lasting pattern that is usually a trend-follower riding the
//! last move. So the engine also keeps a track record of its own picks, scored
//! on the next candle like everything else. That record is out of sample by
//! construction, and nothing is traded unless it too clears break-even.

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

/// How readily the engine trades. Each level loosens the rules and the track
/// record together; looser levels trade more of the candles they're unsure of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Frequency {
    /// Rules and the track record must both beat break-even with confidence.
    Careful,
    /// Rules with confidence; the track record's plain win rate beats break-even.
    Balanced,
    /// Rules' and the track record's plain win rates beat break-even.
    Active,
    /// Rules with confidence, no track record check (how the app traded before it had one).
    Unchecked,
}

impl Frequency {
    pub const ALL: [Frequency; 4] = [Frequency::Careful, Frequency::Balanced, Frequency::Active, Frequency::Unchecked];

    pub fn name(self) -> &'static str {
        match self {
            Frequency::Careful => "careful",
            Frequency::Balanced => "balanced",
            Frequency::Active => "active",
            Frequency::Unchecked => "unchecked",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|f| f.name() == s.trim())
    }

    /// The level's `z`, `track_z` and `track_min`.
    pub fn levels(self) -> (f64, f64, f64) {
        match self {
            Frequency::Careful => (1.0, 1.0, 40.0),
            Frequency::Balanced => (1.0, 0.0, 20.0),
            Frequency::Active => (0.0, 0.0, 20.0),
            Frequency::Unchecked => (1.0, 1.0, 0.0),
        }
    }

    /// Applies the level to `p`.
    pub fn apply(self, p: Params) -> Params {
        let (z, track_z, track_min) = self.levels();
        Params { z, track_z, track_min, ..p }
    }

    /// The level `p` is set to, if it matches one.
    pub fn of(p: &Params) -> Option<Self> {
        Self::ALL.into_iter().find(|f| f.levels() == (p.z, p.track_z, p.track_min))
    }
}

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
    /// Picks after which an old pick's result counts half in the track record.
    pub track_half_life: f64,
    /// Effective picks the track record needs before anything is traded; 0 turns the check off.
    pub track_min: f64,
    /// Confidence z for the track record's lower bound; 0 trades while its raw win rate beats break-even.
    pub track_z: f64,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            half_life: 400.0,
            z: 1.0,
            min_samples: 50.0,
            margin: 0.0,
            track_half_life: 300.0,
            track_min: 40.0,
            track_z: 1.0,
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
    /// Rebuilds a record from saved training.
    pub fn from_parts(w: f64, n: f64, wins: u32, total: u32) -> Self {
        Self { w, n, wins, total }
    }

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

/// A rule's standing, independent of what it votes on the next candle.
#[derive(Clone, Copy, Debug)]
pub struct RuleScore {
    pub expert: usize,
    pub rate: f64,
    pub lower: f64,
    pub n: f64,
}

/// Why the engine isn't trading the next candle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hold {
    /// No rule has enough votes yet.
    Learning,
    /// No rule's lower bound clears break-even.
    NoEdge,
    /// Qualified rules point both ways.
    Conflict,
    /// A rule qualifies, but the engine's own picks haven't been proven yet.
    Proving,
    /// A rule qualifies, but the engine's own recent picks don't beat break-even.
    Unproven,
}

impl Hold {
    pub fn name(self) -> &'static str {
        match self {
            Hold::Learning => "learning",
            Hold::NoEdge => "no-edge",
            Hold::Conflict => "conflict",
            Hold::Proving => "proving",
            Hold::Unproven => "unproven",
        }
    }
}

/// How the engine's own picks have done, traded or not.
#[derive(Clone, Copy, Debug)]
pub struct Track {
    /// Recency-weighted win rate.
    pub rate: f64,
    pub lower: f64,
    /// Recency-weighted picks.
    pub n: f64,
    pub wins: u32,
    pub total: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct Signal {
    pub side: Option<Side>,
    /// Why there is no side; `None` when trading.
    pub hold: Option<Hold>,
    pub track: Track,
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
    track_decay: f64,
    threshold: f64,
    ind: Indicators,
    records: [Record; EXPERTS],
    pending: [i8; EXPERTS],
    /// The engine's own picks, scored whether or not they were traded.
    track: Record,
    /// Direction picked for the next candle (+1, -1, or 0 for none).
    track_vote: i8,
    /// Candles learned from, including ones learned in earlier sessions.
    candles: u64,
}

impl Engine {
    pub fn new(p: Params, breakeven: f64) -> Self {
        Self {
            p,
            decay: 0.5f64.powf(1.0 / p.half_life.max(1.0)),
            track_decay: 0.5f64.powf(1.0 / p.track_half_life.max(1.0)),
            threshold: breakeven + p.margin,
            ind: Indicators::new(),
            records: [Record::default(); EXPERTS],
            pending: [0; EXPERTS],
            track: Record::default(),
            track_vote: 0,
            candles: 0,
        }
    }

    /// Break-even win rate a rule must beat, for when the payout changes.
    pub fn set_breakeven(&mut self, breakeven: f64) {
        self.threshold = breakeven + self.p.margin;
    }

    /// Changes how readily it trades; what has been learned is kept.
    pub fn set_frequency(&mut self, p: &Params) {
        self.p.z = p.z;
        self.p.track_z = p.track_z;
        self.p.track_min = p.track_min;
    }

    pub fn records(&self) -> &[Record; EXPERTS] {
        &self.records
    }

    pub fn candles(&self) -> u64 {
        self.candles
    }

    pub fn track_record(&self) -> Record {
        self.track
    }

    /// The engine's own picks so far, and how sure that record is.
    pub fn track(&self) -> Track {
        let t = &self.track;
        Track {
            rate: if t.n > 0.0 { t.w / t.n } else { 0.0 },
            lower: wilson_lower(t.w, t.n, self.p.track_z),
            n: t.n,
            wins: t.wins,
            total: t.total,
        }
    }

    /// Whether the engine's own picks are beating break-even with confidence.
    pub fn proven(&self) -> bool {
        self.p.track_min <= 0.0 || (self.track.n >= self.p.track_min && self.track().lower > self.threshold)
    }

    /// Replaces what has been learned with saved training. Indicators and the
    /// votes waiting for the next candle are kept, and so is the track record
    /// when the saved training has none (it was saved before there was one).
    pub fn restore(&mut self, records: [Record; EXPERTS], candles: u64, track: Option<Record>) {
        self.records = records;
        self.candles = candles;
        if let Some(t) = track {
            self.track = t;
        }
    }

    /// The rule with the highest lower bound among those with enough votes.
    pub fn strongest(&self) -> Option<RuleScore> {
        self.records
            .iter()
            .enumerate()
            .filter(|(_, r)| r.n >= self.p.min_samples)
            .map(|(expert, r)| RuleScore {
                expert,
                rate: r.w / r.n,
                lower: wilson_lower(r.w, r.n, self.p.z),
                n: r.n,
            })
            .max_by(|a, b| a.lower.total_cmp(&b.lower))
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
        self.candles += 1;
        let color = sign(c.close - c.open);
        if color != 0 {
            for (rec, &vote) in self.records.iter_mut().zip(&self.pending) {
                if vote != 0 {
                    rec.record(vote == color, self.decay);
                }
            }
            if self.track_vote != 0 {
                self.track.record(self.track_vote == color, self.track_decay);
            }
        }

        let (bases, rsi, z) = self.ind.update(c);
        let most = self.records.iter().map(|r| r.n).fold(0.0, f64::max);
        let mut sig = Signal {
            side: None,
            hold: Some(if most < self.p.min_samples { Hold::Learning } else { Hold::NoEdge }),
            track: self.track(),
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
            (Some(_), Some(_)) => {
                sig.hold = Some(Hold::Conflict);
                None
            }
            _ => None,
        };
        self.track_vote = chosen.map_or(0, |p| if p.side == Side::Call { 1 } else { -1 });
        if let Some(pick) = chosen {
            sig.best = Some(pick);
            if self.proven() {
                sig.side = Some(pick.side);
                sig.hold = None;
            } else {
                sig.hold = Some(if self.track.n < self.p.track_min { Hold::Proving } else { Hold::Unproven });
            }
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

    /// Coin-flip candles from a fixed seed.
    fn noise(e: &mut Engine, n: i64) -> (u32, u32) {
        let mut rng = 0x9E37_79B9_7F4A_7C15u64;
        let (mut traded, mut picked) = (0, 0);
        for i in 0..n {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            let up = rng & 1 == 1;
            let sig = e.on_close(&candle(i, 1.0, if up { 1.0001 } else { 0.9999 }));
            traded += sig.side.is_some() as u32;
            picked += sig.best.is_some() as u32;
        }
        (traded, picked)
    }

    #[test]
    fn stands_aside_on_noise_its_picks_cant_beat() {
        // Short memory and loose selectivity: some lucky rule qualifies on noise most of the time.
        let p = Params { z: 0.0, half_life: 60.0, min_samples: 30.0, ..Params::default() };
        let mut gated = Engine::new(p, 1.0 / 1.85);
        let mut ungated = Engine::new(Params { track_min: 0.0, ..p }, 1.0 / 1.85);
        let (traded, _) = noise(&mut gated, 6000);
        let (traded_before, _) = noise(&mut ungated, 6000);
        assert!(traded_before > 300, "the rules alone trade noise: {traded_before}");
        assert!(traded * 4 < traded_before, "the track record should cut most of it: {traded} vs {traded_before}");
        assert!(gated.track().total > 300, "picks are still scored while holding");
    }

    #[test]
    fn restore_keeps_the_track_record_when_none_was_saved() {
        let mut e = Engine::new(Params { z: 0.0, half_life: 60.0, min_samples: 30.0, ..Params::default() }, 0.54);
        noise(&mut e, 2000);
        let before = e.track().total;
        assert!(before > 0);
        e.restore(*e.records(), e.candles(), None);
        assert_eq!(e.track().total, before);
        e.restore(*e.records(), e.candles(), Some(Record::default()));
        assert_eq!(e.track().total, 0);
    }

    #[test]
    fn holds_while_learning() {
        let mut e = Engine::new(Params::default(), 0.54);
        for i in 0..40 {
            let sig = e.on_close(&candle(i, 1.0, 1.0 + if i % 2 == 0 { 1e-4 } else { -1e-4 }));
            assert!(sig.side.is_none());
            assert!(sig.learning.is_some());
            assert_eq!(sig.hold, Some(Hold::Learning));
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
