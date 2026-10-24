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

The app keeps its data in `~/Library/Application Support/Daytrade Assist` on macOS, or `%APPDATA%\Daytrade Assist` on Windows:
- `ticks/`: one log per pair, used for training on the next start.
- `journal.jsonl`: trade history.
- `logs/`: decision logs.
- The browser extension: `pocket-option-bridge/` for Chromium browsers, and `pocket-option-bridge-firefox/` plus `pocket-option-bridge-firefox.xpi` for Firefox-based browsers.

Replacing the app with a newer version keeps all of it.

## Connect Pocket Option

Pocket Option has no public API. Instead, a small browser extension reads the price stream that your logged-in Pocket Option tab already receives, and forwards it to the app on `127.0.0.1:8787`. It never sees your password. It only sends anything to Pocket Option if you turn on demo-account trading (see below).

The app's **Connect Pocket Option** button walks you through these steps, with **Show folder** shortcuts.

**Chrome, Edge, Brave, Opera:**
1. Open `chrome://extensions` (or `edge://extensions`) and turn on **Developer mode**.
2. Click **Load unpacked** and choose the `pocket-option-bridge` folder in the data folder.

**Firefox, Zen, LibreWolf, Floorp, Waterfox** (Firefox 128 or newer):
- *To keep it installed* (Zen, LibreWolf, Floorp, Waterfox, Firefox Developer Edition or Nightly):
  1. In `about:config`, set `xpinstall.signatures.required` to `false`.
  2. In `about:addons`, click ⚙ → **Install Add-on From File…** and choose `pocket-option-bridge-firefox.xpi`. Allow the permissions it asks for.
- *Regular Firefox* only keeps Mozilla-signed add-ons. Load it for the session instead: `about:debugging#/runtime/this-firefox` → **Load Temporary Add-on…** → `pocket-option-bridge-firefox/manifest.json`. It's removed when Firefox restarts.
- If no prices arrive, go to `about:addons` → *Daytrade Assist bridge* → **Permissions** and turn everything on.

**Then:** open Pocket Option (the demo account is fine) and pick an OTC pair. The pair shows up in the app within a few seconds, together with the chart history Pocket Option loads, which the app trains on straight away.

Each pair you open in Pocket Option is added to the list. The extension's badge shows **ON** while prices are reaching the app.

The extension reads socket.io `updateStream` frames (`[["EURUSD_otc", <unix seconds>, <price>], …]`) and `history` arrays. These formats come from open-source Pocket Option clients and may change without notice. Pocket Option doesn't support third-party tools, so check its terms before using one.

## Placing trades on Pocket Option

By default the app only shows signals and keeps its own paper account. The **Pocket Option** setting in the account card changes that:

- **Place trades on demo account:** every live paper trade on a Pocket Option pair becomes an order on your demo account, with the same direction and amount (including the multiplier after a loss). Its expiry is one candle of the selected timeframe.
- **Place trades with REAL money:** the same, but on your real-money account. Turning it on takes two confirmations:
  1. Read the risks, set a loss limit and tick that you understand.
  2. Type `REAL`.

  The server also refuses the switch unless it carries the confirmation, so a stray click or another web page can't turn it on.

  Real-money trading turns itself off when you restart the app, when you press Pause, or when trades placed with real money have lost the loss limit (`--loss-limit`, default 100). That limit is measured on the matching paper trades. While it's on, a red banner stays at the top of the window.

How orders are placed:
- The extension sends Pocket Option's own `openOrder` message, the same one the site's Buy buttons send, through your open Pocket Option tab.
- It only sends an order on a connection whose login matches the order's account: demo orders on a demo login, real orders on a real-money login. A mismatch is refused and shown in the app, never guessed.
- An order the extension hasn't picked up within 3 seconds is dropped, never placed late.

**Pause / Continue** (top right) stops all new trades: paper, demo and real. Signals and learning keep running.

The **Pocket Option trades** tab shows each order's account, status (sent, open, won, lost or rejected) and the profit Pocket Option reports. These can differ slightly from the paper trades because of entry timing.

