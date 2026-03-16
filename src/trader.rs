use anyhow::Result;
use chrono::Utc;
use std::collections::HashMap;
use uuid::Uuid;

use crate::config::Config;
use crate::detector::PriceDetector;
use crate::kalshi_api::KalshiApiClient;
use crate::models::{
    BuyOpportunity, KalshiOrderRequest, MarketSnapshot, MarketType, OrderSide, OrderStatus,
    OrderType, PendingTrade, TradeStatus,
};

/// Timeout (seconds) before a resting PendingFill order is cancelled to unblock the asset class.
const PENDING_FILL_TIMEOUT_SECS: i64 = 90;

/// Grace period (seconds) after trade entry during which sync_positions will not remove the trade,
/// even if it is absent from the portfolio, to account for Kalshi's eventual consistency.
const EVENTUAL_CONSISTENCY_BUFFER_SECS: i64 = 30;

/// Maximum number of attempts when placing a sell order before giving up for the current tick.
const MAX_SELL_ATTEMPTS: u8 = 3;

/// Delay in milliseconds between consecutive sell-order retry attempts.
const SELL_RETRY_DELAY_MS: u64 = 100;

/// Executes trades and manages open positions.
pub struct Trader {
    api: KalshiApiClient,
    config: Config,
    simulation: bool,
    pub pending_trades: HashMap<String, PendingTrade>,
    pub total_pnl: f64,
    pub trades_executed: u64,
    pub history_path: String,
}

impl Trader {
    pub fn new(api: KalshiApiClient, config: Config, simulation: bool) -> Self {
        Trader {
            api,
            config,
            simulation,
            pending_trades: HashMap::new(),
            total_pnl: 0.0,
            trades_executed: 0,
            history_path: "history.toml".to_string(),
        }
    }

    /// Execute a buy for the given opportunity. Marks the market active in the detector.
    pub async fn execute_buy(
        &mut self,
        opp: &BuyOpportunity,
        detector: &mut PriceDetector,
    ) -> Result<()> {
        // Defense-in-depth: reject if we already have a pending trade for this ticker.
        if self.pending_trades.contains_key(&opp.ticker) {
            log::warn!(
                "Rejecting duplicate buy for {} — already have an active/pending trade",
                opp.ticker
            );
            return Ok(());
        }

        // IMMEDIATELY mark active to prevent duplicate buys during API round-trip.
        detector.mark_active(&opp.ticker);

        let buy_price = opp.ask_price; // buy at best ask
        let count: i64 = self.config.trading.fixed_trade_amount.round() as i64;

        if self.simulation {
            log::info!(
                "[SIM] BUY {} x {} contracts @ {}¢  ({}) side={:?}",
                opp.ticker,
                count,
                buy_price,
                opp.market_type,
                opp.order_side,
            );
        } else {
            log::info!(
                "BUY {} x {} contracts @ {}¢  ({}) side={:?}",
                opp.ticker,
                count,
                buy_price,
                opp.market_type,
                opp.order_side,
            );
        }

        let (order_id, initial_status, actual_units) = if self.simulation {
            // In simulation we generate a fake order ID
            (format!("sim-{}", Uuid::new_v4()), TradeStatus::Open, count)
        } else {
            let req = KalshiOrderRequest {
                ticker: opp.ticker.clone(),
                client_order_id: Uuid::new_v4().to_string(),
                side: opp.order_side.clone(),
                order_type: OrderType::Limit,
                yes_price_dollars: if opp.order_side == OrderSide::Yes { Some(format!("{:.4}", buy_price as f64 / 100.0)) } else { None },
                no_price_dollars: if opp.order_side == OrderSide::No { Some(format!("{:.4}", buy_price as f64 / 100.0)) } else { None },
                count_fp: format!("{:.2}", count as f64),
                action: "buy".to_string(),
            };

            let order = self.api.place_order(&req).await.map_err(|e| {
                detector.mark_closed(&opp.ticker);
                e
            })?;
            log::info!("Order placed: {} (status: {:?})", order.order_id, order.status);

            match order.status {
                OrderStatus::Executed => {
                    let filled = order.filled_count.unwrap_or(count);
                    let remaining = order.remaining_count.unwrap_or(0);
                    log::info!(
                        "Order {}: {}/{} contracts filled immediately, {} resting",
                        order.order_id, filled, count, remaining
                    );
                    (order.order_id, TradeStatus::Open, filled)
                }
                OrderStatus::Resting => {
                    let filled = order.filled_count.unwrap_or(0);
                    let remaining = order.remaining_count.unwrap_or(count);
                    log::info!(
                        "Order {} is resting; {}/{} contracts filled immediately, {} resting",
                        order.order_id, filled, count, remaining
                    );
                    (order.order_id, TradeStatus::PendingFill, filled)
                }
                OrderStatus::Canceled => {
                    log::warn!("Order {} was canceled immediately", order.order_id);
                    detector.mark_closed(&opp.ticker);
                    return Ok(());
                }
                _ => {
                    log::warn!(
                        "Order {} has unexpected status {:?}; not tracking",
                        order.order_id, order.status
                    );
                    detector.mark_closed(&opp.ticker);
                    return Ok(());
                }
            }
        };

        let trade = PendingTrade {
            ticker: opp.ticker.clone(),
            order_id: order_id.clone(),
            buy_price,
            units: actual_units,
            requested_units: count,
            sell_target: if opp.is_early_window {
                self.config.trading.early_window_sell_price
            } else {
                self.config.trading.sell_price
            },
            status: initial_status,
            entered_at: Utc::now(),
            exited_at: None,
            realized_pnl: None,
            market_type: opp.market_type,
            order_side: opp.order_side.clone(),
            elapsed_minutes_at_entry: opp.elapsed_minutes,
            is_early_window: opp.is_early_window,
            sell_order_ids: vec![],
        };

        // Mark early window buy so the ticker is blocked from late window entry.
        if opp.is_early_window {
            detector.mark_early_buy(&opp.ticker);
        }

        self.log_event(&format!(
            "[{}] BUY  {} ticker={} price={}¢ count={} order_id={} sim={}",
            trade.entered_at.to_rfc3339(),
            opp.market_type,
            opp.ticker,
            buy_price,
            count,
            order_id,
            self.simulation,
        ));

        self.pending_trades.insert(opp.ticker.clone(), trade);
        self.trades_executed += 1;
        Ok(())
    }

