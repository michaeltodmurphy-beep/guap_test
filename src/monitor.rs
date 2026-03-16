use anyhow::Result;
use chrono::Utc;
use futures::future::join_all;
use std::collections::HashMap;

use crate::config::Config;
use crate::kalshi_api::KalshiApiClient;
use crate::models::{MarketSnapshot, MarketType};

/// Monitors active Kalshi crypto markets and returns snapshots.
pub struct MarketMonitor {
    api: KalshiApiClient,
    config: Config,
}

impl MarketMonitor {
    pub fn new(api: KalshiApiClient, config: Config) -> Self {
        MarketMonitor { api, config }
    }

    /// Discover active markets for the configured series tickers.
    /// Returns a map of market ticker → MarketSnapshot.
    pub async fn fetch_snapshots(&self) -> Result<HashMap<String, MarketSnapshot>> {
        let mut snapshots = HashMap::new();
        let t = &self.config.trading;

        // Build list of enabled series to fetch
        let series_to_fetch: Vec<&String> = t
            .market_series_tickers
            .iter()
            .filter(|series| {
                let market_type = self.series_to_type(series);
                match market_type {
                    Some(MarketType::Btc) if !t.enable_btc => false,
                    Some(MarketType::Eth) if !t.enable_eth => false,
                    _ => true,
                }
            })
            .collect();

        // Fetch all series in parallel
        let futures: Vec<_> = series_to_fetch
            .iter()
            .map(|series| self.api.list_series_markets(series))
            .collect();

        let results = join_all(futures).await;

        let now = Utc::now();

        for (series, result) in series_to_fetch.iter().zip(results) {
            let markets = match result {
                Ok(m) => m,
                Err(e) => {
                    log::warn!("Failed to list markets for series {series}: {e}");
                    continue;
                }
            };

            log::debug!(
                "Series {}: found {} markets, {} currently tradeable",
                series,
                markets.len(),
                markets
                    .iter()
                    .filter(|m| {
                        let close = m.close_time.or(m.expiration_time);
                        (m.status == "open" || m.status == "active")
                            && match (m.open_time, close) {
                                (Some(open), Some(close)) => now >= open && now < close,
                                (Some(open), None) => now >= open,
                                (None, Some(close)) => now < close,
                                (None, None) => true,
                            }
                    })
                    .count()
            );

            let mut series_had_snapshot = false;

            for market in markets {
                // A market is tradeable when status is open and current time is between open_time and close_time
                let close = market.close_time.or(market.expiration_time);
                let is_tradeable = (market.status == "open" || market.status == "active")
                    && match (market.open_time, close) {
                        (Some(open), Some(close)) => now >= open && now < close,
                        (Some(open), None) => now >= open,
                        (None, Some(close)) => now < close,
                        (None, None) => true,
                    };

                if !is_tradeable {
                    continue;
                }

                // When open_time is unknown, treat as "fully elapsed" so the min_elapsed check passes.
                let elapsed_seconds = market
                    .open_time
                    .map(|t| (now - t).num_seconds().max(0))
                    .unwrap_or(i64::MAX / 2);
                // When close_time is unknown, treat as "plenty of time remaining" so the min_remaining check passes.
                let remaining_seconds = close
                    .map(|t| (t - now).num_seconds().max(0))
                    .unwrap_or(i64::MAX / 2);

                log::debug!(
                    "Tradeable market: {} status={} yes_bid={:?} yes_ask={:?} last_price={:?} elapsed={}s remaining={}s",
                    market.ticker, market.status,
                    market.yes_bid(), market.yes_ask(), market.last_price(),
                    elapsed_seconds, remaining_seconds
                );

                let yes_ask = market.yes_ask();
                let yes_bid = market.yes_bid();

                let snapshot = MarketSnapshot {
                    ticker: market.ticker.clone(),
                    event_ticker: market.event_ticker.clone(),
                    title: market.title.clone(),
                    yes_ask,
                    yes_bid,
                    no_ask: market.no_ask(),
                    no_bid: market.no_bid(),
                    last_price: market.last_price(),
                    open_time: market.open_time,
                    close_time: close,
                    elapsed_seconds,
                    remaining_seconds,
                };

                snapshots.insert(market.ticker.clone(), snapshot);
                series_had_snapshot = true;
            }

            if !series_had_snapshot {
                log::debug!(
                    "Series {series}: no active market found (gap between contracts or not yet open)"
                );
            }
        }

        Ok(snapshots)
    }

    /// Map a series ticker to a MarketType.
    fn series_to_type(&self, series: &str) -> Option<MarketType> {
        let upper = series.to_uppercase();
        if upper.contains("BTC") || upper.contains("KXBTC") {
            Some(MarketType::Btc)
        } else if upper.contains("ETH") || upper.contains("KXETH") {
            Some(MarketType::Eth)
        } else {
            None
        }
    }

    /// Determine the MarketType for a market ticker, based on its event ticker / series.
    pub fn market_type_for(&self, event_ticker: &str) -> MarketType {
        let upper = event_ticker.to_uppercase();
        if upper.contains("ETH") {
            MarketType::Eth
        } else if upper.contains("BTC") {
            MarketType::Btc
        } else {
            log::warn!(
                "Unknown series type for event ticker '{}'; defaulting to BTC",
                event_ticker
            );
            MarketType::Btc
        }
    }
}
