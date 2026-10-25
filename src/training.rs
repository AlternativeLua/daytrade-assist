//! Training that keeps going for every pair and timeframe, and is saved
//! between sessions.
//!
//! Each pair has a learner per timeframe that scores every rule on every
//! candle, whether or not that pair is on screen. What they have learned is
//! written to `training.txt` in the data folder and merged back in on the next
//! start, so it survives restarts and outlives the tick logs, which only keep
//! about a week.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::Path;

use crate::candle::{Aggregator, Candle};
use crate::feed::Tick;
use crate::strategy::{BASES, EXPERTS, Engine, Params, Record, RuleScore, Signal};

/// Timeframes every pair learns, matching the app's timeframe buttons.
pub const TIMEFRAMES_MS: [i64; 6] = [15_000, 30_000, 60_000, 120_000, 300_000, 900_000];

const HEADER: &str = "# daytrade-assist training v1";

/// What one learner knew after the candle at `slot` closed.
#[derive(Clone, Copy, Debug)]
pub struct Saved {
    pub slot: i64,
    pub candles: u64,
    pub records: [Record; EXPERTS],
}

/// One pair's rules on one timeframe.
pub struct Learner {
    pub agg: Aggregator,
    pub engine: Engine,
    /// Saved training to merge in once the ticks reach the candle it was saved at.
    pending: Option<Saved>,
    last_slot: Option<i64>,
}

impl Learner {
    pub fn new(period_ms: i64, params: Params, breakeven: f64, saved: Option<Saved>) -> Self {
        Self {
            agg: Aggregator::new(period_ms),
            engine: Engine::new(params, breakeven),
            pending: saved,
            last_slot: None,
        }
    }

    /// Feeds one tick; returns the candle and signal when a candle closes.
    #[inline]
    pub fn push(&mut self, tick: Tick) -> Option<(Candle, Signal)> {
        let closed = self.agg.push(tick)?;
        // The saved candle isn't in the ticks (trimmed or never logged): merge before going past it.
        if self.pending.is_some_and(|s| closed.slot > s.slot) {
            self.merge();
        }
        let sig = self.engine.on_close(&closed);
        // The saved state already includes this candle, so it replaces what the replay learned.
        if self.pending.is_some_and(|s| closed.slot == s.slot) {
            self.merge();
        }
        self.last_slot = Some(closed.slot);
        Some((closed, sig))
    }

    /// Keeps whichever has learned from more candles: the saved training, or
    /// what replaying the buffered ticks learned (more, when older history arrived).
    fn merge(&mut self) {
        if let Some(s) = self.pending.take() {
            if s.candles >= self.engine.candles() {
                self.engine.restore(s.records, s.candles);
            }
        }
    }

    /// What to save, or to start a rebuilt learner from.
    pub fn snapshot(&self) -> Option<Saved> {
        if self.pending.is_some() {
            return self.pending;
        }
        let slot = self.last_slot?;
        Some(Saved {
            slot,
            candles: self.engine.candles(),
            records: *self.engine.records(),
        })
    }

    pub fn candles(&self) -> u64 {
        self.pending.map_or(self.engine.candles(), |s| s.candles.max(self.engine.candles()))
    }

    /// The best rule so far, counting saved training not merged in yet.
    pub fn strongest(&self) -> Option<RuleScore> {
        let mut e = self.engine;
        if let Some(s) = self.pending.filter(|s| s.candles >= e.candles()) {
            e.restore(s.records, s.candles);
        }
        e.strongest()
    }
}

pub type Store = HashMap<(String, i64), Saved>;

/// Reads saved training. A missing file, or one written for a different set of
/// rules, gives an empty store.
pub fn load(path: &Path) -> Store {
    let mut store = Store::new();
    let Ok(file) = File::open(path) else { return store };
    let mut lines = BufReader::new(file).lines().map_while(Result::ok);
    if lines.next().as_deref() != Some(&header()) {
        eprintln!("{}: saved for a different set of rules, starting training afresh", path.display());
        return store;
    }
    for line in lines {
        if let Some((key, saved)) = parse(&line) {
            store.insert(key, saved);
        }
    }
    store
}

/// Writes the whole store, replacing the file only once it is complete.
pub fn save(path: &Path, store: &Store) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    let mut w = BufWriter::new(File::create(&tmp)?);
    writeln!(w, "{}", header())?;
    let mut keys: Vec<_> = store.keys().collect();
    keys.sort();
    for key in keys {
        let s = &store[key];
        write!(w, "{}\t{}\t{}\t{}", key.0, key.1, s.slot, s.candles)?;
        for r in &s.records {
            write!(w, "\t{} {} {} {}", r.recent_wins(), r.weight(), r.wins, r.total)?;
        }
        writeln!(w)?;
    }
    w.flush()?;
    drop(w);
    fs::rename(&tmp, path)
}

/// Names the rules, so training saved for a different set is never misread.
fn header() -> String {
    format!("{HEADER} {}", BASES.join("|"))
}

