# Daytrade Assist

A desktop app that gives a CALL/PUT signal for the next candle of an OTC currency pair, reading live prices from Pocket Option. It keeps a binary-option-style paper account (fixed stake, fixed payout, one-candle expiry) so you can see whether the signals beat the break-even rate. It is for paper trading only and never places trades.

## Run the app

**Windows 10/11:** download `DaytradeAssist-windows-x64.zip` from [Releases](../../releases), unzip it and run `Daytrade Assist.exe`. The app is unsigned, so SmartScreen may warn you: click **More info → Run anyway**. The window uses the WebView2 runtime that ships with Windows 10 and 11.

**macOS:** download `DaytradeAssist-macos-arm64.zip` from Releases, or build it yourself:

```
./scripts/bundle-macos.sh          # builds "dist/Daytrade Assist.app"
open "dist/Daytrade Assist.app"    # or drag it into /Applications
```

**From source (any OS):** `cargo run --release` with no arguments opens the same app window.

- **Pair:** the dropdown at the top left lists Pocket Option pairs that are live, saved pairs with no live prices, and four simulated `DEMO` pairs so you can try the app before connecting.
- **Timeframe:** 15s, 30s, 1m, 2m, 5m or 15m. This sets both the candle size and the trade expiry. Switching retrains from that pair's saved ticks in well under a second.
- **History tab:** every live trade, across sessions. You can filter by pair, timeframe, feed and period.

The app keeps its data in `~/Library/Application Support/Daytrade Assist` on macOS, or `%APPDATA%\Daytrade Assist` on Windows: `ticks/` (one log per pair, used for training on the next start), `journal.jsonl` (trade history) and `pocket-option-bridge/` (the browser extension).

## Connect Pocket Option

Pocket Option has no public API. Instead, a small Chrome extension reads the price stream that your logged-in Pocket Option tab already receives, and forwards it to the app on `127.0.0.1:8787`. It never sees your password, never sends anything to Pocket Option, and never places trades.

1. In Chrome, open `chrome://extensions` and turn on **Developer mode**.
2. Click **Load unpacked** and choose the `pocket-option-bridge` folder. The app's **Connect Pocket Option** button has a **Show folder** shortcut.
3. Open Pocket Option (the demo account is fine) and pick an OTC pair. The pair shows up in the app within a few seconds, together with the chart history Pocket Option loads, which the app trains on straight away.

Each pair you open in Pocket Option is added to the list. The extension's badge shows **ON** while prices are reaching the app.

The extension reads socket.io `updateStream` frames (`[["EURUSD_otc", <unix seconds>, <price>], …]`) and `history` arrays. These formats come from open-source Pocket Option clients and may change without notice. Pocket Option doesn't support third-party tools, so check its terms before using one.

## How it decides

OTC prices come from each broker's own price generator, and the generators aren't public, so no fixed indicator recipe can be trusted to carry over. The bot learns from the feed it is given instead:

1. Each closed candle feeds 12 base signals: last candle colour, 3-candle streak, the move in the last sixth of the candle, RSI(2), RSI(14), Bollinger ±2σ and ±1σ, stochastic(5,3), EMA 9/21 trend, wick rejection, trend pullback, and where the close sits in the candle's range.
2. Each base signal is a pair of rules: **follow** (trade in its direction) and **fade** (trade against it), 24 rules in total. The feed decides which direction works.
3. Every vote from every rule is scored against the next candle, whether it was traded or not. Recent votes count more (`--half-life`, 400 votes by default).
4. A rule is only traded while the lower confidence bound of its win rate (Wilson, `--z`) is above the payout's break-even rate. With 85% payout that rate is 54.1%.

Each pair and timeframe is learned separately. The decision takes about 250 ns. Logging, the UI and the bridge all run on other threads, after the decision is made.

### Calibration (simulated, 6 seeds × 20,000 candles, z = 1)

| feed | candles traded | win rate | per $1 staked |
|---|---|---|---|
| random walk (no edge) | 0.4% | 44% | −0.18 |
| weak reversal (true ≈53%, below break-even) | 10% | 53.4% | −0.01 |
| reversal 0.2 (true ≈56%) | 80% | 56.8% | +0.051 |
| reversal 0.3 (true ≈60%) | 98% | 59.9% | +0.107 |
| momentum 0.2 | 81% | 56.9% | +0.052 |

On a feed with a pattern, it trades most candles. On noise it mostly holds, and the few noise trades it does take lose. Lower `--z` (e.g. 0 or 0.5) for more signals at the cost of more noise trades.

Whether a real OTC feed has any pattern is an open question, and the answer can differ by broker, pair, timeframe and day. Paper trade for a few thousand candles before trusting anything, and don't read much into fewer than a few hundred trades.

## Command line

```
daytrade-assist pocket --tf 30s                     # bridge + web UI + terminal output
daytrade-assist sim --revert 0.2 --tf 1m            # instant backtest on simulated ticks
daytrade-assist replay ticks.csv --asset EURUSD_otc --tf 15s
daytrade-assist tcp 127.0.0.1:7000 --ui             # push `asset,timestamp,price` lines from anything
daytrade-assist --help
```

Tick lines are `price`, `timestamp,price` or `asset,timestamp,price` (unix seconds or ms). `cargo build --release --no-default-features` builds without the native window; the app then opens in your browser.

`cargo test` runs the unit tests plus end-to-end tests that start the real binary and talk to it the way the browser bridge and the UI do. CI runs them on Windows, macOS and Linux.
