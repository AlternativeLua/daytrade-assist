use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Stdout, Write};
use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::time::Instant;

use crate::Config;
use crate::candle::{Aggregator, Candle, MIN_PERIOD_MS};
use crate::feed::{AssetId, Input, Registry, Tick};
use crate::latency::{Histogram, Ns};
use crate::paper::{Outcome, Paper, Settlement};
use crate::strategy::{EXPERTS, Engine, Pick, Side, Signal, expert_name};
use crate::ui::{AssetInfo, Event, Phase, Ui};

/// About a week of ticks at Pocket Option's ~1 tick per second.
pub const MAX_TICKS: usize = 600_000;
const MAX_REPLAY_CANDLES: usize = 3_000;

pub fn is_demo(name: &str) -> bool {
    name.starts_with("DEMO:")
}

struct Series {
    name: String,
    ticks: VecDeque<Tick>,
    log: Option<BufWriter<File>>,
}

/// The engine thread: buffers every asset's ticks, and runs the candle →
/// rules → paper-trade pipeline for the one selected asset and timeframe.
pub struct Session {
    cfg: &'static Config,
    p: Printer,
    out: BufWriter<Stdout>,
    registry: Arc<Registry>,
    series: Vec<Series>,
    active: Option<AssetId>,
    switched_to_real: bool,
    agg: Aggregator,
    engine: Engine,
    paper: Paper,
    close_lat: Histogram,
    tick_lat: Histogram,
    previewed_slot: i64,
    learning: bool,
    signals: u32,
    open_pick: Option<Pick>,
    pub ui: Option<Ui>,
    last_forming_push: Option<Instant>,
    last_housekeeping: Instant,
    record: Option<BufWriter<File>>,
    quiet: bool,
    live: bool,
    replay_candles: Vec<Candle>,
}

impl Session {
    pub fn new(cfg: &'static Config, registry: Arc<Registry>) -> io::Result<Self> {
        let paper = Paper::new(cfg.balance, cfg.stake, cfg.payout);
        let record = match &cfg.record {
            Some(path) => Some(BufWriter::new(
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                    .map_err(|e| io::Error::new(e.kind(), format!("{path}: {e}")))?,
            )),
            None => None,
        };
        Ok(Self {
            cfg,
            p: Printer {
                cfg,
                period_ms: cfg.period_ms,
            },
            out: BufWriter::new(io::stdout()),
            registry,
            series: Vec::new(),
            active: None,
            switched_to_real: false,
            agg: Aggregator::new(cfg.period_ms),
            engine: Engine::new(cfg.params, paper.breakeven()),
            paper,
            close_lat: Histogram::default(),
            tick_lat: Histogram::default(),
            previewed_slot: i64::MIN,
            learning: true,
            signals: 0,
            open_pick: None,
            ui: None,
            last_forming_push: None,
            last_housekeeping: Instant::now(),
            record,
            quiet: cfg.quiet,
            live: true,
            replay_candles: Vec::new(),
        })
    }

    /// Consumes inputs until every finite source is done (forever if there are none).
    pub fn run(&mut self, rx: Receiver<Input>, finite_sources: usize) -> io::Result<()> {
        if !self.quiet {
            self.p.header(&mut self.out)?;
        }
        let mut done = 0;
        while let Ok(input) = rx.recv() {
            match input {
                Input::Tick(id, t) => self.on_input_tick(id, t)?,
                Input::Backfill { asset, ticks, save } => self.backfill(asset, ticks, save)?,
                Input::Select { asset, period_ms } => self.select(asset, period_ms)?,
                Input::SourceDone => {
                    done += 1;
                    if finite_sources > 0 && done >= finite_sources {
                        break;
                    }
                }
            }
        }
        self.summary("summary")?;
        self.flush_logs()?;
        self.out.flush()?;
        self.phase(Phase::Ended);
        Ok(())
    }

    fn ensure_series(&mut self, id: AssetId) {
        if self.series.len() > id as usize {
            return;
        }
        while self.series.len() <= id as usize {
            let name = self.registry.name(self.series.len() as AssetId);
            let log = self.cfg.data_dir.as_ref().filter(|_| !is_demo(&name)).and_then(|dir| {
                let path = dir.join("ticks").join(log_file_name(&name));
                OpenOptions::new().create(true).append(true).open(path).ok().map(BufWriter::new)
            });
            self.series.push(Series {
                name,
                ticks: VecDeque::new(),
                log,
            });
        }
        self.push_assets();
    }