## Decision logs

Each live candle is appended to `logs/decisions-YYYY-MM-DD.jsonl` in the data folder. Each line records the candle's prices, all 12 base signals, the chosen rule and its record, whether it traded, and the payout, mode and stake settings at the time. Settlements, Pocket Option order results and every settings change are logged there too. **Open decision logs folder** in the account card shows the folder. These logs are what's needed to review and refine the strategy later: share the folder, or point Claude at it.

## Payouts and stake sizing

**Payout filter.** The bridge also reads Pocket Option's asset table, so the app knows each pair's live payout and whether it's open. Only pairs paying **85% or more** are traded (`--min-payout`). Below that, signals still show but no trade is placed. The **Payouts** tab lists every OTC currency pair by current payout. Pocket Option only streams prices for pairs you open, so open a high-payout pair there to trade it. Each trade is priced at the payout it opened with.

**Amount and multiplier after a loss** (`--stake`, `--martingale` default 4, `--martingale-mult` default 2.1). After a loss the next amount is multiplied by 2.1, up to 4 losses in a row (10 → 21 → 44.10 → 92.61 → 194.48). A win, or a fifth loss in a row, goes back to the base amount. You can change all three in the app's account card, or set *After a loss* to keep the same amount.

At an 85% payout, 2.1× recovers the earlier losses if the win comes within 3 losses. Five losses in a row cost 36.2× the base amount.

What multiplying does and doesn't do (6 simulated runs × 20,000 candles each, $10 base stake, ×2):

| feed | payout | doubling | win rate | return per $ staked | worst drawdown | runs that lost 90%+ of $1,000 |
|---|---|---|---|---|---|---|
| no edge | 92% | off | 49.6% | −4.9% | 796 | 0/6 |
| no edge | 92% | ×2, 4 times | 49.6% | −4.3% | 2,402 | 3/6 |
| weak edge | 85% | off | 55.4% | +2.5% | 624 | 0/6 |
| weak edge | 85% | ×2, 4 times | 55.4% | +3.3% | 3,318 | 0/6 |
| real edge | 92% | off | 56.5% | +8.5% | 364 | 0/6 |
| real edge | 92% | ×2, 4 times | 56.5% | +7.8% | 3,098 | 0/6 |

Doubling never changes the win rate, and it barely changes the return per dollar staked. What it does is stake more after losses, which multiplies the drawdowns about 5 times. At an 85% payout, a win only recovers the earlier losses if it comes within 2 losses. Five losses in a row cost 31× the base stake.

**Selectivity** (`--z`, *Selectivity* in the app) trades off how often it signals against how sure it is:

| weak-edge feed | candles traded | win rate |
|---|---|---|
| Loose | 86% | 55.0% |
| Normal (default) | 51% | 55.4% |
| Strict | 29% | 56.0% |
| Very strict | 12% | 56.5% |

## How it decides

OTC prices come from each broker's own price generator, and the generators aren't public, so no fixed indicator recipe can be trusted to carry over. The bot learns from the feed it is given instead:

1. Each closed candle feeds 12 base signals: last candle colour, 3-candle streak, the move in the last sixth of the candle, RSI(2), RSI(14), Bollinger ±2σ and ±1σ, stochastic(5,3), EMA 9/21 trend, wick rejection, trend pullback, and where the close sits in the candle's range.
2. Each base signal is a pair of rules: **follow** (trade in its direction) and **fade** (trade against it), 24 rules in total. The feed decides which direction works.
3. Every vote from every rule is scored against the next candle, whether it was traded or not. Recent votes count more (`--half-life`, 400 votes by default).
4. A rule is only traded while the lower confidence bound of its win rate (Wilson, `--z`) is above the pair's break-even rate at its current payout (54.1% at 85%, 52.1% at 92%).

Each pair and timeframe is learned separately. The decision takes about 250 ns. Logging, the UI and the bridge all run on other threads, after the decision is made.

### Calibration (simulated, 6 seeds × 20,000 candles, z = 1, fixed stake)

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
