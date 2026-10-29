use std::collections::HashMap;
use std::io::BufRead;
use std::sync::mpsc::{Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::power::Awake;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tick {
    pub ts_ms: i64,
    pub price: f64,
}

pub trait Feed {
    fn next_tick(&mut self) -> Option<Tick>;
}

pub type AssetId = u16;

/// Everything the engine thread consumes, from every source, in arrival order.
pub enum Input {
    Tick(AssetId, Tick),
    /// Older ticks (chart history, saved logs) to learn from without trading;
    /// `save` appends them to the asset's tick log.
    Backfill { asset: AssetId, ticks: Vec<Tick>, save: bool },
    Select { asset: AssetId, period_ms: i64 },
    /// Current payout (0.92 = 92%) and whether each asset is open for trading.
    Payouts(Vec<(AssetId, f64, bool)>),
    Settings(SettingsChange),
    /// The Pocket Option account balance the bridge saw, for the demo or the real account.
    Balance { demo: bool, balance: f64 },
    /// Save training and flush logs, then reply; sent before the app quits.
    Flush(Sender<()>),
    /// A finite source (file, stdin, instant sim) has finished.
    SourceDone,
}

/// Settings changed from the UI; `None` leaves a setting as it is.
#[derive(Clone, Debug, Default)]
pub struct SettingsChange {
    pub martingale_steps: Option<u32>,
    pub martingale_mult: Option<f64>,
    pub z: Option<f64>,
    /// A trade frequency level, which sets `z` and the track record check together.
    pub frequency: Option<crate::strategy::Frequency>,
    pub stake: Option<f64>,
    /// Stake as a % of the balance; 0 goes back to the fixed `stake`.
    pub stake_pct: Option<f64>,
    pub mode: Option<TradeMode>,
    pub paused: Option<bool>,
    /// Real-money losses (in account currency) after which real trading switches itself off.
    pub loss_limit: Option<f64>,
    pub awake: Option<Awake>,
    /// Trade any streaming pair with a qualified signal, not just the selected one.
    pub auto: Option<bool>,
    /// Restart the paper account at this balance.
    pub balance: Option<f64>,
    /// Follow the Pocket Option account's balance instead of a paper one.
    pub sync: Option<bool>,
    /// Pairs auto mode may trade; empty means every pair.
    pub auto_list: Option<Vec<String>>,
}

/// Where signals go besides the paper account.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TradeMode {
    Off,
    Demo,
    Real,
}

impl TradeMode {
    pub fn name(self) -> &'static str {
        match self {
            TradeMode::Off => "off",
            TradeMode::Demo => "demo",
            TradeMode::Real => "real",
        }
    }
}

/// Interns asset names so ticks carry a small id instead of a string.
#[derive(Default)]
pub struct Registry {
    inner: Mutex<(HashMap<String, AssetId>, Vec<String>)>,
}

impl Registry {
    pub fn id(&self, name: &str) -> AssetId {
        let mut g = self.inner.lock().unwrap();
        if let Some(&id) = g.0.get(name) {
            return id;
        }
        let id = g.1.len() as AssetId;
        g.1.push(name.to_owned());
        g.0.insert(name.to_owned(), id);
        id
    }

    pub fn find(&self, name: &str) -> Option<AssetId> {
        self.inner.lock().unwrap().0.get(name).copied()
    }

    pub fn name(&self, id: AssetId) -> String {
        self.inner.lock().unwrap().1.get(id as usize).cloned().unwrap_or_default()
    }
}

/// Per-thread cache in front of the registry, so the hot path never locks.
pub struct AssetCache {
    registry: Arc<Registry>,
    ids: HashMap<String, AssetId>,
}

impl AssetCache {
    pub fn new(registry: Arc<Registry>) -> Self {
        Self {
            registry,
            ids: HashMap::new(),
        }
    }

    #[inline]
    pub fn id(&mut self, name: &str) -> AssetId {
        if let Some(&id) = self.ids.get(name) {
            return id;
        }
        let id = self.registry.id(name);
        self.ids.insert(name.to_owned(), id);
        id
    }
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

pub struct Line<'a> {
    pub asset: Option<&'a str>,
    pub tick: Tick,
}

