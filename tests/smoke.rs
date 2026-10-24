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
