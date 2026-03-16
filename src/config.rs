use anyhow::{anyhow, Result};
use chrono::{Datelike, NaiveTime, Timelike};
use chrono_tz::Tz;
use clap::Parser;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

// ---------------------------------------------------------------------------
// CLI arguments
// ---------------------------------------------------------------------------

#[derive(Parser, Debug)]
#[command(
    name = "kalshi-bot",
    about = "Momentum-based crypto trading bot for Kalshi prediction markets"
)]
pub struct CliArgs {
    /// Run in simulation mode (no real orders placed). This is the default.
    #[arg(long, default_value_t = true)]
    pub simulation: bool,

    /// Disable simulation mode and place real orders.
    #[arg(long = "no-simulation", overrides_with = "simulation")]
    pub no_simulation: bool,

    /// Path to the JSON configuration file.
    #[arg(long, default_value = "config.json")]
    pub config: String,
}

// ---------------------------------------------------------------------------
// Configuration structs
// ---------------------------------------------------------------------------

/// A single time-based blackout window during which no new buy orders are placed.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NoTradeWindow {
    /// Human-readable name for this window (e.g. "Market Open Volatility").
    pub name: String,
    /// Days of the week this window applies to (e.g. ["Mon", "Tue", "Wed", "Thu", "Fri"]).
    pub days: Vec<String>,
    /// Start time in 24-hour "HH:MM" format.
    pub start_time: String,
    /// End time in 24-hour "HH:MM" format.
    pub end_time: String,
}

/// Schedule that suppresses new buy orders during defined time windows.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NoTradeSchedule {
    /// Master toggle — set to false to disable all no-trade windows without removing them.
    #[serde(default)]
    pub enabled: bool,
    /// IANA timezone name (e.g. "America/New_York"). Defaults to "UTC".
    #[serde(default = "default_no_trade_timezone")]
    pub timezone: String,
    /// List of blackout windows.
    #[serde(default)]
    pub windows: Vec<NoTradeWindow>,
}

fn default_no_trade_timezone() -> String {
    "UTC".to_string()
}

impl Default for NoTradeSchedule {
    fn default() -> Self {
        NoTradeSchedule {
            enabled: false,
            timezone: default_no_trade_timezone(),
            windows: vec![],
        }
    }
}