/// Line protocol shared by stdin, TCP, file replay and the browser bridge. A line
/// is `price` (stamped on arrival), `timestamp,price` or `asset,timestamp,price`;
/// whitespace and `;` also separate. Timestamps below 1e11 are seconds, otherwise
/// milliseconds. Blank lines, `#` comments and headers are skipped.
pub fn parse_line(line: &str) -> Option<Line<'_>> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let mut fields = line
        .split(|c: char| c == ',' || c == ';' || c.is_ascii_whitespace())
        .filter(|s| !s.is_empty());
    let mut f = [fields.next()?, "", ""];
    let mut n = 1;
    for slot in f.iter_mut().skip(1) {
        match fields.next() {
            Some(v) => {
                *slot = v;
                n += 1;
            }
            None => break,
        }
    }
    let ts = |s: &str| -> Option<i64> {
        let ts: f64 = s.parse().ok()?;
        Some(if ts < 1e11 { ts * 1000.0 } else { ts } as i64)
    };
    let (asset, ts_ms, price) = match n {
        1 => (None, now_ms(), f[0].parse().ok()?),
        2 => (None, ts(f[0])?, f[1].parse().ok()?),
        _ => (Some(f[0]), ts(f[1])?, f[2].parse().ok()?),
    };
    (price > 0.0 && f64::is_finite(price)).then_some(Line {
        asset,
        tick: Tick { ts_ms, price },
    })
}

/// Reads tick lines on its own thread until EOF.
pub fn spawn_lines<R: BufRead + Send + 'static>(
    mut reader: R,
    default_asset: AssetId,
    registry: Arc<Registry>,
    tx: SyncSender<Input>,
) {
    thread::spawn(move || {
        let mut cache = AssetCache::new(registry);
        let mut line = String::with_capacity(64);
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if let Some(l) = parse_line(&line) {
                        let id = l.asset.map_or(default_asset, |a| cache.id(a));
                        if tx.send(Input::Tick(id, l.tick)).is_err() {
                            return;
                        }
                    }
                }
            }
        }
        let _ = tx.send(Input::SourceDone);
    });
}

/// Reads a whole tick file into per-asset, time-sorted backfills. Lines without
/// an asset go to `default_asset`, or are skipped without one.
pub fn read_backfill<R: BufRead>(reader: R, default_asset: Option<AssetId>, registry: &Registry) -> Vec<(AssetId, Vec<Tick>)> {
    let mut by_asset: HashMap<AssetId, Vec<Tick>> = HashMap::new();
    for line in reader.lines().map_while(Result::ok) {
        if let Some(l) = parse_line(&line) {
            if let Some(id) = l.asset.map(|a| registry.id(a)).or(default_asset) {
                by_asset.entry(id).or_default().push(l.tick);
            }
        }
    }
    let mut out: Vec<_> = by_asset.into_iter().collect();
    for (_, ticks) in &mut out {
        ticks.sort_by_key(|t| t.ts_ms);
        ticks.dedup_by_key(|t| t.ts_ms);
    }
    out.sort_by_key(|(id, _)| *id);
    out
}

/// Runs a simulated asset on its own thread: the prefill arrives as one
/// backfill, then the rest as live ticks.
pub fn spawn_sim(p: SimParams, asset: AssetId, tx: SyncSender<Input>, finite: bool) {
    thread::spawn(move || {
        let mut feed = SimFeed::new(&p);
        if p.prefill > 0 {
            let n = feed.instant as usize;
            let backfill: Vec<Tick> = (0..n).map_while(|_| feed.next_tick()).collect();
            if tx.send(Input::Backfill { asset, ticks: backfill, save: false }).is_err() {
                return;
            }
        }
        while let Some(t) = feed.next_tick() {
            if tx.send(Input::Tick(asset, t)).is_err() {
                return;
            }
        }
        if finite {
            let _ = tx.send(Input::SourceDone);
        }
    });
}

#[derive(Clone, Copy, Debug)]
pub struct SimParams {
    pub seed: u64,
    pub price: f64,
    /// Minutes of history emitted instantly before the run.
    pub prefill: u64,
    pub minutes: u64,
    pub step_ms: i64,
    pub vol: f64,
    /// Pull of each minute's return against the previous one; 0 is a pure random
    /// walk, 0.3 makes ~60% of candles reverse, negative values add momentum.
    pub revert: f64,
    pub realtime: bool,
}

/// Geometric random walk, optionally with candle-to-candle mean reversion. With
/// `revert` 0 there is no edge to find, which makes it a useful sanity check.
pub struct SimFeed {
    rng: u64,
    price: f64,
    ts_ms: i64,
    step_ms: i64,
    sigma: f64,
    remaining: u64,
    instant: u64,
    realtime: bool,
    revert: f64,
    ticks_per_min: f64,
    minute: i64,
    minute_open: f64,
    drift: f64,
}

