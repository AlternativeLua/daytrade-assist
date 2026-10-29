use std::collections::{HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Stdout, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::time::Instant;

use crate::Config;
use crate::candle::{Candle, MIN_PERIOD_MS};
use crate::feed::{AssetId, Input, Registry, Tick, TradeMode};
use crate::latency::{Histogram, Ns};
use crate::paper::{Martingale, Outcome, Paper, Settlement, Trade};
use crate::power::{Awake, KeepAwake};
use crate::strategy::{EXPERTS, Engine, Frequency, Hold, Params, Pick, Side, Signal, expert_name};
use crate::feed::now_ms;
use crate::training::{self, Learner, Store, TIMEFRAMES_MS};
use crate::ui::{AssetInfo, Event, OrderSent, Phase, Status, TrainingRow, Ui};

/// About a week of ticks at Pocket Option's ~1 tick per second.
pub const MAX_TICKS: usize = 600_000;
const MAX_REPLAY_CANDLES: usize = 3_000;
const TRAINING_PUSH_SECS: u64 = 5;
const TRAINING_SAVE_SECS: u64 = 60;
/// The selected pair counts as stopped after this long without prices.
const STOPPED_MS: i64 = 30_000;
/// Pairs that sent prices this recently count as streaming.
const STREAMING_MS: i64 = 10_000;

pub fn is_demo(name: &str) -> bool {
    name.starts_with("DEMO:")
}

struct Series {
    name: String,
    ticks: VecDeque<Tick>,
    /// Opened on first use, so assets that never tick get no file.
    log: Option<BufWriter<File>>,
    log_checked: bool,
    /// When the last tick arrived, by this computer's clock. Feed timestamps
    /// can't be used for this: Pocket Option's run hours off the local clock.
    seen_ms: i64,
    /// One per timeframe in `Session::periods`, learning whether or not this asset is selected.
    learners: Vec<Learner>,
}

impl Series {
    fn append_log(&mut self, cfg: &Config, ticks: &[Tick]) -> io::Result<()> {
        if !self.log_checked {
            self.log_checked = true;
            self.log = cfg.data_dir.as_ref().filter(|_| !is_demo(&self.name)).and_then(|dir| {
                let path = dir.join("ticks").join(log_file_name(&self.name));
                OpenOptions::new().create(true).append(true).open(path).ok().map(BufWriter::new)
            });
        }
        if let Some(log) = &mut self.log {
            for t in ticks {
                writeln!(log, "{},{},{}", self.name, t.ts_ms, t.price)?;
            }
        }
        Ok(())
    }
}

/// The engine thread: buffers every asset's ticks, trains every asset on every
/// timeframe, and runs the candle → rules → paper-trade pipeline for the one
/// selected asset and timeframe.
pub struct Session {
    cfg: &'static Config,
    p: Printer,
    out: BufWriter<Stdout>,
    registry: Arc<Registry>,
    series: Vec<Series>,
    active: Option<AssetId>,
    switched_to_real: bool,
    /// The selected asset and timeframe. It duplicates that asset's background
    /// learner, plus the paper account and UI updates.
    learner: Learner,
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
    /// Latest payout and open flag per asset, as reported by the feed.
    payouts: HashMap<AssetId, (f64, bool)>,
    can_trade: bool,
    martingale: Martingale,
    /// Strategy settings, with the trade frequency the UI last chose.
    params: Params,
    /// Where the chosen trade frequency is kept between sessions.
    frequency_path: Option<PathBuf>,
    stake: f64,
    stake_pct: f64,
    /// Whether live paper trades are also placed on Pocket Option, and on which account.
    mode: TradeMode,
    /// No new trades of any kind while paused; learning and signals continue.
    paused: bool,
    loss_limit: f64,
    /// Profit and loss of trades mirrored to the real account since it was turned on.
    real_pnl: f64,
    open_mirror: TradeMode,
    stop_reason: Option<String>,
    order_seq: u64,
    /// Timeframes every asset learns.
    periods: Vec<i64>,
    /// Training saved in earlier sessions, updated each time it is saved again.
    store: Store,
    training_path: Option<PathBuf>,
    saved_at: Option<i64>,
    last_save: Instant,
    last_training_push: Instant,
    awake: Awake,
    awake_guard: Option<KeepAwake>,
    /// Trade any streaming pair on the selected timeframe, not just the selected pair.
    auto: bool,
    /// Asset and timeframe of the open paper trade.
    trade_on: Option<(AssetId, i64)>,
    /// Where the last loss happened. In auto mode the multiplied trade that
    /// follows stays on that pair rather than going to another one.
    run_on: Option<(AssetId, i64)>,
    /// Pairs auto mode may trade; empty means every pair.
    auto_list: Vec<String>,
    /// Starting balance for a fresh paper account.
    balance: f64,
    /// Follow the Pocket Option balance of the account trades go to.
    sync: bool,
    /// Latest Pocket Option balances: real, demo. Kept on disk, because
    /// Pocket Option only reports them when its tab loads or after a trade.
    po_balance: [Option<f64>; 2],
    balance_path: Option<PathBuf>,
}

impl Session {
    pub fn new(cfg: &'static Config, registry: Arc<Registry>) -> io::Result<Self> {
        let martingale = Martingale {
            steps: cfg.martingale_steps,
            mult: cfg.martingale_mult,
        };
        let mut paper = Paper::new(cfg.balance, cfg.stake, cfg.payout, martingale);
        paper.stake_pct = cfg.stake_pct;
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
        let training_path = cfg.data_dir.as_ref().map(|d| d.join("training.txt"));
        let balance_path = cfg.data_dir.as_ref().map(|d| d.join("pocket-option-balance.txt"));
        let frequency_path = cfg.data_dir.as_ref().map(|d| d.join("trade-frequency.txt"));
        let saved_frequency = frequency_path
            .as_deref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| Frequency::parse(&s));
        let params = saved_frequency.map_or(cfg.params, |f| f.apply(cfg.params));
        let po_balance = balance_path.as_deref().map_or([None, None], load_balances);
        let store = training_path.as_deref().map(training::load).unwrap_or_default();
        let mut periods = TIMEFRAMES_MS.to_vec();
        if !periods.contains(&cfg.period_ms.max(MIN_PERIOD_MS)) {
            periods.push(cfg.period_ms.max(MIN_PERIOD_MS));
        }
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
            learner: Learner::new(cfg.period_ms, params, paper.breakeven(), None),
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
            payouts: HashMap::new(),
            can_trade: true,
            martingale,
            params,
            frequency_path,
            stake: cfg.stake,
            stake_pct: cfg.stake_pct,
            mode: TradeMode::Off,
            paused: false,
            loss_limit: cfg.loss_limit,
            real_pnl: 0.0,
            open_mirror: TradeMode::Off,
            stop_reason: None,
            order_seq: 0,
            periods,
            store,
            training_path,
            saved_at: None,
            last_save: Instant::now(),
            last_training_push: Instant::now(),
            awake: Awake::Off,
            awake_guard: None,
            auto: cfg.auto,
            trade_on: None,
            run_on: None,
            auto_list: Vec::new(),
            balance: cfg.balance,
            sync: false,
            po_balance,
            balance_path,
        })
    }

    /// Consumes inputs until every finite source is done (forever if there are none).
    pub fn run(&mut self, rx: Receiver<Input>, finite_sources: usize) -> io::Result<()> {
        if !self.quiet {
            self.p.header(&mut self.out)?;
        }
        // On this thread: on Windows only the thread that asked can let the computer sleep again.
        self.set_awake(self.cfg.awake);
        let mut done = 0;
        while let Ok(input) = rx.recv() {
            match input {
                Input::Tick(id, t) => self.on_input_tick(id, t)?,
                Input::Backfill { asset, ticks, save } => self.backfill(asset, ticks, save)?,
                Input::Select { asset, period_ms } => self.select(asset, period_ms)?,
                Input::Payouts(list) => {
                    for (id, payout, open) in list {
                        self.payouts.insert(id, (payout, open));
                    }
                    self.apply_payout();
                }
                Input::Settings(change) => {
                    if let Some(steps) = change.martingale_steps {
                        self.martingale.steps = steps;
                    }
                    if let Some(mult) = change.martingale_mult {
                        self.martingale.mult = mult;
                    }
                    self.paper.set_martingale(self.martingale);
                    if change.z.is_some() || change.frequency.is_some() {
                        if let Some(f) = change.frequency {
                            self.params = f.apply(self.params);
                            if let Some(path) = &self.frequency_path {
                                let _ = std::fs::write(path, f.name());
                            }
                        }
                        if let Some(z) = change.z {
                            self.params.z = z;
                        }
                        let p = self.params;
                        self.learner.engine.set_frequency(&p);
                        for l in self.series.iter_mut().flat_map(|s| &mut s.learners) {
                            l.engine.set_frequency(&p);
                        }
                        self.push_training();
                    }
                    if let Some(stake) = change.stake {
                        self.stake = stake;
                        self.paper.base_stake = stake;
                    }
                    if let Some(pct) = change.stake_pct {
                        self.stake_pct = pct;
                        self.paper.stake_pct = pct;
                    }
                    if let Some(limit) = change.loss_limit {
                        self.loss_limit = limit;
                    }
                    if let Some(mode) = change.mode {
                        if mode == TradeMode::Real && self.mode != TradeMode::Real {
                            self.real_pnl = 0.0;
                        }
                        let account_changed = (mode == TradeMode::Real) != (self.mode == TradeMode::Real);
                        self.mode = mode;
                        self.stop_reason = None;
                        if account_changed {
                            self.apply_sync(true);
                        }
                    }
                    if let Some(balance) = change.balance {
                        self.balance = balance;
                        self.sync = false;
                        self.paper.set_balance(balance, true);
                    }
                    if let Some(sync) = change.sync {
                        self.sync = sync;
                        self.apply_sync(true);
                    }
                    if let Some(paused) = change.paused {
                        self.paused = paused;
                    }
                    if let Some(awake) = change.awake {
                        self.set_awake(awake);
                    }
                    if let Some(auto) = change.auto {
                        self.auto = auto;
                    }
                    if let Some(list) = change.auto_list {
                        self.auto_list = list;
                    }
                    self.push_status();
                }
                Input::Balance { demo, balance } => {
                    self.po_balance[demo as usize] = Some(balance);
                    if let Some(path) = &self.balance_path {
                        let line = |b: Option<f64>| b.map_or(String::new(), |b| b.to_string());
                        let _ = std::fs::write(path, format!("{}\n{}\n", line(self.po_balance[0]), line(self.po_balance[1])));
                    }
                    self.apply_sync(false);
                    self.push_status();
                }
                Input::Flush(done) => {
                    self.save_training();
                    self.flush_logs()?;
                    let _ = done.send(());
                }
                Input::SourceDone => {
                    done += 1;
                    if finite_sources > 0 && done >= finite_sources {
                        break;
                    }
                }
            }
        }
        self.summary("summary")?;
        self.save_training();
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
            let learners = self.periods.iter().map(|&p| self.new_learner(&name, p)).collect();
            self.series.push(Series {
                name,
                ticks: VecDeque::new(),
                log: None,
                log_checked: false,
                seen_ms: 0,
                learners,
            });
        }
        self.push_assets();
    }

    fn params(&self) -> Params {
        self.params
    }

    /// A learner starting from whatever was saved for this asset and timeframe.
    fn new_learner(&self, name: &str, period_ms: i64) -> Learner {
        let saved = self.store.get(&(name.to_owned(), period_ms)).copied();
        Learner::new(period_ms, self.params(), self.breakeven_for(name), saved)
    }

    fn breakeven_for(&self, name: &str) -> f64 {
        let payout = self.registry.find(name).and_then(|id| self.payouts.get(&id)).map_or(self.cfg.payout, |p| p.0);
        1.0 / (1.0 + payout)
    }

    /// Adds a timeframe that every asset learns from now on, starting from its buffered ticks.
    fn ensure_period(&mut self, period_ms: i64) {
        if self.periods.contains(&period_ms) {
            return;
        }
        self.periods.push(period_ms);
        for i in 0..self.series.len() {
            let mut l = self.new_learner(&self.series[i].name, period_ms);
            for &t in &self.series[i].ticks {
                l.push(t);
            }
            self.series[i].learners.push(l);
        }
    }

    /// Relearns every timeframe of an asset from its buffered ticks, after older
    /// ticks arrived. Each keeps its earlier training where that knows more.
    fn rebuild_learners(&mut self, id: AssetId) {
        let (params, breakeven) = (self.params(), self.breakeven_for(&self.series[id as usize].name));
        let s = &mut self.series[id as usize];
        for l in &mut s.learners {
            let mut fresh = Learner::new(l.agg.period_ms(), params, breakeven, l.snapshot());
            for &t in &s.ticks {
                fresh.push(t);
            }
            *l = fresh;
        }
    }

    /// Writes every asset's training to the data folder. Demo pairs are
    /// simulated afresh each start, so theirs isn't kept.
    fn save_training(&mut self) {
        self.last_save = Instant::now();
        let Some(path) = self.training_path.clone() else { return };
        for s in self.series.iter().filter(|s| !is_demo(&s.name)) {
            for l in &s.learners {
                if let Some(snap) = l.snapshot() {
                    self.store.insert((s.name.clone(), l.agg.period_ms()), snap);
                }
            }
        }
        if self.store.is_empty() {
            return;
        }
        match training::save(&path, &self.store) {
            Ok(()) => self.saved_at = Some(now_ms()),
            Err(e) => eprintln!("couldn't save training to {}: {e}", path.display()),
        }
        self.push_training();
    }

    fn push_training(&mut self) {
        self.last_training_push = Instant::now();
        let Some(ui) = &self.ui else { return };
        let rows = self
            .series
            .iter()
            .filter(|s| !s.ticks.is_empty())
            .flat_map(|s| {
                s.learners.iter().map(|l| {
                    let (best, track, proven) = l.standing();
                    TrainingRow {
                        asset: s.name.clone(),
                        period_ms: l.agg.period_ms(),
                        candles: l.candles(),
                        best,
                        track,
                        proven,
                    }
                })
            })
            .collect();
        ui.send(Event::Training {
            rows,
            saved_at: self.saved_at,
            file: self.training_path.as_ref().map(|p| p.to_string_lossy().into_owned()),
        });
    }

    fn set_awake(&mut self, awake: Awake) {
        if awake == self.awake && (awake == Awake::Off || self.awake_guard.is_some()) {
            return;
        }
        // Release first: on Windows the new state replaces the old one anyway.
        self.awake_guard = None;
        self.awake_guard = KeepAwake::start(awake);
        self.awake = awake;
        if awake != Awake::Off && self.awake_guard.is_none() {
            eprintln!("couldn't keep this computer awake: the system refused");
        }
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
        s.seen_ms = now_ms();
        let mut closes = Vec::new();
        for l in &mut s.learners {
            if let Some((closed, sig)) = l.push(t) {
                closes.push((l.agg.period_ms(), closed, sig));
            }
        }
        s.append_log(self.cfg, &[t])?;
        if let Some(r) = &mut self.record {
            writeln!(r, "{},{},{}", s.name, t.ts_ms, t.price)?;
        }
        let first = s.ticks.len() == 1;
        for (period_ms, closed, sig) in closes {
            self.background_close(id, t, period_ms, &closed, &sig)?;
        }
        if first || self.last_housekeeping.elapsed().as_millis() >= 1000 {
            self.last_housekeeping = Instant::now();
            self.drop_stale_trade();
            self.follow_streaming()?;
            self.flush_logs()?;
            self.push_assets();
            if self.last_save.elapsed().as_secs() >= TRAINING_SAVE_SECS {
                self.save_training();
            } else if self.last_training_push.elapsed().as_secs() >= TRAINING_PUSH_SECS {
                self.push_training();
            }
        }
        Ok(())
    }

    /// A background learner's candle closed. Settles the open trade if it is on
    /// this asset and timeframe and the selected pipeline isn't the one running
    /// it; in auto mode, opens a trade here if nothing is open.
    fn background_close(&mut self, id: AssetId, t: Tick, period_ms: i64, closed: &Candle, sig: &Signal) -> io::Result<()> {
        let selected = self.active == Some(id) && period_ms == self.learner.agg.period_ms();
        if self.trade_on == Some((id, period_ms)) && (self.auto || !selected) {
            let rule = self.open_pick;
            if let Some(s) = self.settle(closed) {
                self.open_pick = None;
                if let Some(ui) = &self.ui {
                    ui.send(Event::Settled {
                        s,
                        rule,
                        balance: self.paper.balance,
                        asset: self.series[id as usize].name.clone(),
                        period_ms,
                    });
                }
                if !self.quiet {
                    self.p.settlement(&mut self.out, &s, &self.paper)?;
                }
                self.push_status();
            }
        }

        if !self.auto || self.paused || self.paper.open.is_some() || period_ms != self.learner.agg.period_ms() {
            return Ok(());
        }
        let Some(side) = sig.side else { return Ok(()) };
        if !self.auto_allows(id) || self.held_run().is_some_and(|run| run != (id, period_ms)) {
            return Ok(());
        }
        // Only pairs of the same kind as the one on screen: live feeds, or simulated demo pairs.
        let demo = is_demo(&self.series[id as usize].name);
        let same_kind = self.active.is_some_and(|a| is_demo(&self.series[a as usize].name) == demo);
        let Some((payout, _)) = self.payout_of(id).filter(|_| same_kind && self.tradeable(id)) else {
            return Ok(());
        };
        self.paper.payout = payout;
        let opened = self.paper.open(side, t.price, t.ts_ms.div_euclid(period_ms));
        self.paper.payout = self.payout_or_default();
        let Some(trade) = self.paper.open.filter(|_| opened) else { return Ok(()) };
        self.trade_on = Some((id, period_ms));
        self.open_pick = sig.best;
        self.place_order(id, trade, period_ms, t.ts_ms);
        if let Some(ui) = &self.ui {
            ui.send(Event::Opened(Some((self.series[id as usize].name.clone(), period_ms, trade, sig.best))));
        }
        self.push_status();
        Ok(())
    }

    /// Settles the open trade on `closed`, and counts it toward the real-money loss limit.
    fn settle(&mut self, closed: &Candle) -> Option<Settlement> {
        let s = self.paper.settle(closed)?;
        self.run_on = if self.paper.step > 0 { self.trade_on } else { None };
        self.trade_on = None;
        if std::mem::replace(&mut self.open_mirror, TradeMode::Off) == TradeMode::Real {
            self.count_real(s.pnl);
        }
        Some(s)
    }

    fn count_real(&mut self, pnl: f64) {
        self.real_pnl += pnl;
        if self.mode == TradeMode::Real && self.real_pnl <= -self.loss_limit {
            self.mode = TradeMode::Off;
            self.stop_reason = Some(format!(
                "real trading stopped: lost {:.2}, the loss limit is {:.2}",
                -self.real_pnl, self.loss_limit
            ));
        }
    }

    /// The pair a run of losses is held on, while it can still be traded
    /// there: same timeframe, still streaming, open and paying enough.
    /// Otherwise the multiplied stake goes to whichever pair signals next.
    fn held_run(&self) -> Option<(AssetId, i64)> {
        let (id, period_ms) = self.run_on.filter(|_| self.paper.in_run())?;
        let streaming = now_ms() - self.series[id as usize].seen_ms < 2 * period_ms + 10_000;
        let ok = period_ms == self.learner.agg.period_ms() && streaming && self.tradeable(id) && self.auto_allows(id);
        ok.then_some((id, period_ms))
    }

    /// Whether the pair is on the list auto mode may trade (all pairs when the list is empty).
    fn auto_allows(&self, id: AssetId) -> bool {
        self.auto_list.is_empty() || self.auto_list.contains(&self.series[id as usize].name)
    }

    /// Drops the open trade once its pair has sent no prices for two candles,
    /// so one stalled pair can't block auto mode all night. A real-money order
    /// behind it counts as lost toward the loss limit.
    fn drop_stale_trade(&mut self) {
        let (Some((id, period_ms)), Some(trade)) = (self.trade_on, self.paper.open) else { return };
        if now_ms() - self.series[id as usize].seen_ms < 2 * period_ms + 10_000 {
            return;
        }
        self.paper.void();
        self.trade_on = None;
        self.open_pick = None;
        if std::mem::replace(&mut self.open_mirror, TradeMode::Off) == TradeMode::Real {
            self.count_real(-trade.stake);
        }
        if let Some(ui) = &self.ui {
            ui.send(Event::Opened(None));
        }
        self.push_status();
    }

    /// Pocket Option only streams the pairs open there. When the selected pair
    /// stops while others keep streaming (its chart was closed or swapped), the
    /// app moves to the streaming pair with the best payout instead of freezing.
    fn follow_streaming(&mut self) -> io::Result<()> {
        let Some(active) = self.active else { return Ok(()) };
        let now = now_ms();
        let s = &self.series[active as usize];
        if self.cfg.asset.is_some() || is_demo(&s.name) || now - s.seen_ms < STOPPED_MS {
            return Ok(());
        }
        let payout = |id: AssetId| self.payout_of(id).map_or(0.0, |(p, open)| if open { p } else { 0.0 });
        let next = (0..self.series.len() as AssetId)
            .filter(|&i| !is_demo(&self.series[i as usize].name) && now - self.series[i as usize].seen_ms < STREAMING_MS)
            .max_by(|&a, &b| payout(a).total_cmp(&payout(b)));
        match next {
            Some(id) => self.select(id, self.learner.agg.period_ms()),
            None => Ok(()),
        }
    }

    /// Mirrors a paper trade to Pocket Option when that's switched on.
    fn place_order(&mut self, id: AssetId, trade: Trade, period_ms: i64, time_ms: i64) {
        let asset = &self.series[id as usize].name;
        let Some(ui) = &self.ui else { return };
        if self.mode == TradeMode::Off || is_demo(asset) {
            return;
        }
        self.order_seq += 1;
        self.open_mirror = self.mode;
        ui.place_order(OrderSent {
            account: self.mode,
            request_id: (now_ms() * 100 + (self.order_seq % 100) as i64).to_string(),
            asset: asset.clone(),
            call: trade.side == Side::Call,
            amount: (trade.stake * 100.0).round() / 100.0,
            expiry_secs: period_ms / 1000,
            time_ms,
        });
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
                self.select(id, self.learner.agg.period_ms())
            }
            Some(cur) if cur != id && !self.switched_to_real && is_demo(&self.series[cur as usize].name) && !demo => {
                self.switched_to_real = true;
                self.select(id, self.learner.agg.period_ms())
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
        if save {
            s.append_log(self.cfg, &ticks)?;
        }
        for &t in ticks.iter().rev() {
            s.ticks.push_front(t);
        }
        while s.ticks.len() > MAX_TICKS {
            s.ticks.pop_front();
        }
        self.rebuild_learners(id);
        self.push_assets();
        if self.active == Some(id) {
            self.select(id, self.learner.agg.period_ms())?;
        }
        // Chart history from the bridge is a burst of new learning, worth keeping straight away.
        if save {
            self.save_training();
        } else {
            self.push_training();
        }
        Ok(())
    }

    /// Switches asset and/or timeframe: the asset's buffered ticks are replayed
    /// (without trading) for the chart and to warm up the indicators, the
    /// training it has built up so far is merged in, then a fresh paper account goes live.
    fn select(&mut self, id: AssetId, period_ms: i64) -> io::Result<()> {
        self.ensure_series(id);
        let period_ms = period_ms.max(MIN_PERIOD_MS);
        self.ensure_period(period_ms);
        self.active = Some(id);
        self.p.period_ms = period_ms;
        let trained = self.series[id as usize]
            .learners
            .iter()
            .find(|l| l.agg.period_ms() == period_ms)
            .and_then(Learner::snapshot);
        let breakeven = 1.0 / (1.0 + self.payout_or_default());
        self.learner = Learner::new(period_ms, self.params(), breakeven, trained);
        // Auto mode's account and open trade carry on; the replay gets an account of its own.
        let kept = if self.auto {
            let fresh = self.new_paper();
            Some((std::mem::replace(&mut self.paper, fresh), self.trade_on.take(), self.open_pick.take(), self.open_mirror, self.run_on))
        } else {
            None
        };
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
                records: *self.learner.engine.records(),
                wins: self.paper.wins,
                losses: self.paper.losses,
                ties: self.paper.ties,
                pnl: self.paper.balance - self.paper.start_balance,
            });
        }
        self.replay_candles = Vec::new();
        self.reset_account();
        if let Some((paper, trade_on, pick, mirror, run_on)) = kept {
            self.paper = paper;
            self.trade_on = trade_on;
            self.open_pick = pick;
            self.open_mirror = mirror;
            self.run_on = run_on;
        }
        self.phase(Phase::Live);
        self.push_assets();
        self.apply_payout();
        self.push_training();
        Ok(())
    }

    /// The active asset's payout and open flag, if they are known. Assets from
    /// sources without payout data (demo pairs, files, the CLI) use `--payout`.
    fn active_payout(&self) -> Option<(f64, bool)> {
        self.payout_of(self.active?)
    }

    fn payout_of(&self, id: AssetId) -> Option<(f64, bool)> {
        if let Some(&p) = self.payouts.get(&id) {
            return Some(p);
        }
        let demo = self.series.get(id as usize).is_some_and(|s| is_demo(&s.name));
        (!self.cfg.require_payout || demo).then_some((self.cfg.payout, true))
    }

    /// Open, and paying at least `--min-payout`.
    fn tradeable(&self, id: AssetId) -> bool {
        self.payout_of(id).is_some_and(|(p, open)| open && p + 1e-9 >= self.cfg.min_payout)
    }

    fn payout_or_default(&self) -> f64 {
        self.active_payout().map_or(self.cfg.payout, |(p, _)| p)
    }

    /// Re-prices the engine and account for the active asset's payout, and
    /// stops trading while it is closed or pays less than `--min-payout`.
    fn apply_payout(&mut self) {
        let payout = self.payout_or_default();
        self.paper.payout = payout;
        self.learner.engine.set_breakeven(1.0 / (1.0 + payout));
        self.can_trade = self.active.is_some_and(|id| self.tradeable(id));
        // Background learners decide auto mode's trades, so each needs its own pair's break-even.
        let breakevens: Vec<f64> = self.series.iter().map(|s| self.breakeven_for(&s.name)).collect();
        for (s, be) in self.series.iter_mut().zip(breakevens) {
            for l in &mut s.learners {
                l.engine.set_breakeven(be);
            }
        }
        self.push_status();
    }

    fn push_status(&self) {
        if let Some(ui) = &self.ui {
            let known = self.active_payout();
            ui.send(Event::Status(Status {
                mode: self.mode,
                paused: self.paused,
                loss_limit: self.loss_limit,
                real_pnl: self.real_pnl,
                stop_reason: self.stop_reason.clone(),
                stake: self.stake,
                stake_pct: self.stake_pct,
                base_amount: self.paper.base_amount(),
                payout: known.map(|(p, _)| p),
                open: known.is_some_and(|(_, open)| open),
                can_trade: self.can_trade,
                min_payout: self.cfg.min_payout,
                next_stake: self.paper.next_stake(),
                can_afford: self.paper.can_afford(),
                step: self.paper.step,
                steps: self.martingale.steps,
                mult: self.martingale.mult,
                z: self.params.z,
                track_z: self.params.track_z,
                track_min: self.params.track_min,
                frequency: Frequency::of(&self.params),
                awake: self.awake,
                awake_ok: self.awake == Awake::Off || self.awake_guard.is_some(),
                auto: self.auto,
                auto_pairs: self.auto_pairs(),
                held_on: self.held_run().filter(|_| self.auto).map(|(id, _)| self.series[id as usize].name.clone()),
                auto_list: self.auto_list.clone(),
                balance: self.paper.balance,
                start_balance: self.paper.start_balance,
                sync: self.sync,
                po_demo: self.po_balance[1],
                po_real: self.po_balance[0],
            }));
        }
    }

    /// Pairs auto mode could trade now: the same kind as the selected one,
    /// streaming, open and paying enough.
    fn auto_pairs(&self) -> usize {
        let Some(active) = self.active else { return 0 };
        let demo = is_demo(&self.series[active as usize].name);
        let now = now_ms();
        (0..self.series.len() as AssetId)
            .filter(|&i| {
                let s = &self.series[i as usize];
                is_demo(&s.name) == demo && now - s.seen_ms < STREAMING_MS && self.tradeable(i) && self.auto_allows(i)
            })
            .count()
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
                    seen_ms: s.seen_ms,
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

    fn new_paper(&self) -> Paper {
        let balance = self.synced_balance().unwrap_or(self.balance);
        let mut paper = Paper::new(balance, self.stake, self.payout_or_default(), self.martingale);
        paper.stake_pct = self.stake_pct;
        paper
    }

    /// The Pocket Option balance to follow: real money's when trades go there,
    /// otherwise the demo account's.
    fn synced_balance(&self) -> Option<f64> {
        let demo = self.mode != TradeMode::Real;
        self.po_balance[demo as usize].filter(|_| self.sync)
    }

    /// Copies the Pocket Option balance into the account when syncing.
    /// `restart` makes it the new starting point for P&L.
    fn apply_sync(&mut self, restart: bool) {
        if let Some(balance) = self.synced_balance() {
            self.paper.set_balance(balance, restart);
        }
    }

    /// Keeps what the engine has learned, starts a fresh account and stats.
    fn reset_account(&mut self) {
        self.paper = self.new_paper();
        self.close_lat = Histogram::default();
        self.tick_lat = Histogram::default();
        self.signals = 0;
        self.open_pick = None;
        self.trade_on = None;
        self.run_on = None;
        self.open_mirror = TradeMode::Off;
    }

    /// Tick → candle → rules → decision, with no allocation, locking or I/O
    /// before the decision is made.
    #[inline]
    fn on_tick(&mut self, tick: Tick) -> io::Result<()> {
        let t0 = Instant::now();

        if let Some((closed, sig)) = self.learner.push(tick) {
            let period_ms = self.learner.agg.period_ms();
            // Live in auto mode, `background_close` runs the trades on every pair, this one included.
            let auto = self.live && self.auto;
            let mine = self.active.is_some_and(|id| self.trade_on == Some((id, period_ms)));
            let settled = if !auto && mine { self.settle(&closed) } else { None };
            let settled_rule = settled.and_then(|_| self.open_pick.take());
            let new_slot = self.learner.agg.slot(tick.ts_ms);
            // Payout gating and pausing only apply live; history replays show what the rules would do.
            let allowed = !self.live || (self.can_trade && !self.paused);
            let opened = !auto && allowed && sig.side.is_some_and(|side| self.paper.open(side, tick.price, new_slot));
            if opened {
                self.trade_on = self.active.map(|id| (id, period_ms));
            }
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
            if opened {
                self.open_pick = sig.best;
                if let (Some(trade), Some(id)) = (self.paper.open, self.active) {
                    self.place_order(id, trade, period_ms, tick.ts_ms);
                }
            }
            if let Some(ui) = &self.ui {
                if let (Some(s), Some(id)) = (settled, self.active) {
                    ui.send(Event::Settled {
                        s,
                        rule: settled_rule,
                        balance: self.paper.balance,
                        asset: self.series[id as usize].name.clone(),
                        period_ms,
                    });
                }
                if settled.is_some() || opened {
                    self.push_status();
                }
                ui.send(Event::Closed {
                    candle: closed,
                    sig,
                    entry: opened.then_some((tick.price, new_slot)),
                    ns: dt.as_nanos() as u64,
                    records: *self.learner.engine.records(),
                });
            }
            if !self.quiet {
                if let Some(s) = settled {
                    self.p.settlement(&mut self.out, &s, &self.paper)?;
                }
                let skipped = if auto {
                    "auto mode trades from all pairs"
                } else if !allowed && self.paused {
                    "paused"
                } else if allowed {
                    "insufficient balance"
                } else {
                    "payout below minimum or pair closed"
                };
                self.p.candle(&mut self.out, &closed, &sig, opened.then_some(tick.price), skipped, dt.as_nanos() as u64)?;
                self.out.flush()?;
            }
            return Ok(());
        }

        let show = self.live && (!self.quiet || self.ui.is_some());
        if self.cfg.heads_up && !self.learning && show && self.learner.agg.is_late(tick.ts_ms) {
            if let Some(forming) = self.learner.agg.current().copied() {
                if forming.slot != self.previewed_slot {
                    self.previewed_slot = forming.slot;
                    let sig = self.learner.engine.peek(&forming);
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
            if let (Some(ui), Some(forming)) = (&self.ui, self.learner.agg.current()) {
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
        self.p.leaderboard(&mut self.out, &self.learner.engine)
    }
}

/// The real and demo balances saved by an earlier session, one per line.
fn load_balances(path: &std::path::Path) -> [Option<f64>; 2] {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut lines = text.lines().map(|l| l.trim().parse::<f64>().ok().filter(|b| b.is_finite() && *b >= 0.0));
    [lines.next().flatten(), lines.next().flatten()]
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

    fn candle(&self, out: &mut impl Write, c: &Candle, sig: &Signal, entry: Option<f64>, skipped: &str, ns: u64) -> io::Result<()> {
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
            (Some(side), None) => write!(out, "{} (skipped: {skipped})", self.side_label(side))?,
            (None, _) => {
                let why = match sig.hold {
                    Some(Hold::Conflict) => "rules disagree".to_owned(),
                    Some(Hold::Proving) => format!("proving its picks ({:.0} so far)", sig.track.n),
                    Some(Hold::Unproven) => format!(
                        "its picks win {:.1}% (≥{:.1}%), not enough",
                        sig.track.rate * 100.0,
                        sig.track.lower * 100.0
                    ),
                    _ => "no edge".to_owned(),
                };
                write!(out, "{}", self.paint("2", &format!("hold ({why})   best: {}", self.rule(sig))))?
            }
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
            "       └ {side} {:.d$} → {:.d$}  stake {:.2}  {result}   balance {:.2}   {}W/{}L/{}T {:.1}%",
            s.trade.entry,
            s.exit,
            s.trade.stake,
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
        if paper.staked > 0.0 {
            let sizing = if paper.martingale.steps > 0 {
                format!("×{} after a loss, up to {} times", paper.martingale.mult, paper.martingale.steps)
            } else {
                "fixed".to_owned()
            };
            let base = if paper.stake_pct > 0.0 {
                format!("{}% of balance", paper.stake_pct)
            } else {
                format!("{:.2}", paper.base_stake)
            };
            writeln!(
                out,
                "stakes    base {base}, {sizing}   largest {:.2}   total staked {:.2}   return {:+.1}% of staked",
                paper.max_stake,
                paper.staked,
                pnl / paper.staked * 100.0,
            )?;
        }
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