impl NoTradeSchedule {
    /// Returns `true` if the current wall-clock time falls inside any configured blackout window.
    ///
    /// When `enabled` is false or `windows` is empty this always returns `false`.
    /// Falls back to UTC (with a warning) when the configured timezone string is invalid.
    pub fn is_no_trade_time(&self) -> bool {
        if !self.enabled || self.windows.is_empty() {
            return false;
        }

        let tz: Tz = match self.timezone.parse() {
            Ok(t) => t,
            Err(_) => {
                log::warn!(
                    "no_trade_schedule: invalid timezone '{}', falling back to UTC",
                    self.timezone
                );
                chrono_tz::UTC
            }
        };

        let now = chrono::Utc::now().with_timezone(&tz);
        let weekday = now.weekday();
        // hour/minute/second from a valid DateTime are always in range; unwrap is safe.
        let current_time = NaiveTime::from_hms_opt(now.hour(), now.minute(), now.second())
            .expect("hour/minute/second from DateTime are always valid");

        let day_abbrev = match weekday {
            chrono::Weekday::Mon => "mon",
            chrono::Weekday::Tue => "tue",
            chrono::Weekday::Wed => "wed",
            chrono::Weekday::Thu => "thu",
            chrono::Weekday::Fri => "fri",
            chrono::Weekday::Sat => "sat",
            chrono::Weekday::Sun => "sun",
        };

        for window in &self.windows {
            // Case-insensitive day check
            let day_match = window.days.iter().any(|d| d.to_lowercase() == day_abbrev);
            if !day_match {
                continue;
            }

            let start = match NaiveTime::parse_from_str(&window.start_time, "%H:%M") {
                Ok(t) => t,
                Err(_) => {
                    log::warn!(
                        "no_trade_schedule: window '{}' has invalid start_time '{}', skipping",
                        window.name,
                        window.start_time
                    );
                    continue;
                }
            };
            let end = match NaiveTime::parse_from_str(&window.end_time, "%H:%M") {
                Ok(t) => t,
                Err(_) => {
                    log::warn!(
                        "no_trade_schedule: window '{}' has invalid end_time '{}', skipping",
                        window.name,
                        window.end_time
                    );
                    continue;
                }
            };

            // Support midnight-spanning windows (e.g. 22:00–02:00).
            let in_window = if start <= end {
                current_time >= start && current_time < end
            } else {
                current_time >= start || current_time < end
            };

            if in_window {
                log::info!(
                    "No-trade window active: '{}' ({} – {} {})",
                    window.name,
                    window.start_time,
                    window.end_time,
                    self.timezone
                );
                return true;
            }
        }

        false
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct KalshiConfig {
    /// Base URL for the Kalshi API.
    /// Demo: "https://demo-api.kalshi.co"
    /// Production: "https://api.elections.kalshi.com"
    pub api_base_url: String,

    /// Your Kalshi API key ID (from the Kalshi dashboard).
    pub api_key_id: Option<String>,

    /// Path to your RSA private key PEM file used for JWT signing.
    pub private_key_path: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TradingConfig {
    /// How often to check for opportunities, in milliseconds.
    pub check_interval_ms: u64,

    /// Minimum Yes bid price to trigger a buy, in cents (default 87).
    pub trigger_price: i64,

    /// Minimum minutes elapsed since market open before buying (default 5.0).
    pub min_elapsed_minutes: f64,

    /// Target sell price for late window trades, in cents (default 99).
    /// Used when `late_window_sell_enabled` is true. When a late window trade's
    /// bid reaches this price, the position is sold for profit.
    pub sell_price: i64,

    /// Enable profit-target selling for late window trades (default: false).
    /// When true, late window positions will be sold when the bid reaches `sell_price`.
    /// When false, late window positions are held until market settlement or stop-loss.
    #[serde(default)]
    pub late_window_sell_enabled: bool,

    /// Maximum buy price in cents (default 95).
    pub max_buy_price: i64,

    /// Minimum seconds remaining before market close to allow entry (default 30).
    pub min_time_remaining_seconds: i64,

    /// Kalshi event series tickers to monitor (e.g. ["KXBTC", "KXETH"]).
    pub market_series_tickers: Vec<String>,

    /// Enable BTC markets.
    pub enable_btc: bool,

    /// Enable ETH markets.
    pub enable_eth: bool,

    /// Fixed number of contracts to buy per trade signal (default 1).
    #[serde(default = "default_fixed_trade_amount")]
    pub fixed_trade_amount: f64,

    /// Maximum number of concurrent open positions per asset type (default 1).
    #[serde(default = "default_max_concurrent_positions")]
    pub max_concurrent_positions: usize,

    /// Use WebSocket for real-time market data instead of REST polling.
    #[serde(default = "default_use_websocket")]
    pub use_websocket: bool,

    /// Enable early window buying (default: false).
    /// When true, the bot will also consider buying during the first few minutes of a market's life.
    #[serde(default = "default_early_window_enabled")]
    pub early_window_enabled: bool,

    /// Minimum minutes elapsed before early window opens (default: 1.0).
    /// Gives prices time to stabilize after market open.
    #[serde(default = "default_early_window_start_minutes")]
    pub early_window_start_minutes: f64,

    /// Maximum minutes elapsed for early window (default: 5.0).
    /// After this, the early window closes and only the late window applies.
    #[serde(default = "default_early_window_end_minutes")]
    pub early_window_end_minutes: f64,

    /// Trigger price for early window buys, in cents (default: 88).
    #[serde(default = "default_early_window_trigger_price")]
    pub early_window_trigger_price: i64,

    /// Maximum buy price for early window buys, in cents (default: 92).
    #[serde(default = "default_early_window_max_buy_price")]
    pub early_window_max_buy_price: i64,

    /// Take-profit (sell) price for early window trades, in cents (default: 99).
    /// When an early window trade reaches this bid price, the position is sold.
    #[serde(default = "default_early_window_sell_price")]
    pub early_window_sell_price: i64,

    /// Enable profit-target selling for early window trades (default: true).
    /// When true, early window positions will be sold when the bid reaches `early_window_sell_price`.
    /// When false, early window positions are held until market settlement or stop-loss.
    #[serde(default = "default_early_window_sell_enabled")]
    pub early_window_sell_enabled: bool,

    /// Enable momentum confirmation for late-window entries (default: true).
    /// When true, a late-window entry is only allowed if the current bid is at or
    /// above the sliding-window average bid over the prior `momentum_window_seconds`.
    /// Early-window buys and stopped-out re-entries are unaffected.
    #[serde(default = "default_momentum_enabled")]
    pub momentum_enabled: bool,

    /// Width of the sliding window used for momentum confirmation, in seconds (default: 10).
    #[serde(default = "default_momentum_window_seconds")]
    pub momentum_window_seconds: u64,

    /// Minimum trend in cents required for momentum confirmation (default: 0).
    /// Entry is only allowed when `current_bid >= avg_bid + momentum_min_trend`.
    #[serde(default)]
    pub momentum_min_trend: i64,

    /// Minimum seconds remaining before market close for stop-loss sells to be allowed.
    /// When remaining time is below this threshold, SL checks are skipped entirely
    /// and the position is held until market settlement. Prevents selling into
    /// thin liquidity near market close. Default: 60 seconds.
    #[serde(default = "default_stop_loss_min_remaining_seconds")]
    pub stop_loss_min_remaining_seconds: i64,

    /// Stop-loss price in cents. If the current bid drops to or below this value,
    /// the entire position is sold immediately. Default: 50.
    #[serde(default = "default_stop_loss_price")]
    pub stop_loss_price: i64,

    /// When true, stop-loss sells use market orders for guaranteed execution regardless
    /// of price. When false, limit orders at the current bid are used instead.
    /// Default: true.
    #[serde(default = "default_stop_loss_use_market_order")]
    pub stop_loss_use_market_order: bool,
}

fn default_fixed_trade_amount() -> f64 {
    1.0
}

fn default_max_concurrent_positions() -> usize {
    1
}

fn default_use_websocket() -> bool {
    true
}

fn default_early_window_enabled() -> bool {
    false
}

fn default_early_window_start_minutes() -> f64 {
    1.0
}

fn default_early_window_end_minutes() -> f64 {
    5.0
}

fn default_early_window_trigger_price() -> i64 {
    88
}

fn default_early_window_max_buy_price() -> i64 {
    92
}

fn default_early_window_sell_price() -> i64 {
    99
}

fn default_early_window_sell_enabled() -> bool {
    true
}

fn default_momentum_enabled() -> bool {
    true
}

fn default_momentum_window_seconds() -> u64 {
    10
}

fn default_stop_loss_min_remaining_seconds() -> i64 {
    60
}

fn default_stop_loss_price() -> i64 {
    50
}

fn default_stop_loss_use_market_order() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    pub kalshi: KalshiConfig,
    pub trading: TradingConfig,
    #[serde(default)]
    pub no_trade_schedule: NoTradeSchedule,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            kalshi: KalshiConfig {
                api_base_url: "https://demo-api.kalshi.co".to_string(),
                api_key_id: None,
                private_key_path: None,
            },
            trading: TradingConfig {
                check_interval_ms: 1000,
                trigger_price: 87,
                min_elapsed_minutes: 5.0,
                sell_price: 99,
                late_window_sell_enabled: false,
                max_buy_price: 95,
                min_time_remaining_seconds: 30,
                market_series_tickers: vec!["KXBTC".to_string(), "KXETH".to_string()],
                enable_btc: true,
                enable_eth: true,
                fixed_trade_amount: 1.0,
                max_concurrent_positions: 1,
                use_websocket: true,
                early_window_enabled: false,
                early_window_start_minutes: 1.0,
                early_window_end_minutes: 5.0,
                early_window_trigger_price: 88,
                early_window_max_buy_price: 92,
                early_window_sell_price: 99,
                early_window_sell_enabled: true,
                momentum_enabled: true,
                momentum_window_seconds: 10,
                momentum_min_trend: 0,
                stop_loss_min_remaining_seconds: 60,
                stop_loss_price: 50,
                stop_loss_use_market_order: true,
            },
            no_trade_schedule: NoTradeSchedule {
                enabled: true,
                timezone: "America/New_York".to_string(),
                windows: vec![
                    NoTradeWindow {
                        name: "Market Open Volatility".to_string(),
                        days: vec![
                            "Mon".to_string(),
                            "Tue".to_string(),
                            "Wed".to_string(),
                            "Thu".to_string(),
                            "Fri".to_string(),
                        ],
                        start_time: "09:30".to_string(),
                        end_time: "10:30".to_string(),
                    },
                ],
            },
        }
    }
}

