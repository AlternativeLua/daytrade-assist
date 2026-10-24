#[cfg(feature = "app")]
mod app;
mod candle;
mod feed;
mod indicators;
mod latency;
mod paper;
mod session;
mod strategy;
mod ui;

use std::fs::{self, File};
use std::io::{self, BufReader, IsTerminal, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::mpsc::{self, SyncSender};
use std::thread;

use feed::{Input, Registry, SimParams, read_backfill, spawn_lines, spawn_sim};
use session::{MAX_TICKS, Session, log_file_name};
use strategy::Params;
use ui::{Ui, UiConfig};

const DEFAULT_UI_ADDR: &str = "127.0.0.1:8787";

const USAGE: &str = "\
daytrade-assist — adaptive CALL/PUT signals for OTC currency pairs, with a paper account

USAGE:
    daytrade-assist                     Open the app (same as `daytrade-assist app`)
    daytrade-assist <SOURCE> [OPTIONS]

SOURCES:
    app                 Desktop app: Pocket Option bridge + demo pairs + UI window
    pocket              Pocket Option bridge + web UI, with terminal output
    sim                 Simulated ticks (backtests instantly unless --realtime)
    replay <FILE>       Replay ticks from a file
    stdin               Read ticks from stdin
    tcp <ADDR>          Listen on ADDR (e.g. 127.0.0.1:7000) and read ticks from one client

    Tick lines are `price`, `timestamp,price` or `asset,timestamp,price`
    (unix seconds or ms).

OPTIONS:
    --asset <NAME>      Pair to trade (default: the first one that ticks)
    --tf <TIME>         Candle timeframe and expiry: 15s, 30s, 1m, 2m, 5m … [default: 1m]
    --history <FILE>    Learn from logged ticks first, then run the source
    --record <FILE>     Append every incoming tick to FILE (use it as --history next time)
    --stake <N>         Base paper stake per trade       [default: 10]
    --payout <F>        Payout on a win when the feed doesn't report one, 0.85 or 85 [default: 0.85]
    --min-payout <F>    Don't trade pairs paying less than this [default: 0.85]
    --martingale <N>    After a loss, multiply the stake, up to N losses in a row,
                        then go back to the base stake; 0 = fixed stake [default: 4]
    --martingale-mult <F>  Stake multiplier after each loss   [default: 2]
    --balance <N>       Starting paper balance           [default: 1000]
    --digits <N>        Price decimals to print          [default: 5]
    --no-heads-up       Don't preview signals before the candle closes
    --quiet             Only print the final summary
    --ui                Serve the web UI (chart, signal, account, rules, trade history)
    --ui-addr <ADDR>    Address for the UI and bridge    [default: 127.0.0.1:8787]
    --journal <FILE>    Trade journal for the UI's History tab [default: in the data folder]
    --data-dir <DIR>    Where the app keeps tick logs, the journal and the bridge extension
    --no-demo           (app) Don't add the simulated DEMO pairs
    --demo              (pocket) Add the simulated DEMO pairs
    --browser           (app) Use the web browser instead of an app window

  signal frequency (lower = more signals, less proof each one is real):
    --z <F>             Confidence required: 0 = raw win rate, 1 = moderate, 2 = strict [default: 1]
    --margin <F>        Extra win rate required above break-even      [default: 0]
    --min-samples <N>   Votes a rule needs before it can be traded     [default: 50]
    --half-life <N>     Votes until old results count half (adaptivity) [default: 400]

  sim:
    --minutes <N>       [default: 600]    --seed <N>       [default: 1]
    --step-ms <N>       [default: 250]    --price <F>      [default: 1.085]
    --vol <F>           Volatility per minute [default: 0.0002]
    --revert <F>        Candle-to-candle reversion, 0 = random walk [default: 0]
    --prefill <N>       Minutes of instant history before the run [default: 0]
    --realtime          Pace ticks in real time
";

#[derive(PartialEq)]
enum Source {
    App,
    Pocket,
    Sim,
    Replay(String),
    Stdin,
    Tcp(String),
}

pub struct Config {
    source: Source,
    history: Option<String>,
    pub record: Option<String>,
    ui: Option<String>,
    journal: Option<String>,
    pub params: Params,
    pub stake: f64,
    pub payout: f64,
    pub balance: f64,
    pub digits: usize,
    pub heads_up: bool,
    pub quiet: bool,
    pub color: bool,
    pub asset: Option<String>,
    pub period_ms: i64,
    pub data_dir: Option<PathBuf>,
    pub martingale_steps: u32,
    pub martingale_mult: f64,
    pub min_payout: f64,
    /// Live feeds must report a payout before their pairs are traded.
    pub require_payout: bool,
    demo: bool,
    window: bool,
    sim: SimParams,
}

fn parse_tf(s: &str) -> Option<i64> {
    let s = s.trim().to_ascii_lowercase();
    let (num, mult) = match s.chars().last()? {
        's' => (&s[..s.len() - 1], 1_000),
        'm' => (&s[..s.len() - 1], 60_000),
        'h' => (&s[..s.len() - 1], 3_600_000),
        _ => (s.as_str(), 1_000),
    };
    let ms = num.parse::<i64>().ok()? * mult;
    (1_000..=86_400_000).contains(&ms).then_some(ms)
}

fn parse_args() -> Result<Config, String> {
    let mut args = std::env::args().skip(1).peekable();
    let source = match args.peek().map(String::as_str) {
        None => Source::App,
        Some(a) if a.starts_with('-') && a != "-h" && a != "--help" => Source::App,
        _ => match args.next().as_deref() {
            Some("app") => Source::App,
            Some("pocket") => Source::Pocket,
            Some("sim") => Source::Sim,
            Some("replay") => Source::Replay(args.next().ok_or("replay needs a file")?),
            Some("stdin") => Source::Stdin,
            Some("tcp") => Source::Tcp(args.next().ok_or("tcp needs an address")?),
            Some("-h" | "--help") => return Err(String::new()),
            Some(other) => return Err(format!("unknown source `{other}`")),
            None => unreachable!(),
        },
    };
    let bridged = matches!(source, Source::App | Source::Pocket);
    let source_is_app = source == Source::App;
    let mut cfg = Config {
        quiet: source == Source::App,
        source,
        history: None,
        record: None,
        ui: bridged.then(|| DEFAULT_UI_ADDR.to_owned()),
        journal: None,
        params: Params::default(),
        stake: 10.0,
        payout: 0.85,
        balance: 1000.0,
        digits: 5,
        heads_up: true,
        color: io::stdout().is_terminal(),
        asset: None,
        period_ms: 60_000,
        data_dir: bridged.then(default_data_dir),
        martingale_steps: 4,
        martingale_mult: 2.0,
        min_payout: 0.85,
        require_payout: bridged,
        demo: source_is_app,
        window: true,
        sim: SimParams {
            seed: 1,
            price: 1.085,
            prefill: 0,
            minutes: 600,
            step_ms: 250,
            vol: 0.0002,
            revert: 0.0,
            realtime: false,
        },
    };

    while let Some(flag) = args.next() {
        let mut value = |name: &str| args.next().ok_or(format!("{name} needs a value"));
        macro_rules! num {
            ($field:expr) => {{
                let v = value(&flag)?;
                $field = v.parse().map_err(|_| format!("bad value `{v}` for {flag}"))?;
            }};
        }
        match flag.as_str() {
            "--asset" => cfg.asset = Some(value(&flag)?),
            "--tf" => {
                let v = value(&flag)?;
                cfg.period_ms = parse_tf(&v).ok_or(format!("bad timeframe `{v}`, try 15s, 1m or 5m"))?;
            }
            "--history" => cfg.history = Some(value(&flag)?),
            "--record" => cfg.record = Some(value(&flag)?),
            "--ui" => cfg.ui = cfg.ui.take().or(Some(DEFAULT_UI_ADDR.into())),
            "--ui-addr" => cfg.ui = Some(value(&flag)?),
            "--journal" => cfg.journal = Some(value(&flag)?),
            "--data-dir" => cfg.data_dir = Some(PathBuf::from(value(&flag)?)),
            "--stake" => num!(cfg.stake),
            "--payout" => {
                num!(cfg.payout);
                cfg.payout = as_fraction(cfg.payout);
            }
            "--min-payout" => {
                num!(cfg.min_payout);
                cfg.min_payout = as_fraction(cfg.min_payout);
            }
            "--martingale" => num!(cfg.martingale_steps),
            "--martingale-mult" => num!(cfg.martingale_mult),
            "--balance" => num!(cfg.balance),
            "--digits" => num!(cfg.digits),
            "--z" => num!(cfg.params.z),
            "--margin" => num!(cfg.params.margin),
            "--min-samples" => num!(cfg.params.min_samples),
            "--half-life" => num!(cfg.params.half_life),
            "--minutes" => num!(cfg.sim.minutes),
            "--seed" => num!(cfg.sim.seed),
            "--step-ms" => num!(cfg.sim.step_ms),
            "--price" => num!(cfg.sim.price),
            "--vol" => num!(cfg.sim.vol),
            "--revert" => num!(cfg.sim.revert),
            "--prefill" => num!(cfg.sim.prefill),
            "--no-heads-up" => cfg.heads_up = false,
            "--quiet" => cfg.quiet = true,
            "--realtime" => cfg.sim.realtime = true,
            "--no-color" => cfg.color = false,
            "--no-demo" => cfg.demo = false,
            "--demo" => cfg.demo = true,
            "--browser" => cfg.window = false,
            "-h" | "--help" => return Err(String::new()),
            _ => return Err(format!("unknown option `{flag}`")),
        }
    }
    Ok(cfg)
}

/// Accepts payouts as either 0.85 or 85.
fn as_fraction(x: f64) -> f64 {
    if x > 1.0 { x / 100.0 } else { x }
}

fn default_data_dir() -> PathBuf {
    let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from);
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/Daytrade Assist")
    } else if cfg!(windows) {
        std::env::var_os("APPDATA").map_or(home, PathBuf::from).join("Daytrade Assist")
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map_or_else(|| home.join(".local/share"), PathBuf::from)
            .join("daytrade-assist")
    }
}

