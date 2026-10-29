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
- `training.txt`: what every pair has learned on every timeframe, carried over between sessions.
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

## Training on several pairs

Every pair that sends prices learns all six timeframes at once, whether or not it's the pair on screen. Pocket Option only streams the pairs you have open, so open several (one per tab, or Pocket Option's multi-chart layout) to train them side by side.

The **Training** tab shows each pair and timeframe: candles learned, and how much of the bot's own track record there is proven (see *How it decides*). Green cells beat that pair's break-even and are traded. Orange cells have a rule that looks good, but the bot's picks there aren't winning enough, so it stands aside. Click a cell to select it.

What has been learned is saved to `training.txt` every minute, after chart history arrives, and when the app closes. On the next start it is merged with the saved tick logs, so learning carries on from where it stopped, even for history older than the week of ticks the logs keep. Demo pairs are simulated afresh each start, so their training isn't saved.

## Auto mode

**Pairs traded → Auto: any streaming pair** (or `--auto`) trades whichever pair has a qualified signal on the selected timeframe, not just the pair on screen. It trades pairs of the same kind as the selected one: Pocket Option pairs, or demo pairs.

- **Auto pairs → choose…** limits it to the pairs you tick. With none ticked it trades any pair.
- One trade at a time, taken from the first pair whose candle closes with a signal. So the multiplier after a loss, Pause and the real-money loss limit work as before.
- After a loss, the multiplied trade stays on the pair that lost, and waits for its next signal. If that pair stops streaming, falls below the minimum payout or is taken off the list, the run carries on with whichever pair signals next.
- Each pair is held to its own payout: it must pay at least the minimum payout, and its rule must beat that payout's break-even.
- Orders go to Pocket Option as usual, through the tab streaming that pair.
- Switching the chart doesn't reset the auto account or close its trade.
- If the open trade's pair stops sending prices, the trade is dropped after three candles. A real-money order behind it counts as lost toward the loss limit.

## Running overnight

**Sleep → Stay awake, screen stays on** (or `--keep-awake`) stops the computer sleeping while the app is open. On macOS it holds the same power assertions as `caffeinate` (`pmset -g assertions` lists them), which also stops App Nap from slowing the app down. *Stay awake, screen can turn off* saves power, but the browser streaming Pocket Option may then be throttled, so the screen-on option is the safer choice overnight. An **AWAKE** pill shows while it's on.

On a laptop, keep the lid open and the charger plugged in, because closing the lid still sleeps it. Keep the Pocket Option tab open and logged in. Real-money trading still turns itself off on restart and at the loss limit, so check it before leaving it unattended.

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

**Balance** (*Balance* in the account card). Either a paper balance you type in (`--balance`, default 1000), or **Sync with Pocket Option**: the account then follows the balance Pocket Option reports. That's the demo account's balance when trades go to demo or nowhere, and the real account's in real-money mode. % stakes and the check that a stake is affordable then use your actual balance. The bridge (0.6.0 or newer) reads the balance when the Pocket Option tab loads and after each trade, so reload the tab if it shows *Waiting for your balance*.

**Stake as a % of the balance** (`--stake-pct`, *Stake → % of balance* in the app). Each run of trades starts at that % of the paper balance, at least 1. A multiplied stake after a loss scales the run's first stake, not the lower balance. With *Sync with Pocket Option* it's a % of your Pocket Option balance; otherwise of the paper balance.

**Amount and multiplier after a loss** (`--stake`, `--martingale` default 4, `--martingale-mult` default 2.1). After a loss the next amount is multiplied by 2.1, up to 4 losses in a row (10 → 21 → 44.10 → 92.61 → 194.48). A win, a fifth loss in a row, or a multiplied amount the paper balance can't cover goes back to the base amount. You can change all three in the app's account card, or set *After a loss* to keep the same amount.

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

**Trade frequency** (*Trade frequency* in the app, remembered between sessions) sets how sure the bot must be before it trades. It sets the rules' confidence (`--z`) and the track record check (`--track-z`, `--track-min`, see *How it decides*) together:

