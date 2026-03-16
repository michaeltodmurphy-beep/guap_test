use serde::{Deserialize, Deserializer, Serialize};
use chrono::{DateTime, Utc};

// ---------------------------------------------------------------------------
// Custom serde helpers for Kalshi _dollars price fields
// ---------------------------------------------------------------------------
//
// As of March 2026 Kalshi changed all `*_dollars` price fields from JSON
// numbers (e.g. `0.56`) to JSON strings (e.g. `"0.5600"`).  These helpers
// accept **both** representations so the bot works during any transition period.
//
// The same migration also replaced integer count/position fields (e.g. `count`,
// `position`) with string-encoded fixed-point fields (e.g. `count_fp: "3.00"`).

/// Deserialize an optional price that Kalshi may send as a JSON string ("0.56")
/// or as a JSON number (0.56), or as JSON null / absent.
fn deserialize_opt_dollars<'de, D>(deserializer: D) -> Result<Option<f64>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringOrFloat {
        Float(f64),
        Str(String),
    }

    match Option::<StringOrFloat>::deserialize(deserializer)? {
        None => Ok(None),
        Some(StringOrFloat::Float(f)) => Ok(Some(f)),
        Some(StringOrFloat::Str(s)) if s.is_empty() => Ok(None),
        Some(StringOrFloat::Str(s)) => s
            .parse::<f64>()
            .map(Some)
            .map_err(|_| serde::de::Error::custom(format!("invalid float string: {s}"))),
    }
}

/// Deserialize a required price that Kalshi may send as a JSON string ("0.56")
/// or as a JSON number (0.56).
fn deserialize_dollars<'de, D>(deserializer: D) -> Result<f64, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringOrFloat {
        Float(f64),
        Str(String),
    }

    match StringOrFloat::deserialize(deserializer)? {
        StringOrFloat::Float(f) => Ok(f),
        StringOrFloat::Str(s) => s
            .parse::<f64>()
            .map_err(|_| serde::de::Error::custom(format!("invalid float string: {s}"))),
    }
}

/// Deserialize an optional integer count / position that Kalshi may now send as a
/// fixed-point string (e.g. `"3.00"`) or as a legacy JSON integer (e.g. `3`).
fn deserialize_opt_string_or_int<'de, D>(deserializer: D) -> Result<Option<i64>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Val {
        Int(i64),
        Float(f64),
        Str(String),
    }

    match Option::<Val>::deserialize(deserializer)? {
        None => Ok(None),
        Some(Val::Int(i)) => Ok(Some(i)),
        Some(Val::Float(f)) => Ok(Some(f.round() as i64)),
        Some(Val::Str(s)) if s.is_empty() => Ok(None),
        Some(Val::Str(s)) => s
            .parse::<f64>()
            .map(|f| Some(f.round() as i64))
            .map_err(|_| serde::de::Error::custom(format!("invalid int/float string: {s}"))),
    }
}

/// Deserialize a required integer count / position that Kalshi may now send as a
/// fixed-point string (e.g. `"3.00"`) or as a legacy JSON integer (e.g. `3`).
fn deserialize_string_or_int<'de, D>(deserializer: D) -> Result<i64, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Val {
        Int(i64),
        Float(f64),
        Str(String),
    }

    match Val::deserialize(deserializer)? {
        Val::Int(i) => Ok(i),
        Val::Float(f) => Ok(f.round() as i64),
        Val::Str(s) => s
            .parse::<f64>()
            .map(|f| f.round() as i64)
            .map_err(|_| serde::de::Error::custom(format!("invalid int/float string: {s}"))),
    }
}