const EXTENSION: [(&str, &str); 4] = [
    ("manifest.json", include_str!("../extension/manifest.json")),
    ("inject.js", include_str!("../extension/inject.js")),
    ("relay.js", include_str!("../extension/relay.js")),
    ("background.js", include_str!("../extension/background.js")),
];

/// Writes the browser bridge to a stable folder Chrome can load it from.
fn write_extension(data_dir: &Path) -> io::Result<PathBuf> {
    let dir = data_dir.join("pocket-option-bridge");
    fs::create_dir_all(&dir)?;
    for (name, body) in EXTENSION {
        fs::write(dir.join(name), body)?;
    }
    Ok(dir)
}

/// Sends each saved tick log to the engine as a backfill, trimming logs that
/// have grown well past what is kept in memory.
fn load_tick_logs(dir: &Path, registry: &Registry, tx: &SyncSender<Input>) -> io::Result<()> {
    let Ok(entries) = fs::read_dir(dir) else { return Ok(()) };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "csv") {
            continue;
        }
        let lines = fs::metadata(&path).map_or(0, |m| m.len() / 24);
        for (id, mut ticks) in read_backfill(BufReader::new(File::open(&path)?), None, registry) {
            if ticks.len() > MAX_TICKS {
                ticks.drain(..ticks.len() - MAX_TICKS);
            }
            if lines as usize > MAX_TICKS * 3 / 2 {
                let name = registry.name(id);
                let tmp = path.with_extension("tmp");
                let mut w = io::BufWriter::new(File::create(&tmp)?);
                for t in &ticks {
                    writeln!(w, "{name},{},{}", t.ts_ms, t.price)?;
                }
                w.flush()?;
                fs::rename(&tmp, dir.join(log_file_name(&name)))?;
            }
            let _ = tx.send(Input::Backfill { asset: id, ticks, save: false });
        }
    }
    Ok(())
}