impl Config {
    /// Load config from the given path. Creates a default config file if it doesn't exist.
    pub fn load(path: &str) -> Result<Self> {
        if !Path::new(path).exists() {
            let default = Config::default();
            let json = serde_json::to_string_pretty(&default)?;
            fs::write(path, &json)?;
            log::info!("Created default config file at {path}");
        }

        let raw = fs::read_to_string(path)
            .map_err(|e| anyhow!("Failed to read config file '{}': {}", path, e))?;
        let config: Config = serde_json::from_str(&raw)
            .map_err(|e| anyhow!("Failed to parse config file '{}': {}", path, e))?;

        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        let t = &self.trading;

        if t.trigger_price < 1 || t.trigger_price > 99 {
            return Err(anyhow!("trigger_price must be between 1 and 99 cents"));
        }
        if t.max_buy_price < 1 || t.max_buy_price > 99 {
            return Err(anyhow!("max_buy_price must be between 1 and 99 cents"));
        }
        if t.trigger_price > t.max_buy_price {
            return Err(anyhow!(
                "trigger_price ({}) must be <= max_buy_price ({})",
                t.trigger_price,
                t.max_buy_price
            ));
        }
        if t.stop_loss_price < 1 || t.stop_loss_price > 99 {
            return Err(anyhow!("stop_loss_price must be between 1 and 99 cents"));
        }
        if t.market_series_tickers.is_empty() {
            return Err(anyhow!("market_series_tickers must not be empty"));
        }
        if t.early_window_enabled {
            if t.early_window_start_minutes < 0.0 {
                return Err(anyhow!("early_window_start_minutes must be >= 0"));
            }
            if t.early_window_end_minutes <= t.early_window_start_minutes {
                return Err(anyhow!(
                    "early_window_end_minutes ({}) must be > early_window_start_minutes ({})",
                    t.early_window_end_minutes,
                    t.early_window_start_minutes
                ));
            }
            if t.early_window_trigger_price < 1 || t.early_window_trigger_price > 99 {
                return Err(anyhow!("early_window_trigger_price must be between 1 and 99 cents"));
            }
            if t.early_window_max_buy_price < 1 || t.early_window_max_buy_price > 99 {
                return Err(anyhow!("early_window_max_buy_price must be between 1 and 99 cents"));
            }
            if t.early_window_trigger_price > t.early_window_max_buy_price {
                return Err(anyhow!(
                    "early_window_trigger_price ({}) must be <= early_window_max_buy_price ({})",
                    t.early_window_trigger_price,
                    t.early_window_max_buy_price
                ));
            }
            if t.early_window_sell_enabled {
                if t.early_window_sell_price < 1 || t.early_window_sell_price > 99 {
                    return Err(anyhow!("early_window_sell_price must be between 1 and 99 cents"));
                }
                if t.early_window_sell_price <= t.early_window_max_buy_price {
                    return Err(anyhow!(
                        "early_window_sell_price ({}) must be > early_window_max_buy_price ({})",
                        t.early_window_sell_price,
                        t.early_window_max_buy_price
                    ));
                }
            }
            if t.early_window_end_minutes > t.min_elapsed_minutes {
                return Err(anyhow!(
                    "early_window_end_minutes ({}) must be <= min_elapsed_minutes ({}) to avoid overlap",
                    t.early_window_end_minutes,
                    t.min_elapsed_minutes
                ));
            }
        }

        // Validate sell_price when late_window_sell_enabled is true
        if t.late_window_sell_enabled {
            if t.sell_price < 1 || t.sell_price > 99 {
                return Err(anyhow!("sell_price must be between 1 and 99 cents"));
            }
            if t.sell_price <= t.max_buy_price {
                return Err(anyhow!(
                    "sell_price ({}) must be > max_buy_price ({}) to ensure profit",
                    t.sell_price,
                    t.max_buy_price
                ));
            }
        }

        // Validate no_trade_schedule when enabled
        let nts = &self.no_trade_schedule;
        if nts.enabled {
            if nts.timezone.parse::<Tz>().is_err() {
                return Err(anyhow!(
                    "no_trade_schedule.timezone '{}' is not a valid IANA timezone",
                    nts.timezone
                ));
            }
            for window in &nts.windows {
                if window.days.is_empty() {
                    return Err(anyhow!(
                        "no_trade_schedule window '{}' must have at least one day",
                        window.name
                    ));
                }
                NaiveTime::parse_from_str(&window.start_time, "%H:%M").map_err(|_| {
                    anyhow!(
                        "no_trade_schedule window '{}' has invalid start_time '{}' (expected HH:MM)",
                        window.name,
                        window.start_time
                    )
                })?;
                NaiveTime::parse_from_str(&window.end_time, "%H:%M").map_err(|_| {
                    anyhow!(
                        "no_trade_schedule window '{}' has invalid end_time '{}' (expected HH:MM)",
                        window.name,
                        window.end_time
                    )
                })?;
            }
        }

        Ok(())
    }