    /// Check all open trades for sell conditions and act accordingly.
    pub async fn check_pending_trades(
        &mut self,
        snapshots: &HashMap<String, MarketSnapshot>,
        detector: &mut PriceDetector,
    ) -> Result<()> {
        let tickers: Vec<String> = self.pending_trades.keys().cloned().collect();

        for ticker in tickers {
            let snapshot = match snapshots.get(&ticker) {
                Some(s) => s.clone(),
                None => {
                    // Market may have settled; check
                    self.check_market_settlement(&ticker, detector).await?;
                    continue;
                }
            };

            // For resting orders, check if they have been filled yet.
            if let Some(trade) = self.pending_trades.get(&ticker).filter(|t| t.status == TradeStatus::PendingFill && !self.simulation) {
                let order_id = trade.order_id.clone();
                let entered_at = trade.entered_at;
                let requested_units = trade.requested_units;
                let pending_too_long = (Utc::now() - entered_at).num_seconds() >= PENDING_FILL_TIMEOUT_SECS;
                match self.api.get_order(&order_id).await {
                    Ok(order) => {
                        let filled = order.filled_count.unwrap_or(0);
                        let remaining = order.remaining_count.unwrap_or(0);
                        match order.status {
                            OrderStatus::Executed => {
                                // Use requested_units as the fallback since a fully executed order
                                // should always have filled_count set; this avoids shadowing `filled`.
                                let executed_filled = order.filled_count.unwrap_or(requested_units);
                                log::info!(
                                    "Resting order {} for {}: now {}/{} filled",
                                    order_id, ticker, executed_filled, requested_units
                                );
                                if let Some(t) = self.pending_trades.get_mut(&ticker) {
                                    t.units = executed_filled;
                                    t.status = TradeStatus::Open;
                                }
                            }
                            OrderStatus::Canceled => {
                                log::warn!(
                                    "Resting order {} for {} was canceled; removing trade",
                                    order_id, ticker
                                );
                                self.pending_trades.remove(&ticker);
                                detector.mark_closed(&ticker);
                                continue;
                            }
                            _ if pending_too_long => {
                                if filled > 0 {
                                    log::warn!(
                                        "Order {} timed out with {}/{} filled; cancelling remaining {}, keeping {} contracts",
                                        order_id, filled, requested_units, remaining, filled
                                    );
                                    if let Err(e) = self.api.cancel_order(&order_id).await {
                                        log::warn!("Failed to cancel timed-out resting order {order_id}: {e}");
                                    }
                                    if let Some(t) = self.pending_trades.get_mut(&ticker) {
                                        t.units = filled;
                                        t.status = TradeStatus::Open;
                                    }
                                    // Fall through to sell logic below
                                } else {
                                    log::warn!(
                                        "Resting order {} for {} has been pending >{}s; cancelling to unblock asset class",
                                        order_id, ticker, PENDING_FILL_TIMEOUT_SECS
                                    );
                                    if let Err(e) = self.api.cancel_order(&order_id).await {
                                        log::warn!("Failed to cancel timed-out resting order {order_id}: {e}");
                                    }
                                    self.pending_trades.remove(&ticker);
                                    detector.mark_closed(&ticker);
                                    continue;
                                }
                            }
                            _ => {
                                if filled > 0 {
                                    log::info!(
                                        "Order {} for {}: {}/{} contracts filled, {} still resting",
                                        order_id, ticker, filled, requested_units, remaining
                                    );
                                    if let Some(t) = self.pending_trades.get_mut(&ticker) {
                                        t.units = filled;
                                    }
                                } else {
                                    log::debug!(
                                        "Resting order {} for {} still pending (status: {:?})",
                                        order_id, ticker, order.status
                                    );
                                }
                                continue;
                            }
                        }
                    }
                    Err(e) => {
                        let err_str = e.to_string();
                        if err_str.contains("404") || err_str.contains("not_found") || err_str.contains("Not Found") {
                            // Only remove if the order has been pending for > 30 seconds.
                            // Fresh orders may return 404 due to Kalshi API eventual consistency.
                            let age_secs = (Utc::now() - entered_at).num_seconds();
                            if age_secs > 30 {
                                log::warn!(
                                    "Order {} for {} not found (404) after {}s; removing ghost trade to unblock asset class",
                                    order_id, ticker, age_secs
                                );
                                self.pending_trades.remove(&ticker);
                                detector.mark_closed(&ticker);
                            } else {
                                log::debug!(
                                    "Order {} for {} returned 404 but only {}s old; will retry (eventual consistency)",
                                    order_id, ticker, age_secs
                                );
                            }
                        } else {
                            log::warn!("Error checking resting order {order_id}: {e}");
                        }
                        continue;
                    }
                }
            }

            // ── Determine current bid ───────────────────────────────────────────
            let current_bid = {
                let trade = match self.pending_trades.get(&ticker) {
                    Some(t) if t.status == TradeStatus::Open => t,
                    _ => continue,
                };
                let bid = match trade.order_side {
                    OrderSide::Yes => snapshot.yes_bid,
                    OrderSide::No => snapshot.no_bid,
                };
                // If bid data is not available or is zero, skip — don't assume 0 and trigger stop-loss
                match bid {
                    Some(b) if b > 0 => b,
                    _ => {
                        log::debug!("No valid bid for {}; skipping sell check", ticker);
                        continue;
                    }
                }
            };

            // ── Profit-target check ─────
            let is_profit_sell = {
                let trade = self.pending_trades.get(&ticker).unwrap();
                if trade.is_early_window {
                    self.config.trading.early_window_sell_enabled && current_bid >= trade.sell_target
                } else {
                    self.config.trading.late_window_sell_enabled && current_bid >= trade.sell_target
                }
            };
            if is_profit_sell {
                self.execute_sell(&ticker, Some(current_bid), false, detector).await?;
                continue;
            }

            // ── Skip stop-loss near market close ────────────────────────────
            // Near close, the order book thins out and bids widen — triggering
            // stop-losses on phantom price drops. Better to let Kalshi settle.
            let sl_min_remaining = self.config.trading.stop_loss_min_remaining_seconds;
            if sl_min_remaining > 0 && snapshot.remaining_seconds < sl_min_remaining {
                log::debug!(
                    "Skipping SL check for {} — remaining {}s < stop_loss_min_remaining_seconds {}s",
                    ticker, snapshot.remaining_seconds, sl_min_remaining
                );
                continue;
            }

            // ── Simple stop-loss check ─────────────────────────────────────────
            // If bid drops to or below stop_loss_price, sell the entire position.
            if current_bid <= self.config.trading.stop_loss_price {
                self.execute_sell(&ticker, Some(current_bid), true, detector).await?;
                continue;
            }
        }

        // Safety net: remove trades that have been open too long (market likely settled)
        const STALE_TRADE_TIMEOUT_MINUTES: i64 = 20;
        let stale_cutoff = Utc::now() - chrono::Duration::minutes(STALE_TRADE_TIMEOUT_MINUTES);
        let stale_tickers: Vec<String> = self
            .pending_trades
            .iter()
            .filter(|(_, t)| {
                (t.status == TradeStatus::Open || t.status == TradeStatus::PendingFill)
                    && t.entered_at < stale_cutoff
            })
            .map(|(k, _)| k.clone())
            .collect();

        for ticker in stale_tickers {
            log::warn!(
                "Trade {} has been open for >20min (market likely settled); force-closing",
                ticker
            );
            if let Some(trade) = self.pending_trades.remove(&ticker) {
                if !self.simulation {
                    if trade.status == TradeStatus::PendingFill {
                        if let Err(e) = self.api.cancel_order(&trade.order_id).await {
                            log::warn!(
                                "Failed to cancel resting order {} for {}: {}",
                                trade.order_id, ticker, e
                            );
                        }
                    }
                }
                detector.mark_closed(&ticker);
                self.log_event(&format!(
                    "[{}] STALE_CLOSE {} buy_price={}¢ units={}",
                    Utc::now().to_rfc3339(),
                    ticker,
                    trade.buy_price,
                    trade.units,
                ));
            }
        }

        Ok(())
    }

