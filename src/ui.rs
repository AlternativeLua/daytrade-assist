//! Local web UI and bridge endpoint. A hub thread owns all UI state and streams
//! it to browsers over server-sent events, so the trading loop's only cost is a
//! channel send made after each decision.

use std::collections::{HashMap, VecDeque};
use std::fmt::Write as _;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::thread;
use std::time::Duration;

use crate::candle::Candle;
use crate::feed::{AssetCache, Input, Registry, Tick, now_ms, parse_line};
use crate::latency::Histogram;
use crate::paper::{Outcome, Settlement};
use crate::strategy::{EXPERTS, Pick, Record, Side, Signal, expert_name};

const INDEX: &str = include_str!("../ui/index.html");
const MAX_CANDLES: usize = 3000;
const MAX_JOURNAL: usize = 50_000;
/// A batch with more events than this is sent as one snapshot instead.
const SNAPSHOT_AFTER: usize = 200;
const MAX_BODY: usize = 32 << 20;

pub struct UiConfig {
    pub addr: String,
    pub journal: Option<String>,
    pub extension_dir: Option<String>,
    pub stake: f64,
    pub payout: f64,
    pub balance: f64,
    pub z: f64,
    pub min_samples: f64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    /// Learning from buffered ticks after an asset or timeframe switch.
    History,
    Live,
    Ended,
}

#[derive(Clone, Debug)]
pub struct AssetInfo {
    pub name: String,
    pub ticks: usize,
    pub first_ts: i64,
    pub last_ts: i64,
    pub last_price: f64,
}

pub enum Event {
    Phase(Phase),
    Active {
        asset: String,
        period_ms: i64,
    },
    Assets(Vec<AssetInfo>),
    Replayed {
        total_candles: usize,
        candles: Vec<Candle>,
        records: [Record; EXPERTS],
        wins: u32,
        losses: u32,
        ties: u32,
        pnl: f64,
    },
    Forming {
        candle: Candle,
        ts_ms: i64,
    },
    Closed {
        candle: Candle,
        sig: Signal,
        /// Entry price and the slot of the candle the trade runs over.
        entry: Option<(f64, i64)>,
        ns: u64,
        records: [Record; EXPERTS],
    },
    Preview {
        slot: i64,
        sig: Signal,
    },
    Settled {
        s: Settlement,
        rule: Option<Pick>,
        balance: f64,
    },
    Status(Status),
}

/// Trading conditions for the active pair: payout gate and stake sizing.
#[derive(Clone, Copy, Debug)]
pub struct Status {
    pub payout: Option<f64>,
    pub open: bool,
    pub can_trade: bool,
    pub min_payout: f64,
    pub next_stake: f64,
    pub step: u32,
    pub steps: u32,
    pub mult: f64,
    pub z: f64,
}

/// One row of the broker's asset table.
struct MarketRow {
    asset: String,
    name: String,
    kind: String,
    payout: f64,
    open: bool,
}

enum Msg {
    Event(Box<Event>),
    Client(TcpStream),
    Bridge(usize),
    Market(Vec<MarketRow>),
}

#[derive(Clone)]
pub struct Ui {
    tx: Sender<Msg>,
}

struct Shared {
    addr: String,
    registry: Arc<Registry>,
    input: SyncSender<Input>,
    hub: Sender<Msg>,
    extension_dir: Option<String>,
}

impl Ui {
    pub fn start(cfg: UiConfig, registry: Arc<Registry>, input: SyncSender<Input>) -> io::Result<Self> {
        let listener = TcpListener::bind(&cfg.addr)?;
        let (tx, rx) = mpsc::channel();
        let shared = Arc::new(Shared {
            addr: cfg.addr.clone(),
            registry,
            input,
            hub: tx.clone(),
            extension_dir: cfg.extension_dir.clone(),
        });
        let hub = Hub::new(cfg)?;
        thread::Builder::new().name("ui-hub".into()).spawn(move || hub.run(rx))?;
        thread::Builder::new()
            .name("ui-http".into())
            .spawn(move || serve(listener, shared))?;
        Ok(Self { tx })
    }

