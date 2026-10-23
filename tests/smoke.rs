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

    wait_for("the tick log", || {
        std::fs::read_to_string(dir.join("ticks").join("EURUSD_otc.csv")).is_ok_and(|s| s.lines().count() > 10_000)
    });
}