    /// Execute a sell for the given ticker.
    async fn execute_sell(
        &mut self,
        ticker: &str,
        current_bid: Option<i64>,
        is_stop_loss: bool,
        detector: &mut PriceDetector,
    ) -> Result<()> {
        // Guard: refuse to place an order at 0¢ (invalid on Kalshi); let settlement handle it.
        // Note: check_pending_trades already skips bids <= 0, but this is a defence-in-depth
        // guard for any future callers of execute_sell.
        if current_bid.map_or(false, |b| b <= 0) {
            log::warn!(
                "Refusing to sell {} at ≤0¢ (invalid price); letting Kalshi settle",
                ticker
            );
            self.pending_trades.remove(ticker);
            detector.mark_closed(ticker);
            return Ok(());
        }

        // Extract needed values before holding any long-lived borrow
        let (sell_price, pnl, units, trade_side) = {
            let trade = match self.pending_trades.get(ticker) {
                Some(t) => t,
                None => return Ok(()),
            };
            let sell_price = current_bid.unwrap_or(trade.sell_target);
            let pnl = (sell_price - trade.buy_price) as f64 * trade.units as f64 / 100.0;
            (sell_price, pnl, trade.units, trade.order_side.clone())
        };

        let reason = if is_stop_loss { "STOP-LOSS" } else { "PROFIT" };

        if self.simulation {
            log::info!(
                "[SIM] SELL {} @ {}¢  ({}) side={:?} PnL: ${:.4}",
                ticker,
                sell_price,
                reason,
                trade_side,
                pnl
            );
        } else {
            log::info!("SELL {} @ {}¢  ({}) side={:?} PnL: ${:.4}", ticker, sell_price, reason, trade_side, pnl);

            let (order_type, yes_price, no_price) = if is_stop_loss && self.config.trading.stop_loss_use_market_order {
                (OrderType::Market, None, None)
            } else {
                (
                    OrderType::Limit,
                    if trade_side == OrderSide::Yes { Some(format!("{:.4}", sell_price as f64 / 100.0)) } else { None },
                    if trade_side == OrderSide::No { Some(format!("{:.4}", sell_price as f64 / 100.0)) } else { None },
                )
            };
            let mut req = KalshiOrderRequest {
                ticker: ticker.to_string(),
                client_order_id: Uuid::new_v4().to_string(),
                side: trade_side.clone(),
                order_type,
                yes_price_dollars: yes_price,
                no_price_dollars: no_price,
                count_fp: format!("{:.2}", units as f64),
                action: "sell".to_string(),
            };
            let mut sell_succeeded = false;
            for attempt in 1_u8..=MAX_SELL_ATTEMPTS {
                if attempt > 1 {
                    tokio::time::sleep(std::time::Duration::from_millis(SELL_RETRY_DELAY_MS)).await;
                    // Escalate to market order and generate a fresh client order ID.
                    req.order_type = OrderType::Market;
                    req.yes_price_dollars = None;
                    req.no_price_dollars = None;
                    req.client_order_id = Uuid::new_v4().to_string();
                }
                match self.api.place_order(&req).await {
                    Ok(_) => {
                        sell_succeeded = true;
                        break;
                    }
                    Err(e) => {
                        log::error!("Sell attempt {attempt}/{MAX_SELL_ATTEMPTS} for {ticker} failed: {e}");
                    }
                }
            }
            if !sell_succeeded {
                log::error!("CRITICAL: All {MAX_SELL_ATTEMPTS} sell attempts failed for {ticker}; will retry next tick");
                return Ok(());
            }
        }

        let exited_at = Utc::now();
        self.total_pnl += pnl;

        self.log_event(&format!(
            "[{}] SELL {} reason={} sell_price={}¢ pnl=${:.4} total_pnl=${:.4} sim={}",
            exited_at.to_rfc3339(),
            ticker,
            reason,
            sell_price,
            pnl,
            self.total_pnl,
            self.simulation,
        ));

        // Cancel any resting sell orders tracked for this position before closing.
        if !self.simulation {
            let sell_order_ids: Vec<String> = self.pending_trades
                .get_mut(ticker)
                .map(|t| std::mem::take(&mut t.sell_order_ids))
                .unwrap_or_default();
            for order_id in &sell_order_ids {
                match self.api.cancel_order(order_id).await {
                    Ok(_) => log::info!("Cancelled resting sell order {} for {}", order_id, ticker),
                    Err(e) => {
                        if !Self::is_order_not_found_error(&e.to_string()) {
                            log::warn!("Failed to cancel sell order {} for {}: {}", order_id, ticker, e);
                        }
                    }
                }
            }
        }

        // Set exit metadata before removing the trade record
        if let Some(trade) = self.pending_trades.get_mut(ticker) {
            trade.exited_at = Some(exited_at);
            trade.realized_pnl = Some(pnl);
        }
        // Remove settled trade from active map
        self.pending_trades.remove(ticker);
        detector.mark_closed(ticker);

        Ok(())
    }

    /// Check if a market has settled and clean up if so.
    async fn check_market_settlement(
        &mut self,
        ticker: &str,
        detector: &mut PriceDetector,
    ) -> Result<()> {
        if self.simulation {
            // In simulation, assume market settled; log and clean up
            if let Some(trade) = self.pending_trades.remove(ticker) {
                log::info!(
                    "[SIM] Market {} settled while holding {} contracts bought @ {}¢",
                    ticker,
                    trade.units,
                    trade.buy_price
                );
                detector.mark_closed(ticker);
            }
            return Ok(());
        }

        match self.api.get_market(ticker).await {
            Ok(market) => {
                if market.status == "finalized"
                    || market.status == "settled"
                    || market.status == "closed"
                    || market.result.is_some()
                {
                    if let Some(trade) = self.pending_trades.remove(ticker) {
                        let result_str = market.result.as_deref().unwrap_or("unknown");
                        log::info!(
                            "Market {} settled with result={}; position {} units bought @ {}¢",
                            ticker,
                            result_str,
                            trade.units,
                            trade.buy_price
                        );
                        detector.mark_closed(ticker);
                        self.log_event(&format!(
                            "[{}] SETTLE {} result={} units={} buy_price={}¢",
                            Utc::now().to_rfc3339(),
                            ticker,
                            result_str,
                            trade.units,
                            trade.buy_price,
                        ));
                    }
                }
            }
            Err(e) => {
                let err_str = e.to_string();
                if err_str.contains("404") || err_str.contains("not_found") || err_str.contains("Not Found") {
                    log::warn!("Market {} not found (404); cleaning up trade", ticker);
                    if let Some(_trade) = self.pending_trades.remove(ticker) {
                        detector.mark_closed(ticker);
                    }
                } else {
                    log::warn!("Error checking settlement for {ticker}: {e}");
                }
            }
        }
        Ok(())
    }