/// Deserialize one side of an orderbook (`yes` or `no`).  Each entry is a two-element
/// array `[price, quantity]` where either element may now be a JSON string (e.g.
/// `"0.9500"`) or a JSON number (e.g. `0.95`).
fn deserialize_orderbook_side<'de, D>(deserializer: D) -> Result<Vec<Vec<f64>>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Val {
        Float(f64),
        Int(i64),
        Str(String),
    }

    let outer: Vec<Vec<Val>> = Vec::deserialize(deserializer)?;
    outer
        .into_iter()
        .map(|inner| {
            inner
                .into_iter()
                .map(|v| match v {
                    Val::Float(f) => Ok(f),
                    Val::Int(i) => Ok(i as f64),
                    Val::Str(s) => s
                        .parse::<f64>()
                        .map_err(|_| serde::de::Error::custom(format!("invalid orderbook float string: {s}"))),
                })
                .collect::<Result<Vec<f64>, D::Error>>()
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Kalshi API response types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct KalshiEvent {
    pub event_ticker: String,
    pub series_ticker: String,
    pub title: String,
    pub status: String,
    #[serde(default)]
    pub markets: Vec<KalshiMarket>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct KalshiMarket {
    pub ticker: String,
    pub event_ticker: String,
    pub title: String,
    pub status: String,
    #[serde(default)]
    pub open_time: Option<DateTime<Utc>>,
    #[serde(default)]
    pub close_time: Option<DateTime<Utc>>,
    #[serde(default)]
    pub expiration_time: Option<DateTime<Utc>>,
    #[serde(default)]
    pub result: Option<String>,
    // New _dollars fields from Kalshi API (may be a JSON string or number, e.g. "0.95" or 0.95 = 95¢)
    #[serde(default, deserialize_with = "deserialize_opt_dollars")]
    pub yes_ask_dollars: Option<f64>,
    #[serde(default, deserialize_with = "deserialize_opt_dollars")]
    pub yes_bid_dollars: Option<f64>,
    #[serde(default, deserialize_with = "deserialize_opt_dollars")]
    pub no_ask_dollars: Option<f64>,
    #[serde(default, deserialize_with = "deserialize_opt_dollars")]
    pub no_bid_dollars: Option<f64>,
    #[serde(default, deserialize_with = "deserialize_opt_dollars")]
    pub last_price_dollars: Option<f64>,
    #[serde(default, deserialize_with = "deserialize_opt_string_or_int")]
    pub volume: Option<i64>,
}

impl KalshiMarket {
    /// Convert a dollars f64 to integer cents (e.g. 0.95 -> 95).
    fn dollars_to_cents(d: Option<f64>) -> Option<i64> {
        d.map(|v| (v * 100.0).round() as i64)
    }

    pub fn yes_ask(&self) -> Option<i64> { Self::dollars_to_cents(self.yes_ask_dollars) }
    pub fn yes_bid(&self) -> Option<i64> { Self::dollars_to_cents(self.yes_bid_dollars) }
    pub fn no_ask(&self) -> Option<i64> { Self::dollars_to_cents(self.no_ask_dollars) }
    pub fn no_bid(&self) -> Option<i64> { Self::dollars_to_cents(self.no_bid_dollars) }
    pub fn last_price(&self) -> Option<i64> { Self::dollars_to_cents(self.last_price_dollars) }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct KalshiOrderBook {
    #[serde(default, deserialize_with = "deserialize_orderbook_side")]
    pub yes: Vec<Vec<f64>>,
    #[serde(default, deserialize_with = "deserialize_orderbook_side")]
    pub no: Vec<Vec<f64>>,
}

impl KalshiOrderBook {
    /// Best ask price for Yes contracts (lowest offer to sell Yes), in cents.
    pub fn best_yes_ask(&self) -> Option<i64> {
        self.yes
            .iter()
            .filter_map(|v| v.first().copied())
            .filter(|v| v.is_finite())
            .min_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
            .map(|d| (d * 100.0).round() as i64)
    }

    /// Best bid price for Yes contracts, in cents.
    /// Derived from the No side: the lowest No ask at price X means
    /// someone is willing to buy Yes at (100 - X).
    pub fn best_yes_bid(&self) -> Option<i64> {
        self.no
            .iter()
            .filter_map(|v| v.first().copied())
            .filter(|v| v.is_finite())
            .min_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
            .map(|no_ask_dollars| 100 - (no_ask_dollars * 100.0).round() as i64)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum OrderSide {
    Yes,
    No,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum OrderType {
    Limit,
    Market,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum OrderStatus {
    Resting,
    Canceled,
    Executed,
    Pending,
    #[serde(other)]
    Unknown,
}

impl Default for OrderStatus {
    fn default() -> Self {
        OrderStatus::Pending
    }
}

impl Default for OrderSide {
    fn default() -> Self {
        OrderSide::Yes
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct KalshiOrderRequest {
    pub ticker: String,
    pub client_order_id: String,
    pub side: OrderSide,
    #[serde(rename = "type")]
    pub order_type: OrderType,
    /// Price in dollars as a string (e.g. "0.9500" for 95¢). Required by Kalshi API v2 March 2026+.
    /// Omitted for market orders.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub yes_price_dollars: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub no_price_dollars: Option<String>,
    /// Contract count as a fixed-point string (e.g. "1.00"). Required by Kalshi API v2 March 2026+.
    pub count_fp: String,
    pub action: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct KalshiOrder {
    pub order_id: String,
    #[serde(default)]
    pub ticker: String,
    pub client_order_id: Option<String>,
    #[serde(default)]
    pub status: OrderStatus,
    #[serde(default)]
    pub side: OrderSide,
    pub action: Option<String>,
    #[serde(default, deserialize_with = "deserialize_opt_dollars")]
    pub yes_price_dollars: Option<f64>,
    #[serde(default, deserialize_with = "deserialize_opt_dollars")]
    pub no_price_dollars: Option<f64>,
    /// Total contracts requested. Kalshi now sends this as `count_fp` (string e.g. "3.00");
    /// legacy integer `count` is also accepted via the alias for backward compatibility.
    #[serde(default, alias = "count_fp", deserialize_with = "deserialize_opt_string_or_int")]
    pub count: Option<i64>,
    /// Filled contracts. Kalshi now sends this as `filled_count_fp` (string).
    #[serde(default, alias = "filled_count_fp", deserialize_with = "deserialize_opt_string_or_int")]
    pub filled_count: Option<i64>,
    /// Remaining contracts. Kalshi now sends this as `remaining_count_fp` (string).
    #[serde(default, alias = "remaining_count_fp", deserialize_with = "deserialize_opt_string_or_int")]
    pub remaining_count: Option<i64>,
    pub created_time: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct KalshiPosition {
    pub ticker: String,
    /// Net position in contracts. Kalshi now sends this as `position_fp` (string e.g. "3.00");
    /// legacy integer `position` is also accepted via the alias.
    #[serde(alias = "position_fp", deserialize_with = "deserialize_string_or_int")]
    pub position: i64,
    pub market_exposure: Option<f64>,
    pub realized_pnl: Option<f64>,
    /// Total contracts traded. Kalshi now sends this as `total_traded_fp` (string).
    #[serde(default, alias = "total_traded_fp", deserialize_with = "deserialize_opt_string_or_int")]
    pub total_traded: Option<i64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct KalshiBalance {
    #[serde(deserialize_with = "deserialize_dollars")]
    pub balance_dollars: f64,
}

// ---------------------------------------------------------------------------
// Internal bot data structures
// ---------------------------------------------------------------------------

/// Current state of a monitored market.
#[derive(Debug, Clone)]
pub struct MarketSnapshot {
    pub ticker: String,
    pub event_ticker: String,
    pub title: String,
    pub yes_ask: Option<i64>,
    pub yes_bid: Option<i64>,
    pub no_ask: Option<i64>,
    pub no_bid: Option<i64>,
    pub last_price: Option<i64>,
    pub open_time: Option<DateTime<Utc>>,
    pub close_time: Option<DateTime<Utc>>,
    pub elapsed_seconds: i64,
    pub remaining_seconds: i64,
}

impl MarketSnapshot {
    pub fn elapsed_minutes(&self) -> f64 {
        self.elapsed_seconds as f64 / 60.0
    }
}

/// A detected buy opportunity.
#[derive(Debug, Clone)]
pub struct BuyOpportunity {
    pub ticker: String,
    pub event_ticker: String,
    pub market_type: MarketType,
    pub bid_price: i64,
    pub ask_price: i64,
    pub order_side: OrderSide,
    pub elapsed_minutes: f64,
    pub remaining_seconds: i64,
    /// True if this opportunity was detected in the early window.
    pub is_early_window: bool,
}

/// Type of crypto market.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MarketType {
    Btc,
    Eth,
}

impl std::fmt::Display for MarketType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MarketType::Btc => write!(f, "BTC"),
            MarketType::Eth => write!(f, "ETH"),
        }
    }
}

// ---------------------------------------------------------------------------
// WebSocket message types
// ---------------------------------------------------------------------------

/// Incoming WebSocket message wrapper
#[derive(Debug, Deserialize)]
pub struct WsMessage {
    #[serde(default)]
    pub id: Option<i64>,
    #[serde(rename = "type")]
    pub msg_type: Option<String>,
    pub msg: Option<WsTickerData>,
    /// For subscription confirmations
    pub cmd: Option<String>,
    pub result: Option<serde_json::Value>,
}

/// Ticker channel update data
#[derive(Debug, Clone, Deserialize)]
pub struct WsTickerData {
    pub market_ticker: Option<String>,
    #[serde(default, deserialize_with = "deserialize_opt_dollars")]
    pub yes_bid_dollars: Option<f64>,
    #[serde(default, deserialize_with = "deserialize_opt_dollars")]
    pub yes_ask_dollars: Option<f64>,
    #[serde(default, deserialize_with = "deserialize_opt_dollars")]
    pub no_bid_dollars: Option<f64>,
    #[serde(default, deserialize_with = "deserialize_opt_dollars")]
    pub no_ask_dollars: Option<f64>,
    #[serde(default, deserialize_with = "deserialize_opt_dollars")]
    pub last_price_dollars: Option<f64>,
    #[serde(default, deserialize_with = "deserialize_opt_string_or_int")]
    pub volume: Option<i64>,
    #[serde(default, deserialize_with = "deserialize_opt_string_or_int")]
    pub open_interest: Option<i64>,
}

impl WsTickerData {
    fn dollars_to_cents(d: Option<f64>) -> Option<i64> {
        d.map(|v| (v * 100.0).round() as i64)
    }

    pub fn yes_bid(&self) -> Option<i64> { Self::dollars_to_cents(self.yes_bid_dollars) }
    pub fn yes_ask(&self) -> Option<i64> { Self::dollars_to_cents(self.yes_ask_dollars) }
    pub fn no_bid(&self) -> Option<i64> { Self::dollars_to_cents(self.no_bid_dollars) }
    pub fn no_ask(&self) -> Option<i64> { Self::dollars_to_cents(self.no_ask_dollars) }
    pub fn last_price(&self) -> Option<i64> { Self::dollars_to_cents(self.last_price_dollars) }
}

/// Status of a pending trade.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq)]
pub enum TradeStatus {
    PendingFill,
    Open,
    Closed,
    StopLossTriggered,
}

/// Internal tracking record for an active or completed trade.
#[derive(Debug, Clone)]
pub struct PendingTrade {
    pub ticker: String,
    pub order_id: String,
    pub buy_price: i64,
    /// Actual number of contracts filled (may be less than `requested_units` on partial fill).
    pub units: i64,
    /// Number of contracts originally requested when placing the order.
    pub requested_units: i64,
    pub sell_target: i64,
    pub status: TradeStatus,
    pub entered_at: DateTime<Utc>,
    pub exited_at: Option<DateTime<Utc>>,
    pub realized_pnl: Option<f64>,
    pub market_type: MarketType,
    pub order_side: OrderSide,
    pub elapsed_minutes_at_entry: f64,
    /// True if this trade was entered via the early window.
    pub is_early_window: bool,
    /// Order IDs of sell orders placed for this trade (partial or full).
    /// Used to cancel any resting sell orders when the position is closed.
    pub sell_order_ids: Vec<String>,
}

// ---------------------------------------------------------------------------
// API response wrappers
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct EventsResponse {
    pub events: Vec<KalshiEvent>,
    pub cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct MarketsResponse {
    pub markets: Vec<KalshiMarket>,
    #[serde(default)]
    pub cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct MarketResponse {
    pub market: KalshiMarket,
}

#[derive(Debug, Deserialize)]
pub struct OrderBookResponse {
    #[serde(alias = "orderbook")]
    pub orderbook_fp: KalshiOrderBook,
}

#[derive(Debug, Deserialize)]
pub struct OrderResponse {
    pub order: KalshiOrder,
}

#[derive(Debug, Deserialize)]
pub struct PositionsResponse {
    pub market_positions: Vec<KalshiPosition>,
    pub cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct BalanceResponse {
    #[serde(deserialize_with = "deserialize_dollars")]
    pub balance_dollars: f64,
}

#[derive(Debug, Deserialize)]
pub struct ExchangeStatusResponse {
    pub exchange_active: bool,
    pub trading_active: bool,
}