    #[inline]
    pub fn send(&self, e: Event) {
        let _ = self.tx.send(Msg::Event(Box::new(e)));
    }
}

fn serve(listener: TcpListener, shared: Arc<Shared>) {
    for stream in listener.incoming().flatten() {
        let shared = shared.clone();
        thread::spawn(move || {
            let _ = handle(stream, &shared);
        });
    }
}

struct Request {
    method: String,
    path: String,
    origin: Option<String>,
    body: Vec<u8>,
}

fn read_request(stream: &TcpStream) -> io::Result<Request> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_owned();
    let path = parts.next().unwrap_or("/").split('?').next().unwrap_or("/").to_owned();
    let mut len = 0usize;
    let mut origin = None;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? <= 2 {
            break;
        }
        if let Some((k, v)) = header.split_once(':') {
            let v = v.trim();
            match k.trim().to_ascii_lowercase().as_str() {
                "content-length" => len = v.parse().unwrap_or(0),
                "origin" => origin = Some(v.to_owned()),
                _ => {}
            }
        }
    }
    if len > MAX_BODY {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "body too large"));
    }
    let mut body = vec![0; len];
    reader.read_exact(&mut body)?;
    Ok(Request {
        method,
        path,
        origin,
        body,
    })
}

fn respond(stream: &mut TcpStream, status: &str, kind: &str, body: &str) -> io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn handle(mut stream: TcpStream, shared: &Shared) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_nodelay(true)?;
    let req = read_request(&stream)?;
    // Bridge requests come from the extension; UI requests from this page only.
    // Anything else (another website poking localhost) is refused.
    let from_extension = req
        .origin
        .as_deref()
        .is_none_or(|o| o.starts_with("chrome-extension://") || o.starts_with("moz-extension://"));
    let from_ui = req.origin.as_deref().is_none_or(|o| {
        let host = o.trim_start_matches("http://");
        host == shared.addr || host.replace("localhost", "127.0.0.1") == shared.addr
    });

    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/" | "/index.html") => respond(&mut stream, "200 OK", "text/html; charset=utf-8", INDEX),
        ("GET", "/events") => {
            stream.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\nConnection: keep-alive\r\n\r\n",
            )?;
            stream.set_write_timeout(Some(Duration::from_secs(2)))?;
            let _ = shared.hub.send(Msg::Client(stream));
            Ok(())
        }
        ("POST", "/ingest") if from_extension => {
            let body = String::from_utf8_lossy(&req.body);
            if body.starts_with("assets") {
                let n = ingest_assets(&body, shared);
                return respond(&mut stream, "200 OK", "text/plain", &n.to_string());
            }
            let n = ingest(&body, shared);
            let _ = shared.hub.send(Msg::Bridge(n));
            respond(&mut stream, "200 OK", "text/plain", &n.to_string())
        }
        ("POST", "/api/select") if from_ui => {
            let form = parse_form(&String::from_utf8_lossy(&req.body));
            let asset = form.get("asset").and_then(|a| shared.registry.find(a));
            let period_ms = form.get("tf").and_then(|t| t.parse::<i64>().ok()).map(|s| s * 1000);
            match (asset, period_ms) {
                (Some(asset), Some(period_ms)) if (1_000..=86_400_000).contains(&period_ms) => {
                    let _ = shared.input.send(Input::Select { asset, period_ms });
                    respond(&mut stream, "200 OK", "text/plain", "ok")
                }
                _ => respond(&mut stream, "400 Bad Request", "text/plain", "unknown asset or timeframe"),
            }
        }
        ("POST", "/api/settings") if from_ui => {
            let form = parse_form(&String::from_utf8_lossy(&req.body));
            let martingale_steps = form.get("martingale").and_then(|v| v.parse::<u32>().ok()).filter(|n| *n <= 8);
            let z = form.get("z").and_then(|v| v.parse::<f64>().ok()).filter(|z| (0.0..=4.0).contains(z));
            let _ = shared.input.send(Input::Settings { martingale_steps, z });
            respond(&mut stream, "200 OK", "text/plain", "ok")
        }
        ("POST", "/api/reveal-extension") if from_ui => {
            if let Some(dir) = &shared.extension_dir {
                reveal(dir);
            }
            respond(&mut stream, "200 OK", "text/plain", "ok")
        }
        _ => respond(&mut stream, "404 Not Found", "text/plain", ""),
    }
}