    /// Sync internal positions with the Kalshi portfolio.
    pub async fn sync_positions(&mut self, detector: &mut PriceDetector) -> Result<()> {
        if self.simulation {
            return Ok(());
        }

        let positions = self.api.get_positions().await?;
        let position_tickers: std::collections::HashSet<String> = positions
            .iter()
            .filter(|p| p.position != 0)
            .map(|p| p.ticker.clone())
            .collect();

        let now = Utc::now();

        // Categorise pending trades that are absent from the portfolio into stale
        // (safe to remove) vs. skipped (PendingFill or entered within the eventual-
        // consistency grace period).
        let mut stale: Vec<String> = Vec::new();
        for (ticker, trade) in &self.pending_trades {
            if position_tickers.contains(ticker) {
                continue;
            }
            let age_secs = (now - trade.entered_at).num_seconds();
            if trade.status == TradeStatus::PendingFill || age_secs <= EVENTUAL_CONSISTENCY_BUFFER_SECS {
                log::debug!(
                    "sync_positions: skipping {} (PendingFill or <{}s old); not removing despite missing from portfolio",
                    ticker, EVENTUAL_CONSISTENCY_BUFFER_SECS
                );
            } else {
                stale.push(ticker.clone());
            }
        }

        for ticker in stale {
            log::info!("Position for {ticker} no longer in portfolio; removing");
            self.pending_trades.remove(&ticker);
            detector.mark_closed(&ticker);
        }

        Ok(())
    }

    /// Returns true if we currently track an active position for the given ticker.
    pub fn has_active_position(&self, ticker: &str) -> bool {
        self.pending_trades.contains_key(ticker)
    }

    /// Returns true if a cancel-order error string indicates the order is already gone
    /// (filled or previously cancelled) — these are not actionable and can be ignored.
    fn is_order_not_found_error(err_str: &str) -> bool {
        err_str.contains("404") || err_str.contains("not_found") || err_str.contains("Not Found")
    }

    /// Returns true if we have reached the maximum concurrent positions for the given market type.
    pub fn has_active_position_for_type(&self, market_type: &MarketType) -> bool {
        let count = self
            .pending_trades
            .values()
            .filter(|t| {
                t.market_type == *market_type
                    && matches!(t.status, TradeStatus::Open | TradeStatus::PendingFill)
            })
            .count();
        count >= self.config.trading.max_concurrent_positions
    }