    #[inline]
    fn on_input_tick(&mut self, id: AssetId, t: Tick) -> io::Result<()> {
        self.ensure_series(id);
        if self.series[id as usize].ticks.back().is_some_and(|b| t.ts_ms < b.ts_ms) {
            return Ok(());
        }
        self.maybe_auto_select(id)?;
        if self.active == Some(id) {
            self.on_tick(t)?;
        }

        // Bookkeeping after the decision, so none of it delays a signal.
        let s = &mut self.series[id as usize];
        if s.ticks.len() == MAX_TICKS {
            s.ticks.pop_front();
        }
        s.ticks.push_back(t);
        if let Some(log) = &mut s.log {
            writeln!(log, "{},{},{}", s.name, t.ts_ms, t.price)?;
        }
        if let Some(r) = &mut self.record {
            writeln!(r, "{},{},{}", s.name, t.ts_ms, t.price)?;
        }
        if s.ticks.len() == 1 || self.last_housekeeping.elapsed().as_millis() >= 1000 {
            self.last_housekeeping = Instant::now();
            self.flush_logs()?;
            self.push_assets();
        }
        Ok(())
    }

    /// Picks the first asset that ticks (or `--asset`), and moves off a demo
    /// asset the first time a real one appears.
    fn maybe_auto_select(&mut self, id: AssetId) -> io::Result<()> {
        let name = &self.series[id as usize].name;
        let demo = is_demo(name);
        let wanted = self.cfg.asset.as_deref().is_none_or(|want| want == name);
        match self.active {
            None if wanted => {
                self.switched_to_real |= !demo;
                self.select(id, self.agg.period_ms())
            }
            Some(cur) if cur != id && !self.switched_to_real && is_demo(&self.series[cur as usize].name) && !demo => {
                self.switched_to_real = true;
                self.select(id, self.agg.period_ms())
            }
            _ => Ok(()),
        }
    }

    /// Prepends ticks older than anything buffered; retrains if the asset is active.
    fn backfill(&mut self, id: AssetId, mut ticks: Vec<Tick>, save: bool) -> io::Result<()> {
        self.ensure_series(id);
        let s = &mut self.series[id as usize];
        let first = s.ticks.front().map(|t| t.ts_ms);
        ticks.retain(|t| first.is_none_or(|f| t.ts_ms < f) && t.price > 0.0 && t.price.is_finite());
        ticks.sort_by_key(|t| t.ts_ms);
        ticks.dedup_by_key(|t| t.ts_ms);
        if ticks.is_empty() {
            return Ok(());
        }
        if let Some(log) = s.log.as_mut().filter(|_| save) {
            for t in &ticks {
                writeln!(log, "{},{},{}", s.name, t.ts_ms, t.price)?;
            }
        }
        for &t in ticks.iter().rev() {
            s.ticks.push_front(t);
        }
        while s.ticks.len() > MAX_TICKS {
            s.ticks.pop_front();
        }
        self.push_assets();
        if self.active == Some(id) {
            self.select(id, self.agg.period_ms())?;
        }
        Ok(())
    }

    /// Switches asset and/or timeframe: a fresh engine learns from the asset's
    /// buffered ticks (without trading), then a fresh paper account goes live.
    fn select(&mut self, id: AssetId, period_ms: i64) -> io::Result<()> {
        self.ensure_series(id);
        let period_ms = period_ms.max(MIN_PERIOD_MS);
        self.active = Some(id);
        self.agg = Aggregator::new(period_ms);
        self.p.period_ms = period_ms;
        self.engine = Engine::new(self.cfg.params, self.paper.breakeven());
        self.reset_account();
        self.previewed_slot = i64::MIN;
        self.learning = true;
        self.last_forming_push = None;
        if let Some(ui) = &self.ui {
            ui.send(Event::Active {
                asset: self.series[id as usize].name.clone(),
                period_ms,
            });
        }
        self.phase(Phase::History);

        let ticks = std::mem::take(&mut self.series[id as usize].ticks);
        let (quiet, live) = (self.quiet, self.live);
        self.quiet = true;
        self.live = false;
        self.replay_candles.clear();
        let mut result = Ok(());
        for &t in &ticks {
            result = self.on_tick(t).map(|_| ());
            if result.is_err() {
                break;
            }
        }
        self.series[id as usize].ticks = ticks;
        self.quiet = quiet;
        self.live = live;
        result?;

        if !self.replay_candles.is_empty() && !self.quiet {
            let title = format!("{} {} — learned from history", self.series[id as usize].name, tf_label(period_ms));
            self.summary(&title)?;
            self.p.header(&mut self.out)?;
        }
        if let Some(ui) = &self.ui {
            let start = self.replay_candles.len().saturating_sub(MAX_REPLAY_CANDLES);
            ui.send(Event::Replayed {
                total_candles: self.replay_candles.len(),
                candles: self.replay_candles[start..].to_vec(),
                records: *self.engine.records(),
                wins: self.paper.wins,
                losses: self.paper.losses,
                ties: self.paper.ties,
                pnl: self.paper.balance - self.paper.start_balance,
            });
        }
        self.replay_candles = Vec::new();
        self.reset_account();
        self.phase(Phase::Live);
        self.push_assets();
        Ok(())
    }