| level | rules | track record | 1m on the saved Pocket Option ticks ($10, 88%) |
|---|---|---|---|
| Careful (default) | lower bound beats break-even | lower bound beats break-even, 40+ picks | 0.7% of candles, 43.8% won, −$1.77 a trade |
| Balanced | lower bound | win rate beats break-even, 20+ picks | 5.9% of candles, 51.2% won, −$0.38 a trade |
| Active | win rate beats break-even | win rate, 20+ picks | 15% of candles, 52.2% won, −$0.18 a trade |
| No track record check | lower bound | — | 16% of candles, 49.5% won, −$0.69 a trade |

On 15s and 2m Careful made no trades, and every looser level lost money there. On 30s Balanced came closest, at −$0.07 a trade. These figures rest on few trades, so they're noisy, and no level reached break-even on these feeds.

## How it decides

OTC prices come from each broker's own price generator, and the generators aren't public, so no fixed indicator recipe can be trusted to carry over. The bot learns from the feed it is given instead:

1. Each closed candle feeds 12 base signals: last candle colour, 3-candle streak, the move in the last sixth of the candle, RSI(2), RSI(14), Bollinger ±2σ and ±1σ, stochastic(5,3), EMA 9/21 trend, wick rejection, trend pullback, and where the close sits in the candle's range.
2. Each base signal is a pair of rules: **follow** (trade in its direction) and **fade** (trade against it), 24 rules in total. The feed decides which direction works.
3. Every vote from every rule is scored against the next candle, whether it was traded or not. Recent votes count more (`--half-life`, 400 votes by default).
4. A rule only qualifies while the lower confidence bound of its win rate (Wilson, `--z`) is above the pair's break-even rate at its current payout (54.1% at 85%, 52.1% at 92%).
5. **Track record.** The best of 24 rules always looks better than it is, because whichever rule got lucky lately rises to the top. On a feed with no lasting pattern that is usually a trend-follower riding the last move, so the bot would jump on a trend just as it ran out. To catch this, the bot scores its own picks on the next candle, traded or not, with a shorter memory (`--track-half-life`, 300 picks). That record is out of sample, because each pick is made before its candle is known. A qualifying rule is only traded once the bot has made at least 40 picks (`--track-min`) and the lower bound of their win rate is above break-even. When a trend breaks or the market turns choppy, the record drops and the bot stands aside. It resumes by itself when its picks win again. `--track-min 0` turns this off.

Each pair and timeframe is learned separately. The decision takes about 250 ns. Logging, the UI and the bridge all run on other threads, after the decision is made.

### Calibration (simulated, 6 seeds × 20,000 candles, z = 1, fixed stake)

| feed | candles traded | win rate | per $1 staked | without the track record |
|---|---|---|---|---|
| random walk (no edge) | 0% | — | 0 | 0.4% traded, −0.18 |
| weak reversal (true ≈53%, below break-even) | 0.3% | 50.6% | −0.06 | 10.5% traded, −0.013 |
| reversal 0.2 (true ≈56%) | 46% | 56.4% | +0.044 | 80% traded, +0.051 |
| reversal 0.3 (true ≈60%) | 94% | 59.9% | +0.108 | 98% traded, +0.107 |
| momentum 0.2 | 49% | 56.6% | +0.048 | 81% traded, +0.052 |

On noise and on edges too small to pay, it now almost never trades. On a strong pattern it trades nearly every candle. The cost is a marginal edge (56%): it trades about half as often there, because its own record takes longer to prove itself. Lower `--z` (e.g. 0 or 0.5) for more signals at the cost of more noise trades.

### What the real Pocket Option data showed (October 2026)

Replaying the saved tick logs (8 OTC pairs, about 7,000 one-minute candles) through the app, with a fixed $10 stake at an 88% payout:

| timeframe | without the track record | with it |
|---|---|---|
| 15s | 1,496 trades, 50.9% won, −$634 | no trades |
| 30s | 746 trades, 48.9% won, −$598 | 73 trades, 47.9% won, −$72 |
| 1m | 709 trades, 49.6% won, −$472 | 32 trades, 43.8% won, −$57 |
| 2m | 409 trades, 48.9% won, −$330 | no trades |

The 568 trades the app had already placed on these pairs won 48.6%. Their rules had claimed recent win rates of 56–62%, and the rules claiming 60% or more won only 45%. None of the 12 base signals predicted the next candle better than about 51% on any timeframe, and neither did streaks or trend strength. So far these OTC feeds behave like a coin flip, and the right decision has mostly been not to trade. The track record exists so that the bot reaches that conclusion by itself, and starts trading again if a pattern shows up.

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