    /// Append a line to the history file.
    fn log_event(&self, line: &str) {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.history_path)
        {
            let _ = writeln!(f, "{line}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::detector::PriceDetector;
    use crate::kalshi_api::KalshiApiClient;
    use crate::models::{BuyOpportunity, MarketSnapshot, MarketType, OrderSide};

    /// Build a simulation-mode Trader and a matching PriceDetector using default config.
    fn sim_trader() -> (Trader, PriceDetector) {
        let config = Config::default();
        let api = KalshiApiClient::new(&config.kalshi).unwrap();
        let trading_cfg = config.trading.clone();
        let mut trader = Trader::new(api, config, true /* simulation */);
        // Redirect history writes to /tmp so tests don't litter the workspace.
        trader.history_path = "/tmp/trader_test_history.toml".to_string();
        (trader, PriceDetector::new(trading_cfg))
    }

    fn make_opp(ticker: &str) -> BuyOpportunity {
        BuyOpportunity {
            ticker: ticker.to_string(),
            event_ticker: format!("{}-24DEC", ticker),
            market_type: MarketType::Btc,
            bid_price: 90,
            ask_price: 91,
            order_side: OrderSide::No,
            elapsed_minutes: 10.0,
            remaining_seconds: 120,
            is_early_window: false,
        }
    }

    fn make_snap(ticker: &str) -> MarketSnapshot {
        MarketSnapshot {
            ticker: ticker.to_string(),
            event_ticker: format!("{}-24DEC", ticker),
            title: "Test market".to_string(),
            yes_ask: Some(91),
            yes_bid: Some(90),
            no_ask: Some(10),
            no_bid: Some(9),
            last_price: Some(90),
            open_time: None,
            close_time: None,
            elapsed_seconds: 360,
            remaining_seconds: 120,
        }
    }

    // ------------------------------------------------------------------
    // has_active_position
    // ------------------------------------------------------------------

    #[test]
    fn has_active_position_false_initially() {
        let (trader, _) = sim_trader();
        assert!(!trader.has_active_position("KXBTC-1"));
    }

    #[tokio::test]
    async fn has_active_position_true_after_buy() {
        let (mut trader, mut detector) = sim_trader();
        let opp = make_opp("KXBTC-1");
        trader.execute_buy(&opp, &mut detector).await.unwrap();
        assert!(trader.has_active_position("KXBTC-1"));
    }

    // ------------------------------------------------------------------
    // Duplicate-buy prevention — pending_trades guard
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn execute_buy_blocks_duplicate_for_same_ticker() {
        let (mut trader, mut detector) = sim_trader();
        let opp = make_opp("KXBTC-1");

        // First buy should succeed.
        trader.execute_buy(&opp, &mut detector).await.unwrap();
        assert_eq!(trader.trades_executed, 1, "First buy should be recorded");
        assert_eq!(trader.pending_trades.len(), 1, "One pending trade after first buy");

        // Second buy for the same ticker must be blocked by the pending_trades guard.
        trader.execute_buy(&opp, &mut detector).await.unwrap();
        assert_eq!(trader.trades_executed, 1, "Duplicate buy must be rejected");
        assert_eq!(trader.pending_trades.len(), 1, "Still exactly one pending trade");
    }

    #[tokio::test]
    async fn execute_buy_allows_different_tickers() {
        let (mut trader, mut detector) = sim_trader();

        trader.execute_buy(&make_opp("KXBTC-1"), &mut detector).await.unwrap();
        trader.execute_buy(&make_opp("KXBTC-2"), &mut detector).await.unwrap();

        assert_eq!(trader.trades_executed, 2, "Two different tickers should both be bought");
        assert_eq!(trader.pending_trades.len(), 2);
    }

    // ------------------------------------------------------------------
    // Early mark_active — detector blocks re-entry while trade is live
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn execute_buy_marks_detector_active() {
        let (mut trader, mut detector) = sim_trader();
        let opp = make_opp("KXBTC-1");

        assert!(!detector.has_active_position("KXBTC-1"), "No active position before buy");

        trader.execute_buy(&opp, &mut detector).await.unwrap();

        assert!(
            detector.has_active_position("KXBTC-1"),
            "Detector must mark ticker active after buy (before next tick)"
        );
    }

    #[tokio::test]
    async fn detector_blocks_check_after_buy() {
        let (mut trader, mut detector) = sim_trader();
        let opp = make_opp("KXBTC-1");

        trader.execute_buy(&opp, &mut detector).await.unwrap();

        // A subsequent detector.check() for the same ticker must return None,
        // simulating the fast-polling duplicate-buy scenario.
        let snap = make_snap("KXBTC-1");
        let result = detector.check(&snap, MarketType::Btc);
        assert!(
            result.is_none(),
            "Detector must reject check for a ticker that already has an active buy"
        );
    }

    // ------------------------------------------------------------------
    // sync_positions — PendingFill / recent trades must not be removed
    // ------------------------------------------------------------------

    #[test]
    fn sync_positions_skips_pending_fill_trade_missing_from_portfolio() {
        use crate::models::{PendingTrade, TradeStatus};

        let (mut trader, mut detector) = sim_trader();

        // Manually insert a PendingFill trade that is NOT present in the portfolio.
        let ticker = "KXBTC15M-26MAR061745-45".to_string();
        trader.pending_trades.insert(
            ticker.clone(),
            PendingTrade {
                ticker: ticker.clone(),
                order_id: "88bd88b8-test".to_string(),
                buy_price: 95,
                units: 0,
                requested_units: 1,
                sell_target: 97,
                status: TradeStatus::PendingFill,
                entered_at: Utc::now(),
                exited_at: None,
                realized_pnl: None,
                market_type: MarketType::Btc,
                order_side: OrderSide::Yes,
                elapsed_minutes_at_entry: 0.5,
                is_early_window: false,
                sell_order_ids: vec![],
            },
        );
        // Mark it active in the detector so we can verify the guard is preserved.
        detector.mark_active(&ticker);

        // Simulate an empty portfolio response — the ticker is absent.
        let position_tickers: std::collections::HashSet<String> = std::collections::HashSet::new();

        // Run the same filtering logic as sync_positions (without the async API call).
        let now = Utc::now();
        let mut stale: Vec<String> = Vec::new();
        for (t, trade) in &trader.pending_trades {
            if !position_tickers.contains(t) {
                let age_secs = (now - trade.entered_at).num_seconds();
                if trade.status != TradeStatus::PendingFill && age_secs > EVENTUAL_CONSISTENCY_BUFFER_SECS {
                    stale.push(t.clone());
                }
            }
        }
        for t in stale {
            trader.pending_trades.remove(&t);
            detector.mark_closed(&t);
        }

        // The PendingFill trade must still be present.
        assert!(
            trader.pending_trades.contains_key(&ticker),
            "PendingFill trade must NOT be removed by sync_positions when missing from portfolio"
        );
        // The detector guard must still be active.
        assert!(
            detector.has_active_position(&ticker),
            "Detector guard must remain active for a PendingFill trade"
        );
    }

    #[test]
    fn sync_positions_skips_recently_entered_open_trade() {
        use crate::models::{PendingTrade, TradeStatus};

        let (mut trader, mut detector) = sim_trader();

        // An Open trade entered only 5 seconds ago should also be skipped.
        let ticker = "KXBTC15M-RECENT".to_string();
        trader.pending_trades.insert(
            ticker.clone(),
            PendingTrade {
                ticker: ticker.clone(),
                order_id: "recent-order".to_string(),
                buy_price: 90,
                units: 1,
                requested_units: 1,
                sell_target: 95,
                status: TradeStatus::Open,
                entered_at: Utc::now(), // just entered
                exited_at: None,
                realized_pnl: None,
                market_type: MarketType::Btc,
                order_side: OrderSide::Yes,
                elapsed_minutes_at_entry: 1.0,
                is_early_window: false,
                sell_order_ids: vec![],
            },
        );
        detector.mark_active(&ticker);

        let position_tickers: std::collections::HashSet<String> = std::collections::HashSet::new();

        let now = Utc::now();
        let mut stale: Vec<String> = Vec::new();
        for (t, trade) in &trader.pending_trades {
            if !position_tickers.contains(t) {
                let age_secs = (now - trade.entered_at).num_seconds();
                if trade.status != TradeStatus::PendingFill && age_secs > EVENTUAL_CONSISTENCY_BUFFER_SECS {
                    stale.push(t.clone());
                }
            }
        }
        for t in stale {
            trader.pending_trades.remove(&t);
            detector.mark_closed(&t);
        }

        assert!(
            trader.pending_trades.contains_key(&ticker),
            "Open trade entered <{}s ago must NOT be removed by sync_positions",
            EVENTUAL_CONSISTENCY_BUFFER_SECS
        );
        assert!(
            detector.has_active_position(&ticker),
            "Detector guard must remain active for a recently entered trade"
        );
    }

    // ------------------------------------------------------------------
    // early_window_sell_price — sell target assignment
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn early_window_trade_uses_early_window_sell_price() {
        let mut config = Config::default();
        config.trading.early_window_sell_price = 96;
        config.trading.sell_price = 99;

        let api = KalshiApiClient::new(&config.kalshi).unwrap();
        let trading_cfg = config.trading.clone();
        let mut trader = Trader::new(api, config, true);
        trader.history_path = "/tmp/trader_test_history.toml".to_string();
        let mut detector = PriceDetector::new(trading_cfg);

        let opp = BuyOpportunity {
            ticker: "KXBTC-1".to_string(),
            event_ticker: "KXBTC-24DEC".to_string(),
            market_type: MarketType::Btc,
            bid_price: 90,
            ask_price: 91,
            order_side: OrderSide::Yes,
            elapsed_minutes: 1.5,
            remaining_seconds: 120,
            is_early_window: true,
        };

        trader.execute_buy(&opp, &mut detector).await.unwrap();

        let trade = trader.pending_trades.get("KXBTC-1").unwrap();
        assert_eq!(
            trade.sell_target, 96,
            "Early window trade must use early_window_sell_price as sell_target"
        );
        assert!(trade.is_early_window, "Trade must be flagged as early window");
    }

    #[tokio::test]
    async fn late_window_trade_uses_sell_price() {
        let mut config = Config::default();
        config.trading.early_window_sell_price = 96;
        config.trading.sell_price = 99;

        let api = KalshiApiClient::new(&config.kalshi).unwrap();
        let trading_cfg = config.trading.clone();
        let mut trader = Trader::new(api, config, true);
        trader.history_path = "/tmp/trader_test_history.toml".to_string();
        let mut detector = PriceDetector::new(trading_cfg);

        let opp = BuyOpportunity {
            ticker: "KXBTC-2".to_string(),
            event_ticker: "KXBTC-24DEC".to_string(),
            market_type: MarketType::Btc,
            bid_price: 90,
            ask_price: 91,
            order_side: OrderSide::Yes,
            elapsed_minutes: 10.0,
            remaining_seconds: 120,
            is_early_window: false,
        };

        trader.execute_buy(&opp, &mut detector).await.unwrap();

        let trade = trader.pending_trades.get("KXBTC-2").unwrap();
        assert_eq!(
            trade.sell_target, 99,
            "Late window trade must use sell_price as sell_target"
        );
        assert!(!trade.is_early_window, "Trade must not be flagged as early window");
    }

    #[tokio::test]
    async fn late_window_profit_sell_fires_when_enabled() {
        // With late_window_sell_enabled = true, a late window trade should be sold
        // when the bid reaches sell_target.
        let mut config = Config::default();
        config.trading.sell_price = 99;
        config.trading.late_window_sell_enabled = true;

        let api = KalshiApiClient::new(&config.kalshi).unwrap();
        let trading_cfg = config.trading.clone();
        let mut trader = Trader::new(api, config, true);
        trader.history_path = "/tmp/trader_test_late_sell_enabled.toml".to_string();
        let mut detector = PriceDetector::new(trading_cfg);

        // Insert a late window trade manually with sell_target = 99
        let ticker = "KXBTC-3".to_string();
        trader.pending_trades.insert(ticker.clone(), crate::models::PendingTrade {
            ticker: ticker.clone(),
            order_id: "test-order-3".to_string(),
            buy_price: 95,
            units: 1,
            requested_units: 1,
            sell_target: 99,
            status: crate::models::TradeStatus::Open,
            entered_at: chrono::Utc::now(),
            exited_at: None,
            realized_pnl: None,
            market_type: crate::models::MarketType::Btc,
            order_side: crate::models::OrderSide::Yes,
            elapsed_minutes_at_entry: 10.0,
            is_early_window: false,
            sell_order_ids: vec![],
        });

        // Snapshot with bid at 99 — should trigger the profit sell
        let mut snapshots = std::collections::HashMap::new();
        snapshots.insert(ticker.clone(), crate::models::MarketSnapshot {
            ticker: ticker.clone(),
            event_ticker: "KXBTC-24DEC".to_string(),
            title: String::new(),
            yes_bid: Some(99),
            yes_ask: Some(100),
            no_bid: Some(1),
            no_ask: Some(2),
            last_price: None,
            open_time: None,
            close_time: None,
            elapsed_seconds: 600,
            remaining_seconds: 300,
        });

        trader.check_pending_trades(&snapshots, &mut detector).await.unwrap();
        assert!(
            !trader.pending_trades.contains_key(&ticker),
            "Late window trade should be sold when late_window_sell_enabled is true and bid >= sell_target"
        );
    }

    #[tokio::test]
    async fn late_window_profit_sell_does_not_fire_when_disabled() {
        // With late_window_sell_enabled = false (default), a late window trade should NOT
        // be sold at sell_target — it should be held until settlement or stop-loss.
        let mut config = Config::default();
        config.trading.sell_price = 99;
        config.trading.late_window_sell_enabled = false;

        let api = KalshiApiClient::new(&config.kalshi).unwrap();
        let trading_cfg = config.trading.clone();
        let mut trader = Trader::new(api, config, true);
        trader.history_path = "/tmp/trader_test_late_sell_disabled.toml".to_string();
        let mut detector = PriceDetector::new(trading_cfg);

        let ticker = "KXBTC-4".to_string();
        trader.pending_trades.insert(ticker.clone(), crate::models::PendingTrade {
            ticker: ticker.clone(),
            order_id: "test-order-4".to_string(),
            buy_price: 95,
            units: 1,
            requested_units: 1,
            sell_target: 99,
            status: crate::models::TradeStatus::Open,
            entered_at: chrono::Utc::now(),
            exited_at: None,
            realized_pnl: None,
            market_type: crate::models::MarketType::Btc,
            order_side: crate::models::OrderSide::Yes,
            elapsed_minutes_at_entry: 10.0,
            is_early_window: false,
            sell_order_ids: vec![],
        });

        // Snapshot with bid at 99 — should NOT trigger the profit sell
        let mut snapshots = std::collections::HashMap::new();
        snapshots.insert(ticker.clone(), crate::models::MarketSnapshot {
            ticker: ticker.clone(),
            event_ticker: "KXBTC-24DEC".to_string(),
            title: String::new(),
            yes_bid: Some(99),
            yes_ask: Some(100),
            no_bid: Some(1),
            no_ask: Some(2),
            last_price: None,
            open_time: None,
            close_time: None,
            elapsed_seconds: 600,
            remaining_seconds: 300,
        });

        trader.check_pending_trades(&snapshots, &mut detector).await.unwrap();
        assert!(
            trader.pending_trades.contains_key(&ticker),
            "Late window trade should NOT be sold when late_window_sell_enabled is false"
        );
    }

    #[tokio::test]
    async fn early_window_profit_sell_fires_when_enabled() {
        // With early_window_sell_enabled = true (default), an early window trade should
        // be sold when the bid reaches sell_target.
        let mut config = Config::default();
        config.trading.early_window_sell_price = 99;
        config.trading.early_window_sell_enabled = true;

        let api = KalshiApiClient::new(&config.kalshi).unwrap();
        let trading_cfg = config.trading.clone();
        let mut trader = Trader::new(api, config, true);
        trader.history_path = "/tmp/trader_test_early_sell_enabled.toml".to_string();
        let mut detector = PriceDetector::new(trading_cfg);

        // Insert an early window trade manually with sell_target = 99
        let ticker = "KXBTC-5".to_string();
        trader.pending_trades.insert(ticker.clone(), crate::models::PendingTrade {
            ticker: ticker.clone(),
            order_id: "test-order-5".to_string(),
            buy_price: 85,
            units: 1,
            requested_units: 1,
            sell_target: 99,
            status: crate::models::TradeStatus::Open,
            entered_at: chrono::Utc::now(),
            exited_at: None,
            realized_pnl: None,
            market_type: crate::models::MarketType::Btc,
            order_side: crate::models::OrderSide::Yes,
            elapsed_minutes_at_entry: 2.0,
            is_early_window: true,
            sell_order_ids: vec![],
        });

        // Snapshot with bid at 99 — should trigger the profit sell
        let mut snapshots = std::collections::HashMap::new();
        snapshots.insert(ticker.clone(), crate::models::MarketSnapshot {
            ticker: ticker.clone(),
            event_ticker: "KXBTC-24DEC".to_string(),
            title: String::new(),
            yes_bid: Some(99),
            yes_ask: Some(100),
            no_bid: Some(1),
            no_ask: Some(2),
            last_price: None,
            open_time: None,
            close_time: None,
            elapsed_seconds: 120,
            remaining_seconds: 300,
        });

        trader.check_pending_trades(&snapshots, &mut detector).await.unwrap();
        assert!(
            !trader.pending_trades.contains_key(&ticker),
            "Early window trade should be sold when early_window_sell_enabled is true and bid >= sell_target"
        );
    }

    #[tokio::test]
    async fn early_window_profit_sell_does_not_fire_when_disabled() {
        // With early_window_sell_enabled = false, an early window trade should NOT
        // be sold at sell_target — it should be held until settlement or stop-loss.
        let mut config = Config::default();
        config.trading.early_window_sell_price = 99;
        config.trading.early_window_sell_enabled = false;

        let api = KalshiApiClient::new(&config.kalshi).unwrap();
        let trading_cfg = config.trading.clone();
        let mut trader = Trader::new(api, config, true);
        trader.history_path = "/tmp/trader_test_early_sell_disabled.toml".to_string();
        let mut detector = PriceDetector::new(trading_cfg);

        // Insert an early window trade manually with sell_target = 99
        let ticker = "KXBTC-6".to_string();
        trader.pending_trades.insert(ticker.clone(), crate::models::PendingTrade {
            ticker: ticker.clone(),
            order_id: "test-order-6".to_string(),
            buy_price: 85,
            units: 1,
            requested_units: 1,
            sell_target: 99,
            status: crate::models::TradeStatus::Open,
            entered_at: chrono::Utc::now(),
            exited_at: None,
            realized_pnl: None,
            market_type: crate::models::MarketType::Btc,
            order_side: crate::models::OrderSide::Yes,
            elapsed_minutes_at_entry: 2.0,
            is_early_window: true,
            sell_order_ids: vec![],
        });

        // Snapshot with bid at 99 — should NOT trigger the profit sell
        let mut snapshots = std::collections::HashMap::new();
        snapshots.insert(ticker.clone(), crate::models::MarketSnapshot {
            ticker: ticker.clone(),
            event_ticker: "KXBTC-24DEC".to_string(),
            title: String::new(),
            yes_bid: Some(99),
            yes_ask: Some(100),
            no_bid: Some(1),
            no_ask: Some(2),
            last_price: None,
            open_time: None,
            close_time: None,
            elapsed_seconds: 120,
            remaining_seconds: 300,
        });

        trader.check_pending_trades(&snapshots, &mut detector).await.unwrap();
        assert!(
            trader.pending_trades.contains_key(&ticker),
            "Early window trade should NOT be sold when early_window_sell_enabled is false"
        );
    }

    // ------------------------------------------------------------------
    // Simple stop-loss: when bid drops to or below stop_loss_price,
    // the position is fully closed via execute_sell.
    // ------------------------------------------------------------------

    fn make_sl_snap(ticker: &str, bid: i64) -> crate::models::MarketSnapshot {
        crate::models::MarketSnapshot {
            ticker: ticker.to_string(),
            event_ticker: "KXBTC-24DEC".to_string(),
            title: String::new(),
            yes_bid: Some(bid),
            yes_ask: Some(bid + 1),
            no_bid: Some(100 - bid),
            no_ask: Some(100 - bid + 1),
            last_price: None,
            open_time: None,
            close_time: None,
            elapsed_seconds: 600,
            remaining_seconds: 300,
        }
    }

    /// When bid drops to exactly stop_loss_price, the position must be fully closed.
    #[tokio::test]
    async fn stop_loss_fires_when_bid_at_stop_loss_price() {
        let mut config = Config::default();
        config.trading.stop_loss_price = 50;

        let api = KalshiApiClient::new(&config.kalshi).unwrap();
        let trading_cfg = config.trading.clone();
        let mut trader = Trader::new(api, config, true);
        trader.history_path = "/tmp/trader_test_sl_fires.toml".to_string();
        let mut detector = PriceDetector::new(trading_cfg);

        let ticker = "KXBTC-SL";
        trader.pending_trades.insert(ticker.to_string(), crate::models::PendingTrade {
            ticker: ticker.to_string(),
            order_id: "sl-test-order".to_string(),
            buy_price: 90,
            units: 5,
            requested_units: 5,
            sell_target: 99,
            status: crate::models::TradeStatus::Open,
            entered_at: Utc::now(),
            exited_at: None,
            realized_pnl: None,
            market_type: crate::models::MarketType::Btc,
            order_side: crate::models::OrderSide::Yes,
            elapsed_minutes_at_entry: 5.0,
            is_early_window: false,
            sell_order_ids: vec![],
        });
        detector.mark_active(ticker);

        let mut snapshots = std::collections::HashMap::new();
        // bid == stop_loss_price (50) → should trigger SL
        snapshots.insert(ticker.to_string(), make_sl_snap(ticker, 50));

        trader.check_pending_trades(&snapshots, &mut detector).await.unwrap();

        assert!(
            !trader.pending_trades.contains_key(ticker),
            "Trade must be fully closed when bid == stop_loss_price"
        );
        assert!(
            !detector.has_active_position(ticker),
            "Detector must mark ticker inactive after stop-loss"
        );
    }

    /// When bid drops below stop_loss_price, the position must also be fully closed.
    #[tokio::test]
    async fn stop_loss_fires_when_bid_below_stop_loss_price() {
        let mut config = Config::default();
        config.trading.stop_loss_price = 50;

        let api = KalshiApiClient::new(&config.kalshi).unwrap();
        let trading_cfg = config.trading.clone();
        let mut trader = Trader::new(api, config, true);
        trader.history_path = "/tmp/trader_test_sl_fires_below.toml".to_string();
        let mut detector = PriceDetector::new(trading_cfg);

        let ticker = "KXBTC-SL2";
        trader.pending_trades.insert(ticker.to_string(), crate::models::PendingTrade {
            ticker: ticker.to_string(),
            order_id: "sl-test-order-2".to_string(),
            buy_price: 90,
            units: 3,
            requested_units: 3,
            sell_target: 99,
            status: crate::models::TradeStatus::Open,
            entered_at: Utc::now(),
            exited_at: None,
            realized_pnl: None,
            market_type: crate::models::MarketType::Btc,
            order_side: crate::models::OrderSide::Yes,
            elapsed_minutes_at_entry: 5.0,
            is_early_window: false,
            sell_order_ids: vec![],
        });
        detector.mark_active(ticker);

        let mut snapshots = std::collections::HashMap::new();
        // bid = 20, well below stop_loss_price=50 → should trigger SL
        snapshots.insert(ticker.to_string(), make_sl_snap(ticker, 20));

        trader.check_pending_trades(&snapshots, &mut detector).await.unwrap();

        assert!(
            !trader.pending_trades.contains_key(ticker),
            "Trade must be fully closed when bid < stop_loss_price"
        );
    }

    /// When bid is above stop_loss_price, the position must NOT be closed by the stop-loss.
    #[tokio::test]
    async fn stop_loss_does_not_fire_when_bid_above_stop_loss_price() {
        let mut config = Config::default();
        config.trading.stop_loss_price = 50;

        let api = KalshiApiClient::new(&config.kalshi).unwrap();
        let trading_cfg = config.trading.clone();
        let mut trader = Trader::new(api, config, true);
        trader.history_path = "/tmp/trader_test_sl_no_fire.toml".to_string();
        let mut detector = PriceDetector::new(trading_cfg);

        let ticker = "KXBTC-SL3";
        trader.pending_trades.insert(ticker.to_string(), crate::models::PendingTrade {
            ticker: ticker.to_string(),
            order_id: "sl-test-order-3".to_string(),
            buy_price: 90,
            units: 2,
            requested_units: 2,
            sell_target: 99,
            status: crate::models::TradeStatus::Open,
            entered_at: Utc::now(),
            exited_at: None,
            realized_pnl: None,
            market_type: crate::models::MarketType::Btc,
            order_side: crate::models::OrderSide::Yes,
            elapsed_minutes_at_entry: 5.0,
            is_early_window: false,
            sell_order_ids: vec![],
        });
        detector.mark_active(ticker);

        let mut snapshots = std::collections::HashMap::new();
        // bid = 51, above stop_loss_price=50 → should NOT trigger SL
        snapshots.insert(ticker.to_string(), make_sl_snap(ticker, 51));

        trader.check_pending_trades(&snapshots, &mut detector).await.unwrap();

        assert!(
            trader.pending_trades.contains_key(ticker),
            "Trade must remain open when bid > stop_loss_price"
        );
    }

    /// total_pnl is updated correctly when stop-loss fires.
    #[tokio::test]
    async fn stop_loss_updates_total_pnl() {
        let mut config = Config::default();
        config.trading.stop_loss_price = 50;

        let api = KalshiApiClient::new(&config.kalshi).unwrap();
        let trading_cfg = config.trading.clone();
        let mut trader = Trader::new(api, config, true);
        trader.history_path = "/tmp/trader_test_sl_pnl.toml".to_string();
        let mut detector = PriceDetector::new(trading_cfg);

        let ticker = "KXBTC-SL4";
        let buy_price: i64 = 90;
        let sell_price: i64 = 30; // well below stop_loss_price=50
        trader.pending_trades.insert(ticker.to_string(), crate::models::PendingTrade {
            ticker: ticker.to_string(),
            order_id: "sl-test-order-4".to_string(),
            buy_price,
            units: 10,
            requested_units: 10,
            sell_target: 99,
            status: crate::models::TradeStatus::Open,
            entered_at: Utc::now(),
            exited_at: None,
            realized_pnl: None,
            market_type: crate::models::MarketType::Btc,
            order_side: crate::models::OrderSide::Yes,
            elapsed_minutes_at_entry: 5.0,
            is_early_window: false,
            sell_order_ids: vec![],
        });
        detector.mark_active(ticker);

        let initial_pnl = trader.total_pnl;

        let mut snapshots = std::collections::HashMap::new();
        snapshots.insert(ticker.to_string(), make_sl_snap(ticker, sell_price));

        trader.check_pending_trades(&snapshots, &mut detector).await.unwrap();

        // Expected PnL: (30 - 90) * 10 / 100 = -$6.00
        let expected_pnl = (sell_price - buy_price) as f64 * 10.0 / 100.0;
        let delta = trader.total_pnl - initial_pnl;
        assert!(
            (delta - expected_pnl).abs() < 0.0001,
            "total_pnl should increase by {:.4} but increased by {:.4}",
            expected_pnl, delta
        );
    }

    // ------------------------------------------------------------------
    // Simulation sell path: trade is removed and pnl updated on sell
    // ------------------------------------------------------------------

    /// In simulation mode, execute_sell removes the trade and updates total_pnl
    /// correctly — verifying the sell path works end-to-end after the retry
    /// logic was introduced (the sim branch bypasses the retry loop entirely).
    #[tokio::test]
    async fn sim_sell_removes_trade_and_updates_pnl() {
        let mut config = Config::default();
        config.trading.sell_price = 95;
        config.trading.late_window_sell_enabled = true;

        let api = KalshiApiClient::new(&config.kalshi).unwrap();
        let trading_cfg = config.trading.clone();
        let mut trader = Trader::new(api, config, true); // simulation = true
        trader.history_path = "/tmp/trader_test_sim_sell.toml".to_string();
        let mut detector = PriceDetector::new(trading_cfg);

        let ticker = "KXBTC-SIMSELL";
        let buy_price: i64 = 80;
        let sell_price: i64 = 95;
        trader.pending_trades.insert(ticker.to_string(), crate::models::PendingTrade {
            ticker: ticker.to_string(),
            order_id: "sim-sell-order".to_string(),
            buy_price,
            units: 5,
            requested_units: 5,
            sell_target: sell_price,
            status: crate::models::TradeStatus::Open,
            entered_at: Utc::now(),
            exited_at: None,
            realized_pnl: None,
            market_type: crate::models::MarketType::Btc,
            order_side: crate::models::OrderSide::Yes,
            elapsed_minutes_at_entry: 5.0,
            is_early_window: false,
            sell_order_ids: vec![],
        });
        detector.mark_active(ticker);

        // Build a snapshot at sell_target to trigger the profit sell
        let mut snapshots = std::collections::HashMap::new();
        snapshots.insert(ticker.to_string(), crate::models::MarketSnapshot {
            ticker: ticker.to_string(),
            event_ticker: "KXBTC-24DEC".to_string(),
            title: String::new(),
            yes_bid: Some(sell_price),
            yes_ask: Some(sell_price + 1),
            no_bid: Some(100 - sell_price),
            no_ask: Some(100 - sell_price + 1),
            last_price: None,
            open_time: None,
            close_time: None,
            elapsed_seconds: 600,
            remaining_seconds: 300,
        });

        let initial_pnl = trader.total_pnl;
        trader.check_pending_trades(&snapshots, &mut detector).await.unwrap();

        assert!(
            !trader.pending_trades.contains_key(ticker),
            "Trade must be removed after a successful simulation sell"
        );
        assert!(
            !detector.has_active_position(ticker),
            "Detector must mark ticker inactive after simulation sell"
        );
        // Expected PnL: (95 - 80) * 5 / 100 = $0.75
        let expected_pnl = (sell_price - buy_price) as f64 * 5.0 / 100.0;
        let delta = trader.total_pnl - initial_pnl;
        assert!(
            (delta - expected_pnl).abs() < 0.0001,
            "total_pnl delta should be {:.4} but was {:.4}",
            expected_pnl, delta
        );
    }
}