    fn phase(&self, phase: Phase) {
        if let Some(ui) = &self.ui {
            ui.send(Event::Phase(phase));
        }
    }

    fn push_assets(&self) {
        if let Some(ui) = &self.ui {
            let assets = self
                .series
                .iter()
                .map(|s| AssetInfo {
                    name: s.name.clone(),
                    ticks: s.ticks.len(),
                    first_ts: s.ticks.front().map_or(0, |t| t.ts_ms),
                    last_ts: s.ticks.back().map_or(0, |t| t.ts_ms),
                    last_price: s.ticks.back().map_or(0.0, |t| t.price),
                })
                .collect();
            ui.send(Event::Assets(assets));
        }
    }

    fn flush_logs(&mut self) -> io::Result<()> {
        for s in &mut self.series {
            if let Some(log) = &mut s.log {
                log.flush()?;
            }
        }
        if let Some(r) = &mut self.record {
            r.flush()?;
        }
        Ok(())
    }

    /// Keeps what the engine has learned, starts a fresh account and stats.
    fn reset_account(&mut self) {
        self.paper = Paper::new(self.cfg.balance, self.cfg.stake, self.cfg.payout);
        self.close_lat = Histogram::default();
        self.tick_lat = Histogram::default();
        self.signals = 0;
        self.open_pick = None;
    }

    /// Tick → candle → rules → decision, with no allocation, locking or I/O
    /// before the decision is made.
    #[inline]
    fn on_tick(&mut self, tick: Tick) -> io::Result<()> {
        let t0 = Instant::now();

        if let Some(closed) = self.agg.push(tick) {
            let settled = self.paper.settle(&closed);
            let sig = self.engine.on_close(&closed);
            let new_slot = self.agg.slot(tick.ts_ms);
            let opened = sig.side.is_some_and(|side| self.paper.open(side, tick.price, new_slot));
            let dt = t0.elapsed();
            self.close_lat.record(dt);
            self.tick_lat.record(dt);
            self.learning = sig.learning.is_some();
            self.signals += sig.side.is_some() as u32;

            if !self.live {
                if self.ui.is_some() {
                    self.replay_candles.push(closed);
                }
                return Ok(());
            }
            if let Some(ui) = &self.ui {
                if let Some(s) = settled {
                    ui.send(Event::Settled {
                        s,
                        rule: self.open_pick.take(),
                        balance: self.paper.balance,
                    });
                }
                if opened {
                    self.open_pick = sig.best;
                }
                ui.send(Event::Closed {
                    candle: closed,
                    sig,
                    entry: opened.then_some((tick.price, new_slot)),
                    ns: dt.as_nanos() as u64,
                    records: *self.engine.records(),
                });
            }
            if !self.quiet {
                if let Some(s) = settled {
                    self.p.settlement(&mut self.out, &s, &self.paper)?;
                }
                self.p.candle(&mut self.out, &closed, &sig, opened.then_some(tick.price), dt.as_nanos() as u64)?;
                self.out.flush()?;
            }
            return Ok(());
        }

        let show = self.live && (!self.quiet || self.ui.is_some());
        if self.cfg.heads_up && !self.learning && show && self.agg.is_late(tick.ts_ms) {
            if let Some(forming) = self.agg.current().copied() {
                if forming.slot != self.previewed_slot {
                    self.previewed_slot = forming.slot;
                    let sig = self.engine.peek(&forming);
                    if sig.side.is_some() {
                        if let Some(ui) = &self.ui {
                            ui.send(Event::Preview { slot: forming.slot, sig });
                        }
                        if !self.quiet {
                            self.p.preview(&mut self.out, &forming, &sig)?;
                            self.out.flush()?;
                        }
                    }
                }
            }
        }
        self.tick_lat.record(t0.elapsed());
        if self.live {
            if let (Some(ui), Some(forming)) = (&self.ui, self.agg.current()) {
                if self.last_forming_push.is_none_or(|t| t0.duration_since(t).as_millis() >= 100) {
                    self.last_forming_push = Some(t0);
                    ui.send(Event::Forming {
                        candle: *forming,
                        ts_ms: tick.ts_ms,
                    });
                }
            }
        }
        Ok(())
    }

