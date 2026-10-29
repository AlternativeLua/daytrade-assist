//! Runs the real binary the way people and the browser bridge use it.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const BIN: &str = env!("CARGO_BIN_EXE_daytrade-assist");

struct Running(Child, PathBuf);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
        let _ = std::fs::remove_dir_all(&self.1);
    }
}

fn free_addr() -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().to_string()
}

fn request(addr: &str, method: &str, path: &str, origin: Option<&str>, body: &str) -> String {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let origin = origin.map_or(String::new(), |o| format!("Origin: {o}\r\n"));
    write!(
        s,
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\n{origin}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    out
}

/// The first server-sent event, which is always a full snapshot.
fn snapshot(addr: &str) -> String {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    write!(s, "GET /events HTTP/1.1\r\nHost: {addr}\r\n\r\n").unwrap();
    let mut reader = BufReader::new(s);
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).unwrap() == 0 {
            return String::new();
        }
        if let Some(data) = line.strip_prefix("data: ") {
            return data.to_owned();
        }
    }
}

fn wait_for(what: &str, mut check: impl FnMut() -> bool) {
    let start = Instant::now();
    while !check() {
        assert!(start.elapsed() < Duration::from_secs(20), "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn sim_backtest_prints_a_summary() {
    let out = Command::new(BIN)
        .args(["sim", "--minutes", "600", "--revert", "0.3", "--tf", "1m", "--quiet", "--no-color"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("summary") && text.contains("win rate"), "{text}");
}

#[test]
fn replay_reads_asset_lines_from_a_file() {
    let dir = std::env::temp_dir().join(format!("daytrade-replay-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("ticks.csv");
    let mut body = String::from("asset,timestamp,price\r\n");
    let mut p = 1.0;
    for i in 0..3600 {
        p += if i % 120 < 60 { 1e-5 } else { -1e-5 };
        body.push_str(&format!("EURUSD_otc,{},{p:.5}\r\n", 1_700_000_000 + i));
    }
    std::fs::write(&file, body).unwrap();
    let out = Command::new(BIN)
        .args(["replay", file.to_str().unwrap(), "--tf", "30s", "--quiet", "--no-color"])
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(out.status.success(), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stdout).contains("on 120 candles"), "{out:?}");
}

#[test]
fn bridge_ticks_reach_the_ui() {
    let addr = free_addr();
    let dir = std::env::temp_dir().join(format!("daytrade-smoke-{}", std::process::id()));
    let child = Command::new(BIN)
        .args(["pocket", "--quiet", "--no-color", "--ui-addr", &addr, "--data-dir"])
        .arg(&dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _running = Running(child, dir.clone());
    wait_for("the UI server", || TcpStream::connect(&addr).is_ok());

    assert!(request(&addr, "GET", "/", None, "").contains("<title>Daytrade Assist</title>"));
    assert!(dir.join("pocket-option-bridge").join("manifest.json").exists());
    assert!(dir.join("pocket-option-bridge-firefox").join("manifest.json").exists());
    assert!(std::fs::read(dir.join("pocket-option-bridge-firefox.xpi")).unwrap().starts_with(b"PK\x03\x04"));

    // What the extension sends: chart history first, then live ticks.
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs_f64();
    let mut history = String::from("history\n");
    for i in 0..3 * 3600 {
        let t = now - 3.0 * 3600.0 + i as f64;
        history.push_str(&format!("EURUSD_otc,{t:.3},{:.5}\n", 1.085 + (i % 97) as f64 * 1e-5));
    }
    let ext = Some("chrome-extension://smoke-test");
    assert!(request(&addr, "POST", "/ingest", ext, &history).starts_with("HTTP/1.1 200"));
    let live = format!("stream\nEURUSD_otc,{now:.3},1.08512\nGBPUSD_otc,{now:.3},1.25510\n");
    assert!(request(&addr, "POST", "/ingest", ext, &live).starts_with("HTTP/1.1 200"));

    // Other websites can't feed prices or change the selection.
    assert!(request(&addr, "POST", "/ingest", Some("https://example.com"), &live).starts_with("HTTP/1.1 404"));

    wait_for("EURUSD_otc to go live", || {
        let s = snapshot(&addr);
        s.contains(r#""asset":"EURUSD_otc""#) && s.contains(r#""phase":"live""#) && s.contains(r#""name":"GBPUSD_otc""#)
    });
    let s = snapshot(&addr);
    assert!(s.contains(r#""backtest":{"#), "{s}");

    let ui = Some(format!("http://{addr}"));
    let select = request(&addr, "POST", "/api/select", ui.as_deref(), "asset=EURUSD_otc&tf=15");
    assert!(select.starts_with("HTTP/1.1 200"), "{select}");
    wait_for("the 15s timeframe", || snapshot(&addr).contains(r#""tf":15,"#));

    // Payouts from the broker's asset table decide whether the pair is traded.
    let assets = "assets\nEURUSD_otc,92,1,currency,EUR/USD OTC\nGBPUSD_otc,70,1,currency,GBP/USD OTC\nAAPL_otc,95,1,stock,Apple OTC\n";
    assert!(request(&addr, "POST", "/ingest", ext, assets).starts_with("HTTP/1.1 200"));
    wait_for("EURUSD_otc to be tradeable at 92%", || {
        let s = snapshot(&addr);
        s.contains(r#""payout":0.92,"open":true,"canTrade":true"#) && s.contains(r#""asset":"GBPUSD_otc""#) && !s.contains("AAPL_otc")
    });
    let lowered = "assets\nEURUSD_otc,80,1,currency,EUR/USD OTC\n";
    assert!(request(&addr, "POST", "/ingest", ext, lowered).starts_with("HTTP/1.1 200"));
    wait_for("trading to stop at 80%", || snapshot(&addr).contains(r#""payout":0.8,"open":true,"canTrade":false"#));
    let settings = request(&addr, "POST", "/api/settings", ui.as_deref(), "martingale=0&z=1.5");
    assert!(settings.starts_with("HTTP/1.1 200"), "{settings}");
    wait_for("the new settings", || snapshot(&addr).contains(r#""steps":0,"mult":2.1,"z":1.5"#));

    // A trade frequency level sets the rules and the track record check together, and is remembered.
    let freq = request(&addr, "POST", "/api/settings", ui.as_deref(), "frequency=active");
    assert!(freq.starts_with("HTTP/1.1 200"), "{freq}");
    wait_for("the active level", || snapshot(&addr).contains(r#""z":0,"trackZ":0,"trackMin":20,"frequency":"active""#));
    assert_eq!(std::fs::read_to_string(dir.join("trade-frequency.txt")).unwrap(), "active");

    wait_for("the tick log", || {
        std::fs::read_to_string(dir.join("ticks").join("EURUSD_otc.csv")).is_ok_and(|s| s.lines().count() > 10_000)
    });
}

#[test]
fn autotrade_hands_orders_to_the_bridge() {
    let addr = free_addr();
    let dir = std::env::temp_dir().join(format!("daytrade-orders-{}", std::process::id()));
    let child = Command::new(BIN)
        .args(["pocket", "--quiet", "--no-color", "--ui-addr", &addr, "--data-dir"])
        .arg(&dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _running = Running(child, dir.clone());
    wait_for("the UI server", || TcpStream::connect(&addr).is_ok());
    let ext = Some("chrome-extension://smoke-test");
    let ui = format!("http://{addr}");
    let post = |path: &str, origin: Option<&str>, body: &str| {
        let r = request(&addr, "POST", path, origin, body);
        assert!(r.starts_with("HTTP/1.1 200"), "{path}: {r}");
    };

    // One-second candles that alternate colour every candle: "fade last-candle" is always right.
    let start = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64 - 600;
    let candle = |k: i64| {
        let close = if k % 2 == 0 { 1.0852 } else { 1.0848 };
        format!("EURUSD_otc,{k}.0,1.08500\nEURUSD_otc,{k}.5,{close:.5}\n")
    };
    post("/ingest", ext, "assets\nEURUSD_otc,92,1,currency,EUR/USD OTC\n");
    post("/ingest", ext, &format!("history\n{}", (start..start + 590).map(candle).collect::<String>()));
    post("/ingest", ext, &format!("stream\n{}", candle(start + 590)));
    wait_for("EURUSD_otc to go live", || snapshot(&addr).contains(r#""asset":"EURUSD_otc""#));
    post("/api/select", Some(&ui), "asset=EURUSD_otc&tf=1");
    post("/api/settings", Some(&ui), "mode=demo&stake=5&mult=2.1");
    wait_for("1s candles with demo trading on", || {
        let s = snapshot(&addr);
        s.contains(r#""tf":1,"#) && s.contains(r#""phase":"live""#) && s.contains(r#""mode":"demo""#) && s.contains(r#""stake":5,"#) && s.contains(r#""mult":2.1"#)
    });

    // The next candle close produces a signal, a paper trade and an order for the bridge.
    post("/ingest", ext, &format!("stream\n{}{}", candle(start + 591), candle(start + 592)));
    let orders = request(&addr, "GET", "/bridge/orders", ext, "");
    assert!(orders.starts_with("HTTP/1.1 200"), "{orders}");
    let line = orders.lines().last().unwrap().to_owned();
    let f: Vec<&str> = line.split(',').collect();
    assert_eq!((f[0], f[2], f[4], f[5], f[6]), ("order", "EURUSD_otc", "5", "1", "demo"), "{line}");
    assert!(f[3] == "call" || f[3] == "put", "{line}");

    // What the bridge reports back from Pocket Option.
    let req = f[1];
    post("/ingest", ext, &format!("orders\nopened,{req},deal-1,1.085\nclosed,{req},deal-1,4.6,1.0852\n"));
    wait_for("the settled order", || {
        let s = snapshot(&addr);
        s.contains(&format!(r#""requestId":"{req}""#)) && s.contains(r#""status":"won""#) && s.contains(r#""profit":4.6"#)
    });

    // A website can't pull orders.
    assert!(request(&addr, "GET", "/bridge/orders", Some("https://example.com"), "").starts_with("HTTP/1.1 404"));

    // Real money only with the confirmation text the UI sends after its two prompts.
    let refused = request(&addr, "POST", "/api/settings", Some(&ui), "mode=real");
    assert!(refused.starts_with("HTTP/1.1 403"), "{refused}");
    post(
        "/api/settings",
        Some(&ui),
        "mode=real&lossLimit=25&confirm=I+accept+the+risk+of+losing+real+money",
    );
    wait_for("real mode", || {
        let s = snapshot(&addr);
        s.contains(r#""mode":"real""#) && s.contains(r#""lossLimit":25"#)
    });

    // Paused: candles keep closing and being logged, but nothing is traded.
    post("/api/settings", Some(&ui), "paused=1&mode=off");
    wait_for("pause", || snapshot(&addr).contains(r#""mode":"off","paused":true"#));
    post("/ingest", ext, &format!("stream\n{}{}{}", candle(start + 593), candle(start + 594), candle(start + 595)));
    let log_dir = dir.join("logs");
    wait_for("the decision log", || {
        std::fs::read_dir(&log_dir).ok().and_then(|mut d| d.next()).and_then(|e| e.ok()).is_some_and(|e| {
            let log = std::fs::read_to_string(e.path()).unwrap_or_default();
            log.contains(r#""type":"candle""#)
                && log.contains(r#""type":"order""#)
                && log.contains(r#""bases":["#)
                && log.lines().any(|l| l.contains(r#""traded":false"#) && l.contains(r#""paused":true"#))
        })
    });
}

#[test]
fn training_is_saved_and_picked_up_next_time() {
    let addr = free_addr();
    let dir = std::env::temp_dir().join(format!("daytrade-training-{}", std::process::id()));
    let start = |dir: &PathBuf| {
        Command::new(BIN)
            .args(["pocket", "--quiet", "--no-color", "--ui-addr", &addr, "--data-dir"])
            .arg(dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    };
    let ext = Some("chrome-extension://smoke-test");
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs_f64();

    let mut first = start(&dir);
    wait_for("the UI server", || TcpStream::connect(&addr).is_ok());
    let mut history = String::from("history\n");
    for i in 0..4 * 3600 {
        let t = now - 4.0 * 3600.0 + i as f64;
        history.push_str(&format!("EURUSD_otc,{t:.3},{:.5}\n", 1.085 + (i % 97) as f64 * 1e-5));
    }
    assert!(request(&addr, "POST", "/ingest", ext, &history).starts_with("HTTP/1.1 200"));
    let training = dir.join("training.txt");
    wait_for("training.txt", || {
        std::fs::read_to_string(&training).is_ok_and(|s| s.lines().filter(|l| l.starts_with("EURUSD_otc\t")).count() == 6)
    });
    let _ = first.kill();
    let _ = first.wait();

    // The tick logs are gone; the saved training alone has to carry over.
    std::fs::remove_dir_all(dir.join("ticks")).unwrap();
    let second = start(&dir);
    let _running = Running(second, dir.clone());
    wait_for("the UI server again", || TcpStream::connect(&addr).is_ok());
    wait_for("the saved 1m training", || {
        let t = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs_f64();
        let _ = request(&addr, "POST", "/ingest", ext, &format!("stream\nEURUSD_otc,{t:.3},1.08512\n"));
        let s = snapshot(&addr);
        // About 240 one-minute candles were learned in the first run.
        s.split(r#""asset":"EURUSD_otc","tf":60,"candles":"#)
            .nth(1)
            .and_then(|r| r.split(',').next())
            .and_then(|n| n.parse::<u64>().ok())
            .is_some_and(|n| n >= 230)
    });
}

#[test]
fn auto_mode_trades_another_pair() {
    let addr = free_addr();
    let dir = std::env::temp_dir().join(format!("daytrade-auto-{}", std::process::id()));
    let child = Command::new(BIN)
        .args(["pocket", "--quiet", "--no-color", "--ui-addr", &addr, "--data-dir"])
        .arg(&dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _running = Running(child, dir.clone());
    wait_for("the UI server", || TcpStream::connect(&addr).is_ok());
    let ext = Some("chrome-extension://smoke-test");
    let ui = format!("http://{addr}");
    let post = |path: &str, origin: Option<&str>, body: &str| {
        let r = request(&addr, "POST", path, origin, body);
        assert!(r.starts_with("HTTP/1.1 200"), "{path}: {r}");
    };

    // Both pairs alternate colour every one-second candle.
    let start = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64 - 600;
    let candle = |asset: &str, k: i64| {
        let close = if k % 2 == 0 { 1.0852 } else { 1.0848 };
        format!("{asset},{k}.0,1.08500\n{asset},{k}.5,{close:.5}\n")
    };
    let history = |asset: &str| format!("history\n{}", (start..start + 590).map(|k| candle(asset, k)).collect::<String>());
    post("/ingest", ext, "assets\nEURUSD_otc,92,1,currency,EUR/USD OTC\nGBPUSD_otc,90,1,currency,GBP/USD OTC\n");
    post("/ingest", ext, &history("EURUSD_otc"));
    post("/ingest", ext, &history("GBPUSD_otc"));
    post("/ingest", ext, &format!("stream\n{}", candle("EURUSD_otc", start + 590)));
    wait_for("EURUSD_otc to go live", || snapshot(&addr).contains(r#""asset":"EURUSD_otc""#));
    post("/api/select", Some(&ui), "asset=EURUSD_otc&tf=1");
    post("/api/settings", Some(&ui), "mode=demo&stakePct=2&auto=1&awake=system");
    wait_for("auto mode", || {
        let s = snapshot(&addr);
        s.contains(r#""tf":1,"#) && s.contains(r#""phase":"live""#) && s.contains(r#""mode":"demo""#) && s.contains(r#""auto":true"#) && s.contains(r#""stakePct":2,"baseAmount":20,"#)
    });
    assert!(snapshot(&addr).contains(r#""awake":"system","awakeOk":"#));

    // Only GBP/USD ticks: auto mode trades it although EUR/USD is the pair on screen, staking 2% of 1000.
    post("/ingest", ext, &format!("stream\n{}", candle("GBPUSD_otc", start + 590)));
    let orders = request(&addr, "GET", "/bridge/orders", ext, "");
    assert!(orders.starts_with("HTTP/1.1 200"), "{orders}");
    let line = orders.lines().last().unwrap().to_owned();
    let f: Vec<&str> = line.split(',').collect();
    assert_eq!((f[0], f[2], f[4], f[5], f[6]), ("order", "GBPUSD_otc", "20", "1", "demo"), "{line}");
    wait_for("the open auto trade", || snapshot(&addr).contains(r#""open":{"asset":"GBPUSD_otc","tf":1,"#));

    // Its next candle settles it, recorded against GBP/USD at its own 90% payout.
    post("/ingest", ext, &format!("stream\n{}", candle("GBPUSD_otc", start + 591)));
    wait_for("the settled auto trade", || {
        let s = snapshot(&addr);
        s.contains(r#""asset":"GBPUSD_otc","tf":1,"source":"feed""#) && s.contains(r#""payout":0.9,"#)
    });
}

#[test]
fn follows_a_streaming_pair_when_the_selected_one_stops() {
    let addr = free_addr();
    let dir = std::env::temp_dir().join(format!("daytrade-follow-{}", std::process::id()));
    let child = Command::new(BIN)
        .args(["pocket", "--quiet", "--no-color", "--ui-addr", &addr, "--data-dir"])
        .arg(&dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _running = Running(child, dir.clone());
    wait_for("the UI server", || TcpStream::connect(&addr).is_ok());
    let ext = Some("chrome-extension://smoke-test");
    // Pocket Option's clock runs two hours ahead of this one; that mustn't matter.
    let ahead = || SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs_f64() + 7200.0;
    let tick = |asset: &str| {
        let r = request(&addr, "POST", "/ingest", ext, &format!("stream\n{asset},{:.3},1.08512\n", ahead()));
        assert!(r.starts_with("HTTP/1.1 200"), "{r}");
    };
    tick("EURRUB_otc");
    wait_for("EURRUB_otc to be selected", || snapshot(&addr).contains(r#""asset":"EURRUB_otc","tf":60,"assets""#));

    // Its chart is closed in Pocket Option; another pair keeps streaming.
    let start = Instant::now();
    loop {
        tick("CADJPY_otc");
        let s = snapshot(&addr);
        if s.contains(r#""asset":"CADJPY_otc","tf":60,"assets""#) {
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(45), "never moved off the stopped pair");
        thread::sleep(Duration::from_millis(1000));
    }
    assert!(start.elapsed() >= Duration::from_secs(25), "switched before the pair had stopped");
}

#[test]
fn balance_can_be_set_or_follow_pocket_option() {
    let addr = free_addr();
    let dir = std::env::temp_dir().join(format!("daytrade-balance-{}", std::process::id()));
    let child = Command::new(BIN)
        .args(["pocket", "--quiet", "--no-color", "--ui-addr", &addr, "--data-dir"])
        .arg(&dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _running = Running(child, dir.clone());
    wait_for("the UI server", || TcpStream::connect(&addr).is_ok());
    let ext = Some("chrome-extension://smoke-test");
    let ui = format!("http://{addr}");
    let post = |path: &str, origin: Option<&str>, body: &str| {
        let r = request(&addr, "POST", path, origin, body);
        assert!(r.starts_with("HTTP/1.1 200"), "{path}: {r}");
    };
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs_f64();
    post("/ingest", ext, &format!("stream\nEURUSD_otc,{now:.3},1.08512\n"));
    wait_for("a selected pair", || snapshot(&addr).contains(r#""asset":"EURUSD_otc","tf":60,"assets""#));

    // What the bridge reports from the logged-in Pocket Option tab.
    post("/ingest", ext, "balance\n1,52340.5\n0,87.25\n");
    post("/api/settings", Some(&ui), "sync=1&stakePct=1");
    wait_for("the synced demo balance", || {
        let s = snapshot(&addr);
        s.contains(r#""balance":52340.5,"startBalance":52340.5,"sync":true,"poDemo":52340.5,"poReal":87.25"#) && s.contains(r#""baseAmount":523.4,"#)
    });
    post("/ingest", ext, "balance\n1,52863.9\n");
    wait_for("the balance to follow", || snapshot(&addr).contains(r#""balance":52863.9,"startBalance":52340.5,"sync":true"#));

    assert_eq!(std::fs::read_to_string(dir.join("pocket-option-balance.txt")).unwrap(), "87.25\n52863.9\n");

    // Typing a balance goes back to a paper account at that amount.
    post("/api/settings", Some(&ui), "balance=5000");
    wait_for("the paper balance", || snapshot(&addr).contains(r#""balance":5000,"startBalance":5000,"sync":false"#));
}

#[test]
fn auto_mode_keeps_to_its_pair_list_and_stays_on_a_losing_pair() {
    let addr = free_addr();
    let dir = std::env::temp_dir().join(format!("daytrade-hold-{}", std::process::id()));
    let child = Command::new(BIN)
        .args(["pocket", "--quiet", "--no-color", "--ui-addr", &addr, "--data-dir"])
        .arg(&dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _running = Running(child, dir.clone());
    wait_for("the UI server", || TcpStream::connect(&addr).is_ok());
    let ext = Some("chrome-extension://smoke-test");
    let ui = format!("http://{addr}");
    let post = |path: &str, origin: Option<&str>, body: &str| {
        let r = request(&addr, "POST", path, origin, body);
        assert!(r.starts_with("HTTP/1.1 200"), "{path}: {r}");
    };
    let orders = || {
        let r = request(&addr, "GET", "/bridge/orders", ext, "");
        assert!(r.starts_with("HTTP/1.1 200"), "{r}");
        r.split("\r\n\r\n").nth(1).unwrap().lines().map(str::to_owned).collect::<Vec<_>>()
    };

    // Both pairs alternate colour every one-second candle; `flip` breaks the pattern once.
    let start = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64 - 600;
    let close = |k: i64| if k % 2 == 0 { 1.0852 } else { 1.0848 };
    let candle = |asset: &str, k: i64| format!("{asset},{k}.0,1.08500\n{asset},{k}.5,{:.5}\n", close(k));
    let flip = |asset: &str, k: i64| format!("{asset},{k}.0,1.08500\n{asset},{k}.5,{:.5}\n", close(k - 1));
    let history = |asset: &str| format!("history\n{}", (start..start + 590).map(|k| candle(asset, k)).collect::<String>());
    post("/ingest", ext, "assets\nEURUSD_otc,92,1,currency,EUR/USD OTC\nGBPUSD_otc,92,1,currency,GBP/USD OTC\n");
    post("/ingest", ext, &history("EURUSD_otc"));
    post("/ingest", ext, &history("GBPUSD_otc"));
    post("/ingest", ext, &format!("stream\nEURUSD_otc,{}.0,1.08500\n", start + 589));
    wait_for("EURUSD_otc to go live", || snapshot(&addr).contains(r#""asset":"EURUSD_otc","tf""#));
    post("/api/select", Some(&ui), "asset=EURUSD_otc&tf=1");
    post("/api/settings", Some(&ui), "mode=demo&stake=5&auto=1&autoList=GBPUSD_otc");
    wait_for("auto mode limited to GBP/USD", || {
        let s = snapshot(&addr);
        s.contains(r#""tf":1,"#) && s.contains(r#""phase":"live""#) && s.contains(r#""autoList":["GBPUSD_otc"]"#)
    });

    // EUR/USD signals first but isn't on the list; GBP/USD is traded, and its candle breaks the pattern.
    post("/ingest", ext, &format!("stream\n{}", candle("EURUSD_otc", start + 590)));
    post("/ingest", ext, &format!("stream\n{}", flip("GBPUSD_otc", start + 590)));
    let first = orders();
    assert!(first.iter().all(|l| l.contains(",GBPUSD_otc,")) && first.len() == 1, "{first:?}");

    // All pairs allowed again. Paused, the loss settles without a new trade.
    post("/api/settings", Some(&ui), "autoList=&paused=1");
    post("/ingest", ext, &format!("stream\nGBPUSD_otc,{}.0,1.08500\n", start + 591));
    wait_for("the loss to be held on GBP/USD", || {
        let s = snapshot(&addr);
        s.contains(r#""heldOn":"GBPUSD_otc","autoList":[]"#) && s.contains(r#""outcome":"LOSS""#)
    });

    // EUR/USD signals first again, but the multiplied trade waits for GBP/USD.
    post("/api/settings", Some(&ui), "paused=0");
    wait_for("unpaused", || snapshot(&addr).contains(r#""paused":false"#));
    post("/ingest", ext, &format!("stream\nEURUSD_otc,{}.0,1.08500\n", start + 591));
    post("/ingest", ext, &format!("stream\nGBPUSD_otc,{}.5,{:.5}\nGBPUSD_otc,{}.0,1.08500\n", start + 591, close(start + 591), start + 592));
    let next = orders();
    assert!(!next.is_empty() && next.iter().all(|l| l.contains(",GBPUSD_otc,") && l.contains(",10.5,")), "{next:?}");
}