/// `asset  period_ms  slot  candles  (w n wins total) × EXPERTS`, tab-separated.
fn parse(line: &str) -> Option<((String, i64), Saved)> {
    let mut f = line.split('\t');
    let asset = f.next().filter(|a| !a.is_empty())?.to_owned();
    let period_ms = f.next()?.parse().ok()?;
    let slot = f.next()?.parse().ok()?;
    let candles = f.next()?.parse().ok()?;
    let mut records = [Record::default(); EXPERTS];
    for r in &mut records {
        let mut v = f.next()?.split(' ');
        let w: f64 = v.next()?.parse().ok()?;
        let n: f64 = v.next()?.parse().ok()?;
        if !(w.is_finite() && n.is_finite() && (0.0..=n).contains(&w)) {
            return None;
        }
        *r = Record::from_parts(w, n, v.next()?.parse().ok()?, v.next()?.parse().ok()?);
    }
    f.next().is_none().then_some(((asset, period_ms), Saved { slot, candles, records }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A random walk with one tick per second; the same second always has the same price.
    fn ticks(from_s: i64, to_s: i64) -> Vec<Tick> {
        let mut price = 1.0;
        let mut rng = 0x2545_F491_4F6C_DD1Du64;
        let mut out = Vec::new();
        for s in 0..to_s {
            rng = rng.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            price *= 1.0 + ((rng >> 11) as f64 / (1u64 << 53) as f64 - 0.5) * 2e-4;
            if s >= from_s {
                out.push(Tick { ts_ms: s * 1000, price });
            }
        }
        out
    }

    fn learn(l: &mut Learner, ticks: &[Tick]) {
        for &t in ticks {
            l.push(t);
        }
    }

    fn fresh(saved: Option<Saved>) -> Learner {
        Learner::new(15_000, Params::default(), 0.54, saved)
    }

    #[test]
    fn resuming_from_saved_training_matches_never_stopping() {
        let all = ticks(0, 6000);
        let mut straight = fresh(None);
        learn(&mut straight, &all);

        // First session saw the first half; the log then lost everything before 2000 s.
        let mut first = fresh(None);
        learn(&mut first, &all[..3000]);
        let saved = first.snapshot().unwrap();
        let mut resumed = fresh(Some(saved));
        learn(&mut resumed, &all[2000..]);

        // Indicators restart from fewer candles, so a vote can rarely differ; the records must not drift.
        assert_eq!(resumed.engine.candles(), straight.engine.candles());
        for (a, b) in resumed.engine.records().iter().zip(straight.engine.records()) {
            assert!(a.total.abs_diff(b.total) <= 2, "{a:?} vs {b:?}");
            assert!((a.weight() - b.weight()).abs() < 2.0, "{a:?} vs {b:?}");
        }
    }

    #[test]
    fn older_history_is_learned_when_it_adds_more_than_was_saved() {
        let mut first = fresh(None);
        learn(&mut first, &ticks(3000, 3600));
        let saved = first.snapshot().unwrap();
        // Chart history now reaches back much further than the saved training.
        let mut rebuilt = fresh(Some(saved));
        learn(&mut rebuilt, &ticks(0, 3600));
        assert!(rebuilt.engine.candles() > saved.candles);
    }

    #[test]
    fn unapplied_training_is_kept() {
        let mut first = fresh(None);
        learn(&mut first, &ticks(0, 3000));
        let saved = first.snapshot().unwrap();
        let idle = fresh(Some(saved));
        assert_eq!(idle.snapshot().unwrap().candles, saved.candles);
        assert_eq!(idle.candles(), saved.candles);
    }

    #[test]
    fn round_trips_through_the_file() {
        let mut l = fresh(None);
        learn(&mut l, &ticks(0, 3000));
        let mut store = Store::new();
        store.insert(("EURUSD_otc".into(), 15_000), l.snapshot().unwrap());
        let dir = std::env::temp_dir().join(format!("da-training-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("training.txt");
        save(&path, &store).unwrap();
        let back = load(&path);
        let _ = fs::remove_dir_all(&dir);
        let (a, b) = (&store[&("EURUSD_otc".into(), 15_000)], &back[&("EURUSD_otc".into(), 15_000)]);
        assert_eq!((a.slot, a.candles), (b.slot, b.candles));
        for (x, y) in a.records.iter().zip(&b.records) {
            assert_eq!((x.recent_wins(), x.weight(), x.wins, x.total), (y.recent_wins(), y.weight(), y.wins, y.total));
        }
    }

    #[test]
    fn ignores_files_for_other_rules_and_bad_lines() {
        assert!(parse("EURUSD\t60000\t1\t2\t0.5 1 1 1").is_none());
        let dir = std::env::temp_dir().join(format!("da-training-old-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("training.txt");
        fs::write(&path, "# daytrade-assist training v1 some|other|rules\n").unwrap();
        assert!(load(&path).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }
}