    pub fn summary(&mut self, title: &str) -> io::Result<()> {
        self.p.summary(&mut self.out, title, &self.paper, self.signals, &self.close_lat, &self.tick_lat)?;
        self.p.leaderboard(&mut self.out, &self.engine)
    }
}

/// Asset names become file names; anything unusual is replaced.
pub fn log_file_name(asset: &str) -> String {
    let safe: String = asset
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect();
    format!("{safe}.csv")
}

pub fn tf_label(period_ms: i64) -> String {
    let s = period_ms / 1000;
    if s % 3600 == 0 {
        format!("{}h", s / 3600)
    } else if s % 60 == 0 {
        format!("{}m", s / 60)
    } else {
        format!("{s}s")
    }
}

struct Printer {
    cfg: &'static Config,
    period_ms: i64,
}

impl Printer {
    fn paint(&self, code: &str, text: &str) -> String {
        if self.cfg.color { format!("\x1b[{code}m{text}\x1b[0m") } else { text.to_owned() }
    }

    fn time(&self, slot: i64) -> String {
        let secs = (slot * self.period_ms).div_euclid(1000).rem_euclid(86_400);
        if self.period_ms < 60_000 {
            format!("{:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
        } else {
            format!("{:02}:{:02}", secs / 3600, secs / 60 % 60)
        }
    }

    fn side_label(&self, side: Side) -> String {
        match side {
            Side::Call => self.paint("1;32", "CALL ▲"),
            Side::Put => self.paint("1;31", "PUT  ▼"),
        }
    }

    fn header(&self, out: &mut impl Write) -> io::Result<()> {
        writeln!(
            out,
            "{}",
            self.paint(
                "2",
                &format!(
                    "PAPER TRADING ONLY — signals are a statistical guess, not advice. {} candles, times are UTC.",
                    tf_label(self.period_ms)
                )
            )
        )
    }

    fn rule(&self, sig: &Signal) -> String {
        sig.best.map_or(String::new(), |b| {
            let (verb, base) = expert_name(b.expert);
            format!("{verb} {base} {:.1}% (≥{:.1}%, n {:.0})", b.rate * 100.0, b.lower * 100.0, b.n)
        })
    }

    fn candle(&self, out: &mut impl Write, c: &Candle, sig: &Signal, entry: Option<f64>, ns: u64) -> io::Result<()> {
        let d = self.cfg.digits;
        write!(out, "{}  O {:.d$} H {:.d$} L {:.d$} C {:.d$}  ", self.time(c.slot), c.open, c.high, c.low, c.close)?;
        if let Some(n) = sig.learning {
            return writeln!(
                out,
                "learning the feed ({:.0}/{:.0} votes)  {}",
                n,
                self.cfg.params.min_samples,
                Ns(ns)
            );
        }
        write!(out, "rsi {:5.1}  z {:+5.2}  → ", sig.rsi, sig.z)?;
        match (sig.side, entry) {
            (Some(side), Some(entry)) => write!(
                out,
                "{}  {}  entry {entry:.d$} exp {}",
                self.side_label(side),
                self.rule(sig),
                self.time(c.slot + 2),
            )?,
            (Some(side), None) => write!(out, "{} (skipped: insufficient balance)", self.side_label(side))?,
            (None, _) => write!(out, "{}", self.paint("2", &format!("hold   best: {}", self.rule(sig))))?,
        }
        writeln!(out, "  {}", Ns(ns))
    }

    fn preview(&self, out: &mut impl Write, forming: &Candle, sig: &Signal) -> io::Result<()> {
        let side = sig.side.map_or(String::new(), |s| self.side_label(s));
        writeln!(
            out,
            "{}  {} {side} likely at {} if price holds near {:.d$}  [{}]",
            self.time(forming.slot),
            self.paint("33", "heads-up:"),
            self.time(forming.slot + 1),
            forming.close,
            self.rule(sig),
            d = self.cfg.digits,
        )
    }

    fn settlement(&self, out: &mut impl Write, s: &Settlement, paper: &Paper) -> io::Result<()> {
        let d = self.cfg.digits;
        let result = match s.outcome {
            Outcome::Win => self.paint("32", &format!("WIN  {:+.2}", s.pnl)),
            Outcome::Loss => self.paint("31", &format!("LOSS {:+.2}", s.pnl)),
            Outcome::Tie => self.paint("33", "TIE  +0.00"),
        };
        let side = if s.trade.side == Side::Call { "CALL" } else { "PUT " };
        writeln!(
            out,
            "       └ {side} {:.d$} → {:.d$}  {result}   balance {:.2}   {}W/{}L/{}T {:.1}%",
            s.trade.entry,
            s.exit,
            paper.balance,
            paper.wins,
            paper.losses,
            paper.ties,
            paper.win_rate() * 100.0,
        )
    }

    fn summary(
        &self,
        out: &mut impl Write,
        title: &str,
        paper: &Paper,
        signals: u32,
        close: &Histogram,
        tick: &Histogram,
    ) -> io::Result<()> {
        let pnl = paper.balance - paper.start_balance;
        let candles = close.count().max(1);
        writeln!(out, "\n── {title} {}", "─".repeat(48usize.saturating_sub(title.chars().count())))?;
        writeln!(
            out,
            "trades    {} on {} candles ({:.0}% of candles)   {}W / {}L / {}T{}",
            paper.wins + paper.losses + paper.ties,
            close.count(),
            signals as f64 * 100.0 / candles as f64,
            paper.wins,
            paper.losses,
            paper.ties,
            if paper.open.is_some() { "   (1 still open)" } else { "" },
        )?;
        writeln!(
            out,
            "win rate  {:.1}%   breakeven at {:.0}% payout: {:.1}%",
            paper.win_rate() * 100.0,
            paper.payout * 100.0,
            paper.breakeven() * 100.0,
        )?;
        writeln!(
            out,
            "balance   {:.2}   P&L {pnl:+.2}   max drawdown {:.2}",
            paper.balance, paper.max_drawdown
        )?;
        if close.count() > 0 {
            writeln!(
                out,
                "latency   candle close → decision: p50 ≤{}  p99 ≤{}  max {}",
                Ns(close.quantile_ns(0.5)),
                Ns(close.quantile_ns(0.99)),
                Ns(close.max_ns()),
            )?;
            writeln!(
                out,
                "          any tick:                p50 ≤{}  p99 ≤{}  max {}   ({} ticks)",
                Ns(tick.quantile_ns(0.5)),
                Ns(tick.quantile_ns(0.99)),
                Ns(tick.max_ns()),
                tick.count(),
            )?;
        }
        if paper.decided() < 100 {
            writeln!(out, "{}", self.paint("2", "note: under 100 trades, the win rate is mostly noise"))?;
        }
        Ok(())
    }

    fn leaderboard(&self, out: &mut impl Write, engine: &Engine) -> io::Result<()> {
        let mut rows: Vec<(usize, u32, u32)> = engine
            .records()
            .iter()
            .enumerate()
            .filter(|(_, r)| r.total >= 30)
            .map(|(i, r)| (i, r.wins, r.total))
            .collect();
        if rows.is_empty() {
            return Ok(());
        }
        rows.sort_by(|a, b| (b.1 as f64 / b.2 as f64).total_cmp(&(a.1 as f64 / a.2 as f64)));
        writeln!(out, "best rules on this feed (all votes, traded or not; best of {EXPERTS} looks good partly by luck):")?;
        for &(i, wins, total) in rows.iter().take(5) {
            let (verb, base) = expert_name(i);
            writeln!(
                out,
                "  {:<22} {:5.1}%  {total} votes",
                format!("{verb} {base}"),
                wins as f64 * 100.0 / total as f64
            )?;
        }
        Ok(())
    }
}