    /// Returns true if we are in simulation (paper trading) mode.
    pub fn is_simulation(args: &CliArgs) -> bool {
        !args.no_simulation
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(name: &str, days: &[&str], start: &str, end: &str) -> NoTradeWindow {
        NoTradeWindow {
            name: name.to_string(),
            days: days.iter().map(|d| d.to_string()).collect(),
            start_time: start.to_string(),
            end_time: end.to_string(),
        }
    }

    fn schedule_with_windows(enabled: bool, windows: Vec<NoTradeWindow>) -> NoTradeSchedule {
        NoTradeSchedule {
            enabled,
            timezone: "UTC".to_string(),
            windows,
        }
    }

    #[test]
    fn stop_loss_use_market_order_default_is_true() {
        let config = Config::default();
        assert!(config.trading.stop_loss_use_market_order);
    }

    #[test]
    fn disabled_schedule_returns_false() {
        let schedule = schedule_with_windows(
            false,
            vec![window(
                "All Day Every Day",
                &["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"],
                "00:00",
                "23:59",
            )],
        );
        assert!(!schedule.is_no_trade_time());
    }

    #[test]
    fn empty_windows_returns_false() {
        let schedule = schedule_with_windows(true, vec![]);
        assert!(!schedule.is_no_trade_time());
    }

    #[test]
    fn matching_day_and_time_returns_true() {
        use chrono::{Datelike, TimeZone, Utc, Weekday};

        // Find a Monday-in-UTC that is within 09:30–10:30 UTC.
        // We can't control "now", so instead we test with all-day every-day windows.
        let schedule = schedule_with_windows(
            true,
            vec![window(
                "All Day Every Day",
                &["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"],
                "00:00",
                "23:59",
            )],
        );
        // Regardless of when this test runs, all days and almost all times are covered.
        assert!(schedule.is_no_trade_time());
    }

    #[test]
    fn non_matching_time_returns_false() {
        // Use a window from 00:00–00:01 on a fixed day.
        // The current UTC time is almost certainly outside 00:00–00:01,
        // but cover ourselves with all days so the day check always passes.
        let now_utc = chrono::Utc::now();
        let current_minute = now_utc.hour() * 60 + now_utc.minute();
        // If it's 00:00, this test can't work — skip instead of being flaky.
        if current_minute == 0 {
            return;
        }
        let schedule = schedule_with_windows(
            true,
            vec![window(
                "Narrow Window",
                &["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"],
                "00:00",
                "00:01",
            )],
        );
        assert!(!schedule.is_no_trade_time());
    }

    #[test]
    fn non_matching_day_returns_false() {
        use chrono::Datelike;

        let now_utc = chrono::Utc::now();
        let weekday = now_utc.weekday();

        // Build a list of all days EXCEPT today.
        let all_days = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
        let today_abbrev = match weekday {
            chrono::Weekday::Mon => "Mon",
            chrono::Weekday::Tue => "Tue",
            chrono::Weekday::Wed => "Wed",
            chrono::Weekday::Thu => "Thu",
            chrono::Weekday::Fri => "Fri",
            chrono::Weekday::Sat => "Sat",
            chrono::Weekday::Sun => "Sun",
        };
        let other_days: Vec<&str> = all_days.iter().copied().filter(|&d| d != today_abbrev).collect();

        let schedule = schedule_with_windows(
            true,
            vec![window("Other Days Only", &other_days, "00:00", "23:59")],
        );
        assert!(!schedule.is_no_trade_time());
    }
}