/// Body: a `stream` or `history` line, then `asset,timestamp,price` lines.
/// Stream ticks go to the engine as live ticks, history as per-asset backfills.
fn ingest(body: &str, shared: &Shared) -> usize {
    let mut lines = body.lines();
    let history = lines.next().is_some_and(|l| l.trim() == "history");
    let mut cache = AssetCache::new(shared.registry.clone());
    let mut backfills: HashMap<u16, Vec<Tick>> = HashMap::new();
    let mut n = 0;
    for line in lines {
        let Some(l) = parse_line(line) else { continue };
        let Some(asset) = l.asset else { continue };
        let id = cache.id(asset);
        n += 1;
        if history {
            backfills.entry(id).or_default().push(l.tick);
        } else if shared.input.send(Input::Tick(id, l.tick)).is_err() {
            break;
        }
    }
    for (id, ticks) in backfills {
        let _ = shared.input.send(Input::Backfill { asset: id, ticks, save: true });
    }
    n
}

/// Body: `assets`, then `asset,payout%,open,kind,name` lines from the broker's
/// asset table. Payouts go to the engine; the table goes to the UI.
fn ingest_assets(body: &str, shared: &Shared) -> usize {
    let mut cache = AssetCache::new(shared.registry.clone());
    let mut rows = Vec::new();
    let mut payouts = Vec::new();
    for line in body.lines().skip(1) {
        let mut f = line.splitn(5, ',');
        let (Some(asset), Some(payout), Some(open), Some(kind)) = (f.next(), f.next(), f.next(), f.next()) else {
            continue;
        };
        let Ok(payout) = payout.trim().parse::<f64>() else { continue };
        let asset = asset.trim();
        if asset.is_empty() || !(0.0..=1000.0).contains(&payout) {
            continue;
        }
        let payout = payout / 100.0;
        let open = open.trim() == "1";
        payouts.push((cache.id(asset), payout, open));
        rows.push(MarketRow {
            asset: asset.to_owned(),
            name: f.next().unwrap_or(asset).trim().to_owned(),
            kind: kind.trim().to_owned(),
            payout,
            open,
        });
    }
    let n = rows.len();
    let _ = shared.input.send(Input::Payouts(payouts));
    let _ = shared.hub.send(Msg::Market(rows));
    let _ = shared.hub.send(Msg::Bridge(0));
    n
}

fn parse_form(body: &str) -> HashMap<String, String> {
    body.split('&')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (url_decode(k), url_decode(v)))
        .collect()
}

fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                match std::str::from_utf8(&bytes[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(b) => {
                        out.push(b);
                        i += 2;
                    }
                    None => out.push(b'%'),
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn reveal(dir: &str) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(dir).spawn();
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("explorer").arg(dir).spawn();
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
}

struct Hub {
    cfg: UiConfig,
    session: i64,
    phase: Phase,
    asset: String,
    period_ms: i64,
    assets: String,
    candles: VecDeque<Candle>,
    forming: Option<(Candle, i64)>,
    signal: String,
    open: String,
    preview: String,
    trades: Vec<String>,
    backtest: String,
    journal: VecDeque<String>,
    journal_file: Option<File>,
    rules: String,
    status: String,
    market: String,
    balance: f64,
    signals: u32,
    lat: Histogram,
    bridge_last: i64,
    bridge_ticks: u64,
    bridge_sent: i64,
    clients: Vec<TcpStream>,
}

impl Hub {
    fn new(cfg: UiConfig) -> io::Result<Self> {
        let mut journal = VecDeque::new();
        let mut journal_file = None;
        if let Some(path) = &cfg.journal {
            if let Ok(f) = File::open(path) {
                for line in BufReader::new(f).lines().map_while(Result::ok) {
                    if line.starts_with('{') {
                        if journal.len() == MAX_JOURNAL {
                            journal.pop_front();
                        }
                        journal.push_back(line);
                    }
                }
            }
            let f = OpenOptions::new().create(true).append(true).open(path);
            journal_file = Some(f.map_err(|e| io::Error::new(e.kind(), format!("{path}: {e}")))?);
        }
        Ok(Self {
            balance: cfg.balance,
            cfg,
            session: now_ms(),
            phase: Phase::History,
            asset: String::new(),
            period_ms: 60_000,
            assets: "[]".into(),
            candles: VecDeque::with_capacity(MAX_CANDLES),
            forming: None,
            signal: "null".into(),
            open: "null".into(),
            preview: "null".into(),
            trades: Vec::new(),
            backtest: "null".into(),
            journal,
            journal_file,
            rules: "[]".into(),
            status: "null".into(),
            market: "[]".into(),
            signals: 0,
            lat: Histogram::default(),
            bridge_last: 0,
            bridge_ticks: 0,
            bridge_sent: 0,
            clients: Vec::new(),
        })
    }

    fn run(mut self, rx: Receiver<Msg>) {
        while let Ok(first) = rx.recv() {
            let mut out = Vec::new();
            let mut forming = None;
            let mut events = 0usize;
            let mut next = Some(first);
            while let Some(msg) = next.take() {
                match msg {
                    Msg::Client(stream) => self.add_client(stream),
                    Msg::Market(rows) => {
                        self.market = market_json(rows);
                        out.push(format!(r#"{{"type":"market","market":{}}}"#, self.market));
                    }
                    Msg::Bridge(n) => {
                        self.bridge_last = now_ms();
                        self.bridge_ticks += n as u64;
                        if self.bridge_last - self.bridge_sent >= 1000 {
                            self.bridge_sent = self.bridge_last;
                            out.push(format!(r#"{{"type":"bridge","bridge":{}}}"#, self.bridge_json()));
                        }
                    }
                    Msg::Event(e) => {
                        events += 1;
                        if !matches!(*e, Event::Forming { .. } | Event::Preview { .. } | Event::Assets(_)) {
                            forming = None;
                        }
                        if let Some(json) = self.apply(*e) {
                            if json.starts_with(r#"{"type":"forming""#) {
                                forming = Some(json);
                            } else if events <= SNAPSHOT_AFTER {
                                out.push(json);
                            }
                        }
                    }
                }
                if events >= 20_000 {
                    break;
                }
                next = rx.try_recv().ok();
            }
            if events > SNAPSHOT_AFTER {
                out.clear();
                out.push(self.snapshot());
            } else if let Some(f) = forming {
                out.push(f);
            }
            self.broadcast(&out);
        }
    }

    fn add_client(&mut self, mut stream: TcpStream) {
        let snap = self.snapshot();
        if write!(stream, "data: {snap}\n\n").and_then(|_| stream.flush()).is_ok() {
            self.clients.push(stream);
        }
    }

    fn broadcast(&mut self, msgs: &[String]) {
        if msgs.is_empty() || self.clients.is_empty() {
            return;
        }
        let mut payload = String::new();
        for m in msgs {
            let _ = write!(payload, "data: {m}\n\n");
        }
        self.clients
            .retain_mut(|c| c.write_all(payload.as_bytes()).and_then(|_| c.flush()).is_ok());
    }

    fn candle_json(&self, c: &Candle) -> String {
        format!(
            "[{},{},{},{},{}]",
            c.slot * self.period_ms / 1000,
            num(c.open),
            num(c.high),
            num(c.low),
            num(c.close)
        )
    }

    fn apply(&mut self, e: Event) -> Option<String> {
        match e {
            Event::Phase(phase) => {
                self.phase = phase;
                if phase == Phase::History {
                    self.candles.clear();
                    self.forming = None;
                    self.signal = "null".into();
                    self.preview = "null".into();
                    self.backtest = "null".into();
                }
                if phase != Phase::Ended {
                    self.trades.clear();
                    self.open = "null".into();
                    self.balance = self.cfg.balance;
                    self.signals = 0;
                    self.lat = Histogram::default();
                }
                Some(self.snapshot())
            }
            Event::Active { asset, period_ms } => {
                self.asset = asset;
                self.period_ms = period_ms;
                None
            }
            Event::Assets(list) => {
                let mut s = String::from("[");
                for (i, a) in list.iter().enumerate() {
                    if i > 0 {
                        s.push(',');
                    }
                    let _ = write!(
                        s,
                        r#"{{"name":"{}","ticks":{},"first":{},"last":{},"price":{}}}"#,
                        escape(&a.name),
                        a.ticks,
                        a.first_ts,
                        a.last_ts,
                        num(a.last_price)
                    );
                }
                s.push(']');
                if s == self.assets {
                    return None;
                }
                self.assets = s;
                Some(format!(r#"{{"type":"assets","assets":{}}}"#, self.assets))
            }
            Event::Replayed {
                total_candles,
                candles,
                records,
                wins,
                losses,
                ties,
                pnl,
            } => {
                self.candles = candles.iter().copied().collect();
                self.rules = rules_json(&records);
                self.backtest = format!(
                    r#"{{"wins":{wins},"losses":{losses},"ties":{ties},"pnl":{},"candles":{}}}"#,
                    num(pnl),
                    total_candles
                );
                None
            }
            Event::Forming { candle, ts_ms } => {
                self.forming = Some((candle, ts_ms));
                Some(format!(r#"{{"type":"forming","c":{},"ts":{ts_ms}}}"#, self.candle_json(&candle)))
            }
            Event::Closed {
                candle,
                sig,
                entry,
                ns,
                records,
            } => {
                if self.candles.len() == MAX_CANDLES {
                    self.candles.pop_front();
                }
                self.candles.push_back(candle);
                if self.forming.is_some_and(|(f, _)| f.slot <= candle.slot) {
                    self.forming = None;
                }
                self.lat.record(Duration::from_nanos(ns));
                self.signals += sig.side.is_some() as u32;
                self.signal = self.signal_json(&sig, candle.slot);
                self.preview = "null".into();
                if let (Some((price, slot)), Some(side), Some(pick)) = (entry, sig.side, sig.best) {
                    self.open = format!(
                        r#"{{"time":{},"side":"{}","entry":{},"rule":"{}","rate":{}}}"#,
                        slot * self.period_ms,
                        side_name(side),
                        num(price),
                        rule_name(pick.expert),
                        num(pick.rate),
                    );
                }
                self.rules = rules_json(&records);
                Some(format!(
                    r#"{{"type":"closed","c":{},"signal":{},"open":{},"rules":{},"stats":{}}}"#,
                    self.candle_json(&candle),
                    self.signal,
                    self.open,
                    self.rules,
                    self.stats_json(),
                ))
            }
            Event::Status(st) => {
                self.status = format!(
                    r#"{{"payout":{},"open":{},"canTrade":{},"minPayout":{},"nextStake":{},"step":{},"steps":{},"mult":{},"z":{}}}"#,
                    st.payout.map_or("null".into(), num),
                    st.open,
                    st.can_trade,
                    num(st.min_payout),
                    num(st.next_stake),
                    st.step,
                    st.steps,
                    num(st.mult),
                    num(st.z),
                );
                Some(format!(r#"{{"type":"status","status":{}}}"#, self.status))
            }
            Event::Preview { slot, sig } => {
                self.preview = format!(r#"{{"slot":{slot},"signal":{}}}"#, self.signal_json(&sig, slot));
                Some(format!(r#"{{"type":"preview","preview":{}}}"#, self.preview))
            }
            Event::Settled { s, rule, balance } => {
                self.balance = balance;
                self.open = "null".into();
                let trade = self.trade_json(&s, rule);
                self.trades.push(trade.clone());
                let journaled = self.phase == Phase::Live && self.journal_file.is_some();
                if journaled {
                    if let Some(f) = &mut self.journal_file {
                        let _ = writeln!(f, "{trade}").and_then(|_| f.flush());
                    }
                    if self.journal.len() == MAX_JOURNAL {
                        self.journal.pop_front();
                    }
                    self.journal.push_back(trade.clone());
                }
                Some(format!(
                    r#"{{"type":"settled","trade":{trade},"journaled":{journaled},"stats":{}}}"#,
                    self.stats_json()
                ))
            }
        }
    }

    fn signal_json(&self, sig: &Signal, slot: i64) -> String {
        let side = sig.side.map_or("null".into(), |s| format!("\"{}\"", side_name(s)));
        let best = sig.best.map_or("null".into(), |b| {
            format!(
                r#"{{"rule":"{}","side":"{}","rate":{},"lower":{},"n":{}}}"#,
                rule_name(b.expert),
                side_name(b.side),
                num(b.rate),
                num(b.lower),
                num(b.n)
            )
        });
        format!(
            r#"{{"time":{},"side":{side},"best":{best},"voting":{},"learning":{},"rsi":{},"z":{}}}"#,
            slot * self.period_ms,
            sig.voting,
            sig.learning.map_or("null".into(), num),
            num(sig.rsi),
            num(sig.z),
        )
    }

    fn trade_json(&self, s: &Settlement, rule: Option<Pick>) -> String {
        let outcome = match s.outcome {
            Outcome::Win => "WIN",
            Outcome::Loss => "LOSS",
            Outcome::Tie => "TIE",
        };
        format!(
            r#"{{"session":{},"asset":"{}","tf":{},"source":"{}","time":{},"side":"{}","entry":{},"exit":{},"outcome":"{outcome}","pnl":{},"stake":{},"step":{},"payout":{},"rule":{},"rate":{}}}"#,
            self.session,
            escape(&self.asset),
            self.period_ms / 1000,
            if self.asset.starts_with("DEMO:") { "demo" } else { "feed" },
            s.trade.expiry_slot * self.period_ms,
            side_name(s.trade.side),
            num(s.trade.entry),
            num(s.exit),
            num(s.pnl),
            num(s.trade.stake),
            s.trade.step,
            num(s.trade.payout),
            rule.map_or("null".into(), |p| format!("\"{}\"", rule_name(p.expert))),
            rule.map_or("null".into(), |p| num(p.rate)),
        )
    }

    fn stats_json(&self) -> String {
        format!(
            r#"{{"balance":{},"signals":{},"candles":{},"p50":{},"p99":{},"max":{}}}"#,
            num(self.balance),
            self.signals,
            self.lat.count(),
            self.lat.quantile_ns(0.5),
            self.lat.quantile_ns(0.99),
            self.lat.max_ns(),
        )
    }

    fn bridge_json(&self) -> String {
        format!(r#"{{"last":{},"ticks":{}}}"#, self.bridge_last, self.bridge_ticks)
    }

    fn snapshot(&self) -> String {
        let c = &self.cfg;
        let mut s = String::with_capacity(64 * (self.candles.len() + self.journal.len()) + 4096);
        let opt = |v: &Option<String>| v.as_deref().map_or("null".into(), |p| format!("\"{}\"", escape(p)));
        let _ = write!(
            s,
            r#"{{"type":"snapshot","config":{{"stake":{},"payout":{},"balance":{},"z":{},"minSamples":{},"journal":{},"extensionDir":{},"session":{}}},"phase":"{}","asset":"{}","tf":{},"assets":{},"bridge":{},"backtest":{},"status":{},"market":{},"candles":["#,
            num(c.stake),
            num(c.payout),
            num(c.balance),
            num(c.z),
            num(c.min_samples),
            opt(&c.journal),
            opt(&c.extension_dir),
            self.session,
            phase_name(self.phase),
            escape(&self.asset),
            self.period_ms / 1000,
            self.assets,
            self.bridge_json(),
            self.backtest,
            self.status,
            self.market,
        );
        join(&mut s, self.candles.iter().map(|c| self.candle_json(c)));
        let forming = self
            .forming
            .map_or("null".into(), |(f, ts)| format!(r#"{{"c":{},"ts":{ts}}}"#, self.candle_json(&f)));
        let _ = write!(s, r#"],"forming":{forming},"trades":["#);
        join(&mut s, self.trades.iter().cloned());
        s.push_str(r#"],"journal":["#);
        join(&mut s, self.journal.iter().cloned());
        let _ = write!(
            s,
            r#"],"signal":{},"open":{},"preview":{},"rules":{},"stats":{}}}"#,
            self.signal,
            self.open,
            self.preview,
            self.rules,
            self.stats_json()
        );
        s
    }
}

fn join(s: &mut String, items: impl Iterator<Item = String>) {
    for (i, item) in items.enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&item);
    }
}

fn num(x: f64) -> String {
    if x.is_finite() { format!("{x}") } else { "null".into() }
}

fn escape(s: &str) -> String {
    s.chars()
        .flat_map(|c| match c {
            '"' => vec!['\\', '"'],
            '\\' => vec!['\\', '\\'],
            c if (c as u32) < 0x20 => vec![' '],
            c => vec![c],
        })
        .collect()
}

fn phase_name(p: Phase) -> &'static str {
    match p {
        Phase::History => "history",
        Phase::Live => "live",
        Phase::Ended => "ended",
    }
}

fn side_name(side: Side) -> &'static str {
    match side {
        Side::Call => "CALL",
        Side::Put => "PUT",
    }
}

fn rule_name(expert: usize) -> String {
    let (verb, base) = expert_name(expert);
    format!("{verb} {base}")
}

/// Currency pairs only, best payout first.
fn market_json(mut rows: Vec<MarketRow>) -> String {
    rows.retain(|r| r.kind.eq_ignore_ascii_case("currency"));
    rows.sort_by(|a, b| b.payout.total_cmp(&a.payout).then_with(|| a.asset.cmp(&b.asset)));
    let mut s = String::from("[");
    for (i, r) in rows.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        let _ = write!(
            s,
            r#"{{"asset":"{}","name":"{}","payout":{},"open":{}}}"#,
            escape(&r.asset),
            escape(&r.name),
            num(r.payout),
            r.open
        );
    }
    s.push(']');
    s
}

fn rules_json(records: &[Record; EXPERTS]) -> String {
    let mut s = String::from("[");
    for (i, r) in records.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        let _ = write!(
            s,
            r#"{{"name":"{}","wins":{},"total":{},"w":{},"n":{}}}"#,
            rule_name(i),
            r.wins,
            r.total,
            num(r.recent_wins()),
            num(r.weight()),
        );
    }
    s.push(']');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_forms() {
        let f = parse_form("asset=DEMO%3AEURUSD&tf=60");
        assert_eq!(f["asset"], "DEMO:EURUSD");
        assert_eq!(f["tf"], "60");
        assert_eq!(url_decode("a+b%2"), "a b%2");
    }
}