impl SimFeed {
    pub fn new(p: &SimParams) -> Self {
        let step_ms = p.step_ms.max(1);
        let ticks_per_min = 60_000 / step_ms as u64;
        // Realtime runs start now; instant runs end now, minute-aligned so a seed
        // always produces the same candles. Either way no tick is in the future.
        let ts_ms = if p.realtime {
            now_ms() - p.prefill as i64 * 60_000
        } else {
            now_ms().div_euclid(60_000) * 60_000 - (p.prefill + p.minutes) as i64 * 60_000
        };
        Self {
            rng: splitmix64(p.seed) | 1,
            price: p.price,
            ts_ms,
            step_ms,
            sigma: p.vol * (step_ms as f64 / 60_000.0).sqrt(),
            remaining: (p.prefill + p.minutes) * ticks_per_min,
            instant: p.prefill * ticks_per_min,
            realtime: p.realtime,
            revert: p.revert,
            ticks_per_min: ticks_per_min as f64,
            minute: ts_ms.div_euclid(60_000),
            minute_open: p.price,
            drift: 0.0,
        }
    }

    #[inline]
    fn uniform(&mut self) -> f64 {
        // xorshift64*
        self.rng ^= self.rng >> 12;
        self.rng ^= self.rng << 25;
        self.rng ^= self.rng >> 27;
        let x = self.rng.wrapping_mul(0x2545_F491_4F6C_DD1D);
        ((x >> 11) as f64 + 0.5) / (1u64 << 53) as f64
    }

    #[inline]
    fn normal(&mut self) -> f64 {
        let (u1, u2) = (self.uniform(), self.uniform());
        (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
    }
}

fn splitmix64(seed: u64) -> u64 {
    let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

impl Feed for SimFeed {
    #[inline]
    fn next_tick(&mut self) -> Option<Tick> {
        if self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;
        if self.instant > 0 {
            self.instant -= 1;
        } else if self.realtime {
            thread::sleep(Duration::from_millis(self.step_ms as u64));
        }
        self.ts_ms += self.step_ms;
        let minute = self.ts_ms.div_euclid(60_000);
        if minute != self.minute {
            let ret = (self.price / self.minute_open).ln();
            self.drift = -self.revert * ret / self.ticks_per_min;
            self.minute = minute;
            self.minute_open = self.price;
        }
        self.price *= (self.drift + self.sigma * self.normal()).exp();
        Some(Tick {
            ts_ms: self.ts_ms,
            price: self.price,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeds_are_distinct() {
        let p = |seed| SimParams { seed, price: 1.0, prefill: 0, minutes: 1, step_ms: 1000, vol: 0.001, revert: 0.0, realtime: false };
        let first = |seed| SimFeed::new(&p(seed)).next_tick().unwrap().price;
        assert_ne!(first(2), first(3));
    }

    #[test]
    fn parses_formats() {
        let tick = |s| parse_line(s).map(|l| l.tick);
        assert_eq!(tick("1700000000000,1.08500"), Some(Tick { ts_ms: 1_700_000_000_000, price: 1.085 }));
        assert_eq!(tick("1700000000.5 1.085\n"), Some(Tick { ts_ms: 1_700_000_000_500, price: 1.085 }));
        assert_eq!(tick("1.085").map(|t| t.price), Some(1.085));
        assert_eq!(tick("ts,price"), None);
        assert_eq!(tick("# comment"), None);
        assert_eq!(tick("1700000000,-1"), None);
        let l = parse_line("EURUSD_otc,1717000000.123,1.0723").unwrap();
        assert_eq!(l.asset, Some("EURUSD_otc"));
        assert_eq!(l.tick, Tick { ts_ms: 1_717_000_000_123, price: 1.0723 });
    }

    #[test]
    fn backfill_groups_sorts_and_dedups() {
        let reg = Registry::default();
        let def = reg.id("DEFAULT");
        let data = "B,3,1.0\nB,1,1.0\n2,5.0\nB,1,1.0\n";
        let out = read_backfill(data.as_bytes(), Some(def), &reg);
        assert_eq!(out.len(), 2);
        let b = reg.find("B").unwrap();
        let ticks = &out.iter().find(|(id, _)| *id == b).unwrap().1;
        assert_eq!(ticks.iter().map(|t| t.ts_ms).collect::<Vec<_>>(), vec![1000, 3000]);
    }
}
