use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio::sync::RwLock;
use tokio_tungstenite::{connect_async_tls_with_config, tungstenite::Message};
use url::Url;

use crate::config::Config;
use crate::kalshi_api::KalshiApiClient;
use crate::models::{MarketSnapshot, WsMessage};

/// Sentinel value used for elapsed/remaining when the corresponding timestamp is unknown.
/// We use MAX/2 (not MAX) to avoid overflow when arithmetic is performed on the value.
const UNKNOWN_TIME_SECONDS: i64 = i64::MAX / 2;

/// Maximum number of bytes from a raw WebSocket message to include in log output.
const MAX_LOG_MSG_BYTES: usize = 500;

/// Background WebSocket monitor that keeps a shared map of MarketSnapshots up to date.
pub struct WsMarketMonitor {
    api: KalshiApiClient,
    config: Config,
    snapshots: Arc<RwLock<HashMap<String, MarketSnapshot>>>,
}

impl WsMarketMonitor {
    pub fn new(api: KalshiApiClient, config: Config) -> Self {
        WsMarketMonitor {
            api,
            config,
            snapshots: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Spawns the background WebSocket task. Call once at startup.
    pub async fn start(&self) {
        let api = self.api.clone();
        let config = self.config.clone();
        let snapshots = Arc::clone(&self.snapshots);
        tokio::spawn(async move {
            run_ws_loop(api, config, snapshots).await;
        });
    }

    /// Read current snapshots (instant, no API call).
    /// Recomputes elapsed/remaining time from stored open_time/close_time.
    pub async fn get_snapshots(&self) -> HashMap<String, MarketSnapshot> {
        let guard = self.snapshots.read().await;
        let now = Utc::now();
        guard
            .iter()
            .map(|(k, s)| {
                let elapsed_seconds = s
                    .open_time
                    .map(|t| (now - t).num_seconds().max(0))
                    .unwrap_or(UNKNOWN_TIME_SECONDS);
                let remaining_seconds = s
                    .close_time
                    .map(|t| (t - now).num_seconds().max(0))
                    .unwrap_or(UNKNOWN_TIME_SECONDS);
                let mut snapshot = s.clone();
                snapshot.elapsed_seconds = elapsed_seconds;
                snapshot.remaining_seconds = remaining_seconds;
                (k.clone(), snapshot)
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Background WebSocket loop
// ---------------------------------------------------------------------------

async fn run_ws_loop(
    api: KalshiApiClient,
    config: Config,
    snapshots: Arc<RwLock<HashMap<String, MarketSnapshot>>>,
) {
    let ws_url = match build_ws_url(api.base_url()) {
        Ok(u) => u,
        Err(e) => {
            log::error!("WsMonitor: failed to build WebSocket URL: {e}");
            return;
        }
    };

    let mut backoff_secs: u64 = 1;
    let mut msg_id: i64 = 0;

    loop {
        log::info!("WsMonitor: connecting to {ws_url}");

        let headers = match api.ws_auth_headers() {
            Ok(h) => h,
            Err(e) => {
                // No credentials — skip WebSocket entirely
                log::warn!("WsMonitor: cannot generate auth headers ({e}); retrying in {backoff_secs}s");
                tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                backoff_secs = (backoff_secs * 2).min(30);
                continue;
            }
        };

        let (api_key, timestamp_ms, signature) = headers;

        let request = {
            use tokio_tungstenite::tungstenite::client::IntoClientRequest;
            use tokio_tungstenite::tungstenite::http::HeaderValue;

            // into_client_request() automatically adds all required WebSocket headers:
            // Sec-WebSocket-Key, Sec-WebSocket-Version, Connection: Upgrade, Upgrade: websocket, Host
            let mut request = match ws_url.as_str().into_client_request() {
                Ok(r) => r,
                Err(e) => {
                    log::error!("WsMonitor: failed to build WS request: {e}");
                    tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                    backoff_secs = (backoff_secs * 2).min(30);
                    continue;
                }
            };

            // Add Kalshi authentication headers on top of the auto-generated WS headers
            let headers = request.headers_mut();
            match HeaderValue::from_str(&api_key) {
                Ok(v) => { headers.insert("KALSHI-ACCESS-KEY", v); }
                Err(e) => {
                    log::warn!("WsMonitor: invalid api_key header value: {e}; retrying in {backoff_secs}s");
                    tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                    backoff_secs = (backoff_secs * 2).min(30);
                    continue;
                }
            }
            match HeaderValue::from_str(&timestamp_ms) {
                Ok(v) => { headers.insert("KALSHI-ACCESS-TIMESTAMP", v); }
                Err(e) => {
                    log::warn!("WsMonitor: invalid timestamp header value: {e}; retrying in {backoff_secs}s");
                    tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                    backoff_secs = (backoff_secs * 2).min(30);
                    continue;
                }
            }
            match HeaderValue::from_str(&signature) {
                Ok(v) => { headers.insert("KALSHI-ACCESS-SIGNATURE", v); }
                Err(e) => {
                    log::warn!("WsMonitor: invalid signature header value: {e}; retrying in {backoff_secs}s");
                    tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                    backoff_secs = (backoff_secs * 2).min(30);
                    continue;
                }
            }

            request
        };

        match connect_async_tls_with_config(request, None, false, None).await {
            Ok((ws_stream, _)) => {
                log::info!("WsMonitor: WebSocket connected");
                backoff_secs = 1;

                let (mut write, mut read) = ws_stream.split();

                // Discover active markets and build initial snapshots from REST
                let tickers = discover_and_init(&api, &config, &snapshots).await;

                if !tickers.is_empty() {
                    msg_id += 1;
                    let sub_msg = json!({
                        "id": msg_id,
                        "cmd": "subscribe",
                        "params": {
                            "channels": ["ticker"],
                            "market_tickers": tickers
                        }
                    });
                    if let Err(e) = write.send(Message::Text(sub_msg.to_string().into())).await {
                        log::warn!("WsMonitor: failed to send subscription: {e}");
                    }
                }

                let mut last_discovery = tokio::time::Instant::now();
                let discovery_interval = Duration::from_secs(60);
                let mut subscribed: HashSet<String> = tickers.into_iter().collect();

                // Message loop
                loop {
                    // Periodically re-discover new markets
                    if last_discovery.elapsed() >= discovery_interval {
                        let new_tickers = discover_and_init(&api, &config, &snapshots).await;
                        let new_set: HashSet<String> = new_tickers.into_iter().collect();
                        let to_subscribe: Vec<String> =
                            new_set.difference(&subscribed).cloned().collect();
                        if !to_subscribe.is_empty() {
                            log::info!(
                                "WsMonitor: subscribing to {} new ticker(s)",
                                to_subscribe.len()
                            );
                            msg_id += 1;
                            let sub_msg = json!({
                                "id": msg_id,
                                "cmd": "subscribe",
                                "params": {
                                    "channels": ["ticker"],
                                    "market_tickers": to_subscribe
                                }
                            });
                            if let Err(e) =
                                write.send(Message::Text(sub_msg.to_string().into())).await
                            {
                                log::warn!("WsMonitor: failed to send subscription: {e}");
                                break;
                            }
                            subscribed.extend(new_set);
                        }
                        last_discovery = tokio::time::Instant::now();
                    }

                    // Wait up to 5s for the next message so we can check the discovery timer
                    let msg = tokio::time::timeout(Duration::from_secs(5), read.next()).await;

                    match msg {
                        Ok(Some(Ok(Message::Text(text)))) => {
                            handle_text_message(&text, &snapshots, &mut write).await;
                        }
                        Ok(Some(Ok(Message::Ping(data)))) => {
                            if let Err(e) = write.send(Message::Pong(data)).await {
                                log::warn!("WsMonitor: failed to send pong: {e}");
                                break;
                            }
                        }
                        Ok(Some(Ok(Message::Close(_)))) => {
                            log::warn!("WsMonitor: server closed connection");
                            break;
                        }
                        Ok(Some(Err(e))) => {
                            log::warn!("WsMonitor: WebSocket error: {e}");
                            break;
                        }
                        Ok(None) => {
                            log::warn!("WsMonitor: stream ended");
                            break;
                        }
                        Err(_) => {
                            // Timeout — loop back to check the discovery timer
                        }
                        Ok(Some(Ok(_))) => {
                            // Binary / pong frames — ignore
                        }
                    }
                }

                log::warn!(
                    "WsMonitor: disconnected; reconnecting in {backoff_secs}s"
                );
            }
            Err(e) => {
                log::warn!("WsMonitor: connection failed: {e}; retrying in {backoff_secs}s");
            }
        }

        tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
        backoff_secs = (backoff_secs * 2).min(30);
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Derive the WebSocket URL from the REST base URL.
fn build_ws_url(base_url: &str) -> Result<String> {
    let parsed = Url::parse(base_url)?;
    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("No host in base_url '{base_url}'"))?;
    Ok(format!("wss://{host}/trade-api/ws/v2"))
}

/// Call REST to discover active markets for all configured series.
/// Initialises snapshot entries (with metadata) if not already present.
/// Returns the list of market tickers to subscribe to.
async fn discover_and_init(
    api: &KalshiApiClient,
    config: &Config,
    snapshots: &Arc<RwLock<HashMap<String, MarketSnapshot>>>,
) -> Vec<String> {
    let mut tickers = Vec::new();
    let now = Utc::now();
    let t = &config.trading;

    let series_to_fetch: Vec<&String> = t.market_series_tickers.iter().filter(|series| {
        let upper = series.to_uppercase();
        if (upper.contains("BTC") || upper.contains("KXBTC")) && !t.enable_btc {
            log::debug!("WsMonitor: skipping series {} (disabled in config)", series);
            return false;
        }
        if (upper.contains("ETH") || upper.contains("KXETH")) && !t.enable_eth {
            log::debug!("WsMonitor: skipping series {} (disabled in config)", series);
            return false;
        }
        true
    }).collect();

    for series in series_to_fetch {
        match api.list_series_markets(series).await {
            Err(e) => log::warn!("WsMonitor: failed to list markets for {series}: {e}"),
            Ok(markets) => {
                for market in markets {
                    let close = market.close_time.or(market.expiration_time);
                    let is_active = (market.status == "open" || market.status == "active")
                        && match (market.open_time, close) {
                            (Some(open), Some(close)) => now >= open && now < close,
                            (Some(open), None) => now >= open,
                            (None, Some(close)) => now < close,
                            (None, None) => true,
                        };
                    if !is_active {
                        continue;
                    }

                    tickers.push(market.ticker.clone());

                    // Only initialise once; don't overwrite price data from WS
                    let mut guard = snapshots.write().await;
                    guard.entry(market.ticker.clone()).or_insert_with(|| {
                        let elapsed_seconds = market
                            .open_time
                            .map(|t| (now - t).num_seconds().max(0))
                            .unwrap_or(UNKNOWN_TIME_SECONDS);
                        let remaining_seconds = close
                            .map(|t| (t - now).num_seconds().max(0))
                            .unwrap_or(UNKNOWN_TIME_SECONDS);
                        MarketSnapshot {
                            ticker: market.ticker.clone(),
                            event_ticker: market.event_ticker.clone(),
                            title: market.title.clone(),
                            yes_ask: market.yes_ask(),
                            yes_bid: market.yes_bid(),
                            no_ask: market.no_ask(),
                            no_bid: market.no_bid(),
                            last_price: market.last_price(),
                            open_time: market.open_time,
                            close_time: close,
                            elapsed_seconds,
                            remaining_seconds,
                        }
                    });
                }
            }
        }
    }

    tickers
}

/// Parse a text WebSocket message and update the snapshot map.
async fn handle_text_message<S>(
    text: &str,
    snapshots: &Arc<RwLock<HashMap<String, MarketSnapshot>>>,
    write: &mut S,
) where
    S: SinkExt<Message> + Unpin,
    <S as futures_util::Sink<Message>>::Error: std::fmt::Display,
{
    let ws_msg: WsMessage = match serde_json::from_str(text) {
        Ok(m) => m,
        Err(e) => {
            log::debug!("WsMonitor: failed to parse message: {e}. Raw: {}", &text[..text.len().min(MAX_LOG_MSG_BYTES)]);
            return;
        }
    };

    // Handle application-level ping
    if ws_msg.msg_type.as_deref() == Some("ping") {
        let pong = json!({"type": "pong"});
        if let Err(e) = write.send(Message::Text(pong.to_string().into())).await {
            log::warn!("WsMonitor: failed to send application pong: {e}");
        }
        return;
    }

    // Process ticker updates
    if ws_msg.msg_type.as_deref() == Some("ticker") {
        if let Some(data) = &ws_msg.msg {
            if let Some(ticker) = &data.market_ticker {
                let mut guard = snapshots.write().await;
                if let Some(snapshot) = guard.get_mut(ticker) {
                    if let Some(v) = data.yes_bid() {
                        snapshot.yes_bid = Some(v);
                    }
                    if let Some(v) = data.yes_ask() {
                        snapshot.yes_ask = Some(v);
                    }
                    if let Some(v) = data.no_bid() {
                        snapshot.no_bid = Some(v);
                    }
                    if let Some(v) = data.no_ask() {
                        snapshot.no_ask = Some(v);
                    }
                    if let Some(v) = data.last_price() {
                        snapshot.last_price = Some(v);
                    }
                    // If the WS ticker message didn't include last_price,
                    // infer it from yes_bid (which is what the Kalshi website shows as the current price)
                    if data.last_price_dollars.is_none() {
                        snapshot.last_price = data.yes_bid();
                    }
                    // Always derive No-side prices from live Yes-side prices.
                    // Kalshi's WS ticker channel only sends yes_bid/yes_ask;
                    // no_bid/no_ask are complementary: no_bid = 100 - yes_ask, no_ask = 100 - yes_bid.
                    if let Some(yes_ask) = snapshot.yes_ask {
                        snapshot.no_bid = Some(100_i64.saturating_sub(yes_ask));
                    }
                    if let Some(yes_bid) = snapshot.yes_bid {
                        snapshot.no_ask = Some(100_i64.saturating_sub(yes_bid));
                    }
                    log::debug!(
                        "WsMonitor: ticker update {} yes_bid={:?} yes_ask={:?} no_bid={:?} no_ask={:?}",
                        ticker,
                        snapshot.yes_bid,
                        snapshot.yes_ask,
                        snapshot.no_bid,
                        snapshot.no_ask
                    );
                }
            }
        }
    }
}