const DEMOS: [(&str, f64, f64); 4] = [
    ("DEMO:EURUSD", 1.0850, 0.2),
    ("DEMO:GBPJPY", 191.50, 0.0),
    ("DEMO:AUDCAD", 0.9120, -0.15),
    ("DEMO:USDCHF", 0.8640, 0.3),
];

fn main() -> ExitCode {
    let cfg = match parse_args() {
        Ok(cfg) => cfg,
        Err(e) if e.is_empty() => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("error: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let cfg: &'static Config = Box::leak(Box::new(cfg));
    match run(cfg) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(cfg: &'static Config) -> io::Result<()> {
    #[cfg(windows)]
    if cfg.source == Source::App {
        hide_own_console();
    }
    let registry = Arc::new(Registry::default());
    let (tx, rx) = mpsc::sync_channel::<Input>(1 << 16);
    // Ticks without an asset name (sim, plain files, stdin, tcp) belong to this one.
    let default_asset = || {
        let name = cfg.asset.clone().unwrap_or_else(|| {
            if cfg.source == Source::Sim { "DEMO:EURUSD" } else { "EURUSD" }.to_owned()
        });
        registry.id(&name)
    };

    let mut extension_dir = None;
    if let Some(dir) = &cfg.data_dir {
        fs::create_dir_all(dir.join("ticks"))?;
        extension_dir = Some(write_extension(dir)?);
    }

    let mut session = Session::new(cfg, registry.clone())?;
    let mut url = None;
    if let Some(addr) = &cfg.ui {
        let journal = cfg.journal.clone().unwrap_or_else(|| {
            let dir = cfg.data_dir.clone().unwrap_or_else(default_data_dir);
            let _ = fs::create_dir_all(&dir);
            dir.join("journal.jsonl").to_string_lossy().into_owned()
        });
        let ui = Ui::start(
            UiConfig {
                addr: addr.clone(),
                journal: Some(journal.clone()),
                extension_dir: extension_dir.as_ref().map(|d| d.to_string_lossy().into_owned()),
                stake: cfg.stake,
                payout: cfg.payout,
                balance: cfg.balance,
                z: cfg.params.z,
                min_samples: cfg.params.min_samples,
            },
            registry.clone(),
            tx.clone(),
        );
        let ui = match ui {
            Ok(ui) => ui,
            Err(e) if e.kind() == io::ErrorKind::AddrInUse && cfg.source == Source::App => {
                // Most likely the app is already running: show that one.
                return show(cfg, format!("http://{addr}"));
            }
            Err(e) => return Err(io::Error::new(e.kind(), format!("UI on {addr}: {e}"))),
        };
        session.ui = Some(ui);
        url = Some(format!("http://{addr}"));
        eprintln!("UI at http://{addr}  (journal: {journal})");
        if let Some(dir) = &extension_dir {
            eprintln!("Pocket Option bridge extension: {}", dir.display());
        }
    }

    // Everything already known is learned before any live tick arrives.
    if let Some(dir) = &cfg.data_dir {
        load_tick_logs(&dir.join("ticks"), &registry, &tx)?;
    }
    if let Some(path) = &cfg.history {
        let file = File::open(path).map_err(|e| io::Error::new(e.kind(), format!("{path}: {e}")))?;
        for (asset, ticks) in read_backfill(BufReader::new(file), Some(default_asset()), &registry) {
            let _ = tx.send(Input::Backfill { asset, ticks, save: false });
        }
    }

    let mut finite = 0;
    match &cfg.source {
        Source::App | Source::Pocket => {
            if cfg.demo {
                for (i, (name, price, revert)) in DEMOS.into_iter().enumerate() {
                    let p = SimParams {
                        seed: 1000 + i as u64,
                        price,
                        prefill: 3 * 1440,
                        minutes: 10 * 365 * 1440,
                        step_ms: 1000,
                        vol: 0.0002,
                        revert,
                        realtime: true,
                    };
                    spawn_sim(p, registry.id(name), tx.clone(), false);
                }
            }
        }
        Source::Sim => {
            spawn_sim(cfg.sim, default_asset(), tx.clone(), true);
            finite = 1;
        }
        Source::Replay(path) => {
            let file = File::open(path).map_err(|e| io::Error::new(e.kind(), format!("{path}: {e}")))?;
            spawn_lines(BufReader::new(file), default_asset(), registry.clone(), tx.clone());
            finite = 1;
        }
        Source::Stdin => {
            spawn_lines(BufReader::new(io::stdin()), default_asset(), registry.clone(), tx.clone());
            finite = 1;
        }
        Source::Tcp(addr) => {
            let listener = TcpListener::bind(addr)?;
            eprintln!("listening on {addr}, waiting for a tick feed…");
            let default_asset = default_asset();
            let (registry, tx) = (registry.clone(), tx.clone());
            thread::spawn(move || {
                if let Ok((stream, peer)) = listener.accept() {
                    let _ = stream.set_nodelay(true);
                    eprintln!("feed connected from {peer}");
                    spawn_lines(BufReader::new(stream), default_asset, registry, tx);
                }
            });
            finite = 1;
        }
    }
    drop(tx);

    if cfg.source == Source::App {
        thread::Builder::new().name("engine".into()).spawn(move || {
            if let Err(e) = session.run(rx, 0) {
                eprintln!("engine stopped: {e}");
            }
        })?;
        return show(cfg, url.unwrap_or_default());
    }

    session.run(rx, finite)?;
    if session.ui.is_some() {
        eprintln!("feed finished; the UI stays up until you press Ctrl+C");
        loop {
            thread::park();
        }
    }
    Ok(())
}

/// Shows the UI in the app window, or the browser when built without one.
fn show(cfg: &Config, url: String) -> io::Result<()> {
    #[cfg(feature = "app")]
    if cfg.window {
        app::run_window(&url);
    }
    let _ = cfg;
    let mut opener = if cfg!(target_os = "macos") {
        std::process::Command::new("open")
    } else if cfg!(windows) {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", ""]);
        c
    } else {
        std::process::Command::new("xdg-open")
    };
    let _ = opener.arg(&url).spawn();
    eprintln!("opened {url}; press Ctrl+C to quit");
    loop {
        thread::park();
    }
}

/// Double-clicking the .exe gives the app a console window of its own; close it
/// unless a terminal it was started from shares it.
#[cfg(windows)]
fn hide_own_console() {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetConsoleProcessList(list: *mut u32, count: u32) -> u32;
        fn FreeConsole() -> i32;
    }
    let mut pids = [0u32; 2];
    // SAFETY: the buffer outlives the call and its length is passed alongside it.
    unsafe {
        if GetConsoleProcessList(pids.as_mut_ptr(), pids.len() as u32) == 1 {
            FreeConsole();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeframes() {
        assert_eq!(parse_tf("15s"), Some(15_000));
        assert_eq!(parse_tf("1m"), Some(60_000));
        assert_eq!(parse_tf("300"), Some(300_000));
        assert_eq!(parse_tf("1H"), Some(3_600_000));
        assert_eq!(parse_tf("0s"), None);
        assert_eq!(parse_tf("abc"), None);
    }
}
