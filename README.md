# guap_test — Kalshi Crypto Trading Bot (Demo API)

A momentum-based cryptocurrency trading bot written in Rust, targeting [Kalshi](https://kalshi.com/) short-duration prediction markets (BTC/ETH price prediction contracts).

> **This version is configured to use the Kalshi _demo_ API exclusively.**
> REST: `https://demo-api.kalshi.co/trade-api/v2`
> WebSocket: `wss://demo-api.kalshi.co/trade-api/ws/v2`

---

## How the Momentum Strategy Works

The bot monitors Kalshi's short-duration (e.g. 15-minute, hourly) BTC and ETH crypto prediction markets and applies the following logic:

1. **Market Discovery** — Automatically discovers open crypto prediction markets by querying the Kalshi API for configured event series (e.g. `KXBTC`, `KXETH`).

2. **Entry Conditions** (all must be true to trigger a buy):
   - The Yes contract bid price is between `trigger_price` (default 87¢) and `max_buy_price` (default 95¢).
   - At least `min_elapsed_minutes` (default 5 min) have passed since the market opened (**late window**).
   - At least `min_time_remaining_seconds` (default 30 s) remain before market close.
   - No existing open position in this market.
   - *(Optional)* **Momentum confirmation** — when `momentum_enabled: true` (default), the current bid must be at or above the sliding-window average bid over the prior `momentum_window_seconds`.
   - *(Optional)* **Early window** — when `early_window_enabled: true`, the bot can also buy during the first few minutes of a market's life (see [Early Window](#early-window) below).

3. **Buy Execution** — Places a limit buy order for Yes contracts at the current best ask price, for a fixed USD amount (default $1.00). All prices use Kalshi's integer cent format (1–99).

4. **Exit Conditions**:
   - **Profit target** — Sells when the bid reaches `sell_price` (default 99¢).
   - **Stop-loss** — Sells the entire position when the bid drops to or below `stop_loss_price` (default 50¢). Uses a market order by default (`stop_loss_use_market_order: true`).
   - **Market settlement** — Kalshi settles contracts automatically; the bot just logs the result.

---

## Prerequisites

- **Rust 1.70+** — Install via [rustup](https://rustup.rs/)
- **Kalshi demo account** — Sign up at [kalshi.com](https://kalshi.com/) and use the demo environment
- **API credentials** — API Key ID and an RSA private key (PKCS#8 PEM format) from the Kalshi demo API dashboard

---

## Installation

```bash
git clone https://github.com/michaeltodmurphy-beep/guap_test
cd guap_test
cargo build --release
```

The compiled binary will be at `target/release/kalshi-bot`.

---

## Configuration

On first run the bot creates a default `config.json` pointing at the **demo** API:

```json
{
  "kalshi": {
    "api_base_url": "https://demo-api.kalshi.co",
    "api_key_id": null,
    "private_key_path": null
  },
  "trading": {
    "check_interval_ms": 1000,
    "fixed_trade_amount": 1.0,
    "trigger_price": 87,
    "min_elapsed_minutes": 5,
    "sell_price": 99,
    "late_window_sell_enabled": false,
    "max_buy_price": 95,
    "min_time_remaining_seconds": 30,
    "market_series_tickers": ["KXBTC", "KXETH"],
    "enable_btc": true,
    "enable_eth": true,
    "max_concurrent_positions": 1,
    "use_websocket": true,
    "stop_loss_price": 50,
    "stop_loss_min_remaining_seconds": 60,
    "stop_loss_use_market_order": true,
    "momentum_enabled": true,
    "momentum_window_seconds": 10,
    "momentum_min_trend": 0,
    "early_window_enabled": false,
    "early_window_start_minutes": 1.0,
    "early_window_end_minutes": 5.0,
    "early_window_trigger_price": 88,
    "early_window_max_buy_price": 92,
    "early_window_sell_price": 99,
    "early_window_sell_enabled": true
  }
}
```

### Field Reference

| Field | Description |
|---|---|
| `api_base_url` | `https://demo-api.kalshi.co` (demo) or `https://api.elections.kalshi.com` (production) |
| `api_key_id` | Your Kalshi API key ID |
| `private_key_path` | Path to your RSA private key PEM file (PKCS#8 format) |
| `check_interval_ms` | How often to check for opportunities (milliseconds) |
| `fixed_trade_amount` | USD amount per trade (e.g. `1.0` = $1.00) |
| `trigger_price` | Minimum Yes bid price to enter in the late window (cents, 1–99) |
| `max_buy_price` | Maximum Yes ask price to enter (cents, 1–99) |
| `sell_price` | Profit-target sell price for late-window trades (cents, 1–99) |
| `late_window_sell_enabled` | When `true`, late-window positions are sold at `sell_price`. When `false`, they are held until settlement (default `false`) |
| `min_elapsed_minutes` | Minimum market age before **late window** entry (minutes) |
| `min_time_remaining_seconds` | Minimum time remaining before close (seconds) |
| `market_series_tickers` | Kalshi event series to monitor (e.g. `["KXBTC", "KXETH"]`) |
| `enable_btc` | Whether to trade BTC markets |
| `enable_eth` | Whether to trade ETH markets |
| `max_concurrent_positions` | Maximum number of open positions per asset type (default `1`) |
| `use_websocket` | Use WebSocket for real-time price data instead of REST polling (default `true`) |
| `stop_loss_price` | Bid price at which the entire position is sold immediately (cents, 1–99; default `50`) |
| `stop_loss_min_remaining_seconds` | Minimum seconds before close for stop-loss to fire. Below this threshold, SL is skipped (default `60`) |
| `stop_loss_use_market_order` | Use market orders for stop-loss sells to guarantee execution (default `true`) |
| `momentum_enabled` | Require bid to be at or above the sliding-window average before entry (default `true`) |
| `momentum_window_seconds` | Width of the sliding window for momentum confirmation (default `10`) |
| `momentum_min_trend` | Minimum required bid increase over the window, in cents (default `0`) |
| `early_window_enabled` | Enable early window buying (default `false`; see [Early Window](#early-window)) |
| `early_window_start_minutes` | Minutes elapsed before early window opens (default `1.0`) |
| `early_window_end_minutes` | Minutes elapsed when early window closes (default `5.0`; must be ≤ `min_elapsed_minutes`) |
| `early_window_trigger_price` | Minimum bid for early window entry (cents, 1–99; default `88`) |
| `early_window_max_buy_price` | Maximum ask for early window entry (cents, 1–99; default `92`) |
| `early_window_sell_price` | Profit-target sell price for early-window trades (cents, default `99`) |
| `early_window_sell_enabled` | When `true`, early-window positions are sold at `early_window_sell_price` (default `true`) |

> **All price values are in cents (integers).** Kalshi contracts trade on a 1–99 cent scale where 99¢ ≈ near-certain Yes.

---

## Early Window

When `early_window_enabled: true`, the bot adds a second entry path for the first few minutes of a market's life. This allows capitalising on strong early momentum before the regular "late window" opens.

### How it works

| Time since market open | Active window | Trigger/max prices used |
|---|---|---|
| < `early_window_start_minutes` | Neither | — (too early, prices still stabilising) |
| `early_window_start_minutes` ≤ elapsed < `early_window_end_minutes` | **Early window** | `early_window_trigger_price` / `early_window_max_buy_price` |
| elapsed ≥ `early_window_end_minutes` AND < `min_elapsed_minutes` | Neither | — (gap between windows) |
| elapsed ≥ `min_elapsed_minutes` | **Late window** | `trigger_price` / `max_buy_price` |

> `early_window_end_minutes` must be ≤ `min_elapsed_minutes` so the windows never overlap.

### Example config

```json
"early_window_enabled": true,
"early_window_start_minutes": 1.0,
"early_window_end_minutes": 5.0,
"early_window_trigger_price": 88,
"early_window_max_buy_price": 92,
"early_window_sell_price": 99,
"early_window_sell_enabled": true
```

---

## No-Trade Schedule

The bot supports a configurable no-trade schedule to avoid trading during high-volatility periods (e.g. market open). The default schedule blocks trading on weekdays from 09:30–10:30 ET.

---

## Usage

### Simulation mode (default — no real orders placed)

```bash
./target/release/kalshi-bot
# or explicitly:
./target/release/kalshi-bot --simulation
```

### Live mode (places real demo orders)

```bash
./target/release/kalshi-bot --no-simulation
```

### Custom config path

```bash
./target/release/kalshi-bot --config /path/to/my-config.json
```

### Logging verbosity

```bash
RUST_LOG=debug ./target/release/kalshi-bot
```

Trade events are also appended to `history.toml` in the working directory.

---

## Project Structure

```
src/
├── main.rs         Entry point, main event loop
├── config.rs       CLI args and configuration management
├── models.rs       Kalshi API data structures
├── kalshi_api.rs   Kalshi REST API client (RSA JWT authentication)
├── monitor.rs      Market discovery and snapshot fetching
├── detector.rs     Momentum opportunity detection
├── trader.rs       Trade execution and position management
└── ws_monitor.rs   WebSocket market monitor
```

---

## ⚠️ Disclaimer

**This software is provided for educational purposes only.**
Trading prediction markets involves substantial risk of loss.
Past performance is not indicative of future results.
Never trade with money you cannot afford to lose.
Always test thoroughly in demo/simulation mode before using production credentials.

The authors accept no responsibility for any financial losses incurred through use of this software.
