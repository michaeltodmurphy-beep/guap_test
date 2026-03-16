use std::collections::{HashMap, HashSet, VecDeque};

use chrono::{DateTime, Utc};

use crate::config::TradingConfig;
use crate::models::{BuyOpportunity, MarketSnapshot, MarketType, OrderSide};

/// Counts of how many markets were rejected for each reason during a reporting window.
#[derive(Debug, Default, Clone)]
pub struct RejectionCounts {
    pub active_position: u64,
    pub missing_price: u64,
    pub bid_out_of_range: u64,
    pub ask_too_high: u64,
    pub insufficient_elapsed: u64,
    pub insufficient_remaining: u64,
    pub momentum_rejected: u64,
}

/// Detects momentum-based buy opportunities on Kalshi markets.
pub struct PriceDetector {
    cfg: TradingConfig,
    /// Tickers for which we already hold a position (or have placed a buy order).
    active_tickers: HashSet<String>,
    /// Tickers that bought in the early window and should be blocked from late window buying.
    early_window_bought: HashSet<String>,
    /// Accumulated rejection counts since the last call to `take_rejection_summary`.
    pub rejection_counts: RejectionCounts,
    /// Total markets evaluated since last summary reset.
    pub markets_checked: u64,
    /// Sliding window of recent Yes-side bid prices per ticker for momentum confirmation.
    /// Each entry is (timestamp, bid_price_cents).
    yes_bid_history: HashMap<String, VecDeque<(DateTime<Utc>, i64)>>,
    /// Sliding window of recent No-side bid prices per ticker for momentum confirmation.
    no_bid_history: HashMap<String, VecDeque<(DateTime<Utc>, i64)>>,
}

impl PriceDetector {
    pub fn new(cfg: TradingConfig) -> Self {
        PriceDetector {
            cfg,
            active_tickers: HashSet::new(),
            early_window_bought: HashSet::new(),
            rejection_counts: RejectionCounts::default(),
            markets_checked: 0,
            yes_bid_history: HashMap::new(),
            no_bid_history: HashMap::new(),
        }
    }

    /// Check a single market snapshot for a buy opportunity.
    ///
    /// Returns `Some(BuyOpportunity)` if all entry conditions are met.
    pub fn check(
        &mut self,
        snapshot: &MarketSnapshot,
        market_type: MarketType,
    ) -> Option<BuyOpportunity> {
        let ticker = &snapshot.ticker;
        self.markets_checked += 1;

        let elapsed = snapshot.elapsed_minutes();

        // 1. Skip if we already have an active position
        if self.active_tickers.contains(ticker) {
            log::debug!("Detector rejected {}: already has active position", ticker);
            self.rejection_counts.active_position += 1;
            return None;
        }

        // Record current bid prices into the sliding window for momentum tracking.
        // We do this on every check() call (regardless of other gates) so the window is
        // warm by the time we reach the late-window entry decision.
        {
            let now_ts = Utc::now();
            let window_secs = self.cfg.momentum_window_seconds as i64;
            if let Some(yb) = snapshot.yes_bid {
                let hist = self.yes_bid_history.entry(ticker.clone()).or_default();
                hist.push_back((now_ts, yb));
                while hist.front().map_or(false, |&(ts, _)| (now_ts - ts).num_seconds() > window_secs) {
                    hist.pop_front();
                }
            }
            if let Some(nb) = snapshot.no_bid {
                let hist = self.no_bid_history.entry(ticker.clone()).or_default();
                hist.push_back((now_ts, nb));
                while hist.front().map_or(false, |&(ts, _)| (now_ts - ts).num_seconds() > window_secs) {
                    hist.pop_front();
                }
            }
        }

        // 2. Early window check (new path)
        if self.cfg.early_window_enabled
            && elapsed >= self.cfg.early_window_start_minutes
            && elapsed < self.cfg.early_window_end_minutes
            && !self.early_window_bought.contains(ticker)
        {
            // Remaining time must still be sufficient
            if snapshot.remaining_seconds >= self.cfg.min_time_remaining_seconds {
                let ew_trigger = self.cfg.early_window_trigger_price;
                let ew_max = self.cfg.early_window_max_buy_price;

                // Try Yes side
                if let (Some(yes_bid), Some(yes_ask)) = (snapshot.yes_bid, snapshot.yes_ask) {
                    if yes_bid >= ew_trigger && yes_bid <= ew_max && yes_ask <= ew_max {
                        log::info!(
                            "Detector APPROVED {} (early window): bid={}¢ ask={}¢",
                            ticker, yes_bid, yes_ask
                        );
                        return Some(BuyOpportunity {
                            ticker: ticker.clone(),
                            event_ticker: snapshot.event_ticker.clone(),
                            market_type,
                            bid_price: yes_bid,
                            ask_price: yes_ask,
                            order_side: OrderSide::Yes,
                            elapsed_minutes: elapsed,
                            remaining_seconds: snapshot.remaining_seconds,
                            is_early_window: true,
                        });
                    }
                }

                // Try No side
                if let (Some(no_bid), Some(no_ask)) = (snapshot.no_bid, snapshot.no_ask) {
                    if no_bid >= ew_trigger && no_bid <= ew_max && no_ask <= ew_max {
                        log::info!(
                            "Detector APPROVED {} (early window): bid={}¢ ask={}¢",
                            ticker, no_bid, no_ask
                        );
                        return Some(BuyOpportunity {
                            ticker: ticker.clone(),
                            event_ticker: snapshot.event_ticker.clone(),
                            market_type,
                            bid_price: no_bid,
                            ask_price: no_ask,
                            order_side: OrderSide::No,
                            elapsed_minutes: elapsed,
                            remaining_seconds: snapshot.remaining_seconds,
                            is_early_window: true,
                        });
                    }
                }
            }

            // Early window active but no qualifying entry — reject and skip late window
            // (elapsed < early_window_end <= min_elapsed_minutes, so late window would also fail)
            if snapshot.yes_bid.is_none() && snapshot.no_bid.is_none() {
                log::debug!("Detector rejected {}: no yes_bid or no_bid available (early window)", ticker);
                self.rejection_counts.missing_price += 1;
            } else if snapshot.remaining_seconds < self.cfg.min_time_remaining_seconds {
                log::debug!(
                    "Detector rejected {}: remaining {}s < required {}s (early window)",
                    ticker,
                    snapshot.remaining_seconds,
                    self.cfg.min_time_remaining_seconds
                );
                self.rejection_counts.insufficient_remaining += 1;
            } else {
                let yes_bid = snapshot.yes_bid.unwrap_or(0);
                let yes_ask = snapshot.yes_ask.unwrap_or(0);
                let no_bid = snapshot.no_bid.unwrap_or(0);
                let no_ask = snapshot.no_ask.unwrap_or(0);
                let ew_trigger = self.cfg.early_window_trigger_price;
                let ew_max = self.cfg.early_window_max_buy_price;
                if (yes_bid >= ew_trigger && yes_bid <= ew_max && yes_ask > ew_max)
                    || (no_bid >= ew_trigger && no_bid <= ew_max && no_ask > ew_max)
                {
                    self.rejection_counts.ask_too_high += 1;
                    log::debug!(
                        "Detector rejected {}: bid in early window range but ask too high (yes: bid={}¢ ask={}¢, no: bid={}¢ ask={}¢)",
                        ticker, yes_bid, yes_ask, no_bid, no_ask
                    );
                } else {
                    self.rejection_counts.bid_out_of_range += 1;
                    log::debug!(
                        "Detector rejected {}: both sides out of early window range (yes_bid={}¢ no_bid={}¢, range=[{}, {}])",
                        ticker, yes_bid, no_bid, ew_trigger, ew_max
                    );
                }
            }
            return None;
        }

        // 3. Late window: enough time must have elapsed since market open
        if elapsed < self.cfg.min_elapsed_minutes {
            log::debug!(
                "Detector rejected {}: elapsed {:.1}min < required {:.1}min",
                ticker,
                elapsed,
                self.cfg.min_elapsed_minutes
            );
            self.rejection_counts.insufficient_elapsed += 1;
            return None;
        }

        // 4. Blocked from late window due to an early window buy
        if self.is_blocked_for_late_window(ticker) {
            log::debug!("Detector rejected {}: blocked by early window buy", ticker);
            self.rejection_counts.active_position += 1;
            return None;
        }

        // 5. Enough time must remain before market close.
        if snapshot.remaining_seconds < self.cfg.min_time_remaining_seconds {
            log::debug!(
                "Detector rejected {}: remaining {}s < required {}s",
                ticker,
                snapshot.remaining_seconds,
                self.cfg.min_time_remaining_seconds
            );
            self.rejection_counts.insufficient_remaining += 1;
            return None;
        }

        // 6. Try Yes side first: yes_bid must be in [trigger_price, max_buy_price] and yes_ask <= max_buy_price.
        let mut momentum_blocked = false;
        if let (Some(yes_bid), Some(yes_ask)) = (snapshot.yes_bid, snapshot.yes_ask) {
            if yes_bid >= self.cfg.trigger_price
                && yes_bid <= self.cfg.max_buy_price
                && yes_ask <= self.cfg.max_buy_price
            {
                // Momentum check for late-window entries.
                let yes_momentum_ok = if self.cfg.momentum_enabled {
                    match self.yes_bid_history.get(ticker.as_str()) {
                        Some(hist) if !self.momentum_ok(hist, yes_bid) => {
                            log::debug!(
                                "Detector rejected {} (momentum): yes_bid={}¢ below sliding avg + min_trend={}",
                                ticker, yes_bid, self.cfg.momentum_min_trend
                            );
                            momentum_blocked = true;
                            false
                        }
                        _ => true,
                    }
                } else {
                    true
                };

                if yes_momentum_ok {
                    log::info!(
                        "Detector APPROVED {} (Yes side): bid={}¢ ask={}¢",
                        ticker, yes_bid, yes_ask
                    );
                    return Some(BuyOpportunity {
                        ticker: ticker.clone(),
                        event_ticker: snapshot.event_ticker.clone(),
                        market_type,
                        bid_price: yes_bid,
                        ask_price: yes_ask,
                        order_side: OrderSide::Yes,
                        elapsed_minutes: elapsed,
                        remaining_seconds: snapshot.remaining_seconds,
                        is_early_window: false,
                    });
                }
            }
        }

        // 7. Try No side: no_bid must be in [trigger_price, max_buy_price] and no_ask <= max_buy_price.
        if let (Some(no_bid), Some(no_ask)) = (snapshot.no_bid, snapshot.no_ask) {
            if no_bid >= self.cfg.trigger_price
                && no_bid <= self.cfg.max_buy_price
                && no_ask <= self.cfg.max_buy_price
            {
                // Momentum check for late-window entries.
                let no_momentum_ok = if self.cfg.momentum_enabled {
                    match self.no_bid_history.get(ticker.as_str()) {
                        Some(hist) if !self.momentum_ok(hist, no_bid) => {
                            log::debug!(
                                "Detector rejected {} (momentum): no_bid={}¢ below sliding avg + min_trend={}",
                                ticker, no_bid, self.cfg.momentum_min_trend
                            );
                            momentum_blocked = true;
                            false
                        }
                        _ => true,
                    }
                } else {
                    true
                };

                if no_momentum_ok {
                    log::info!(
                        "Detector APPROVED {} (No side): bid={}¢ ask={}¢",
                        ticker, no_bid, no_ask
                    );
                    return Some(BuyOpportunity {
                        ticker: ticker.clone(),
                        event_ticker: snapshot.event_ticker.clone(),
                        market_type,
                        bid_price: no_bid,
                        ask_price: no_ask,
                        order_side: OrderSide::No,
                        elapsed_minutes: elapsed,
                        remaining_seconds: snapshot.remaining_seconds,
                        is_early_window: false,
                    });
                }
            }
        }

        // If momentum blocked at least one side that was otherwise in range, count it and stop.
        if momentum_blocked {
            self.rejection_counts.momentum_rejected += 1;
            return None;
        }

        // 8. Neither side qualified — log rejection with appropriate reason.
        if snapshot.yes_bid.is_none() && snapshot.no_bid.is_none() {
            log::debug!("Detector rejected {}: no yes_bid or no_bid available", ticker);
            self.rejection_counts.missing_price += 1;
            return None;
        }

        let yes_bid = snapshot.yes_bid.unwrap_or(0);
        let yes_ask = snapshot.yes_ask.unwrap_or(0);
        let no_bid = snapshot.no_bid.unwrap_or(0);
        let no_ask = snapshot.no_ask.unwrap_or(0);

        if (yes_bid >= self.cfg.trigger_price
            && yes_bid <= self.cfg.max_buy_price
            && yes_ask > self.cfg.max_buy_price)
            || (no_bid >= self.cfg.trigger_price
                && no_bid <= self.cfg.max_buy_price
                && no_ask > self.cfg.max_buy_price)
        {
            self.rejection_counts.ask_too_high += 1;
            log::debug!(
                "Detector rejected {}: bid in range but ask too high (yes: bid={}¢ ask={}¢, no: bid={}¢ ask={}¢)",
                ticker, yes_bid, yes_ask, no_bid, no_ask
            );
        } else {
            self.rejection_counts.bid_out_of_range += 1;
            log::debug!(
                "Detector rejected {}: both sides out of range (yes_bid={}¢ no_bid={}¢, range=[{}, {}])",
                ticker, yes_bid, no_bid, self.cfg.trigger_price, self.cfg.max_buy_price
            );
        }
        None
    }

    /// Mark a ticker as having an active position.
    pub fn mark_active(&mut self, ticker: &str) {
        self.active_tickers.insert(ticker.to_string());
    }

    /// Remove a ticker from the active set (position closed).
    pub fn mark_closed(&mut self, ticker: &str) {
        self.active_tickers.remove(ticker);
    }

    /// Mark a ticker as having been bought in the early window.
    pub fn mark_early_buy(&mut self, ticker: &str) {
        self.early_window_bought.insert(ticker.to_string());
    }

    /// Check if a ticker is blocked from late window buying due to an early window buy.
    pub fn is_blocked_for_late_window(&self, ticker: &str) -> bool {
        self.early_window_bought.contains(ticker)
    }

    /// Returns true if there is an active position for this ticker.
    pub fn has_active_position(&self, ticker: &str) -> bool {
        self.active_tickers.contains(ticker)
    }

    /// Returns `true` when `current_bid` satisfies the momentum threshold against `history`.
    ///
    /// Allows entry when:
    /// * the window is empty (no history yet — cold start), or
    /// * `current_bid >= avg(window) + cfg.momentum_min_trend`
    fn momentum_ok(&self, history: &VecDeque<(DateTime<Utc>, i64)>, current_bid: i64) -> bool {
        if history.is_empty() {
            return true;
        }
        let sum: i64 = history.iter().map(|(_, v)| *v).sum();
        let avg = sum / history.len() as i64;
        current_bid >= avg + self.cfg.momentum_min_trend
    }

    /// Returns a snapshot of rejection counts and resets the counters.
    /// Also resets `markets_checked`. Call this periodically to log a summary.
    pub fn take_rejection_summary(&mut self) -> (u64, RejectionCounts) {
        let checked = self.markets_checked;
        let counts = self.rejection_counts.clone();
        self.markets_checked = 0;
        self.rejection_counts = RejectionCounts::default();
        (checked, counts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TradingConfig;
    use crate::models::MarketSnapshot;

    fn default_cfg() -> TradingConfig {
        TradingConfig {
            check_interval_ms: 1000,
            trigger_price: 87,
            min_elapsed_minutes: 5.0,
            sell_price: 99,
            max_buy_price: 95,
            min_time_remaining_seconds: 30,
            market_series_tickers: vec!["KXBTC".to_string()],
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
            momentum_enabled: false,
            momentum_window_seconds: 10,
            momentum_min_trend: 0,
            late_window_sell_enabled: false,
            stop_loss_min_remaining_seconds: 0,
            stop_loss_price: 50,
            stop_loss_use_market_order: true,
        }
    }

    fn make_snapshot(ticker: &str, bid: i64, ask: i64, elapsed_s: i64, remaining_s: i64) -> MarketSnapshot {
        MarketSnapshot {
            ticker: ticker.to_string(),
            event_ticker: "KXBTC-24DEC".to_string(),
            title: "BTC above 50k?".to_string(),
            yes_ask: Some(ask),
            yes_bid: Some(bid),
            no_ask: Some(100 - bid),
            no_bid: Some(100 - ask),
            last_price: Some(bid),  // Use bid as a reasonable proxy for last_price in tests
            open_time: None,
            close_time: None,
            elapsed_seconds: elapsed_s,
            remaining_seconds: remaining_s,
        }
    }

    #[test]
    fn detects_valid_opportunity() {
        let mut detector = PriceDetector::new(default_cfg());
        let snap = make_snapshot("KXBTC-1", 90, 91, 360, 120);
        assert!(detector.check(&snap, MarketType::Btc).is_some());
    }

    #[test]
    fn rejects_low_price() {
        let mut detector = PriceDetector::new(default_cfg());
        let snap = make_snapshot("KXBTC-1", 80, 81, 360, 120);
        assert!(detector.check(&snap, MarketType::Btc).is_none());
    }

    #[test]
    fn rejects_high_price() {
        let mut detector = PriceDetector::new(default_cfg());
        let snap = make_snapshot("KXBTC-1", 96, 97, 360, 120);
        assert!(detector.check(&snap, MarketType::Btc).is_none());
    }

    #[test]
    fn rejects_ask_above_max_buy_price() {
        let mut detector = PriceDetector::new(default_cfg());
        // yes_bid=95 (at max), yes_ask=97 (above max_buy_price=95)
        // no_bid=100-97=3 (out of range), no_ask=100-95=5 (out of range)
        // Both sides should be rejected; ask_too_high should be counted for yes side
        let snap = MarketSnapshot {
            ticker: "KXBTC-1".to_string(),
            event_ticker: "KXBTC-24DEC".to_string(),
            title: "BTC above 50k?".to_string(),
            yes_ask: Some(97),
            yes_bid: Some(95),
            no_ask: Some(5),
            no_bid: Some(3),
            last_price: Some(95),
            open_time: None,
            close_time: None,
            elapsed_seconds: 360,
            remaining_seconds: 120,
        };
        assert!(detector.check(&snap, MarketType::Btc).is_none());
        assert_eq!(detector.rejection_counts.ask_too_high, 1);
    }

    #[test]
    fn rejects_insufficient_elapsed() {
        let mut detector = PriceDetector::new(default_cfg());
        let snap = make_snapshot("KXBTC-1", 90, 91, 60, 120); // only 1 minute elapsed
        assert!(detector.check(&snap, MarketType::Btc).is_none());
    }

    #[test]
    fn rejects_too_little_remaining() {
        let mut detector = PriceDetector::new(default_cfg());
        let snap = make_snapshot("KXBTC-1", 90, 91, 360, 10); // only 10s remaining
        assert!(detector.check(&snap, MarketType::Btc).is_none());
    }

    #[test]
    fn rejects_active_position() {
        let mut detector = PriceDetector::new(default_cfg());
        detector.mark_active("KXBTC-1");
        let snap = make_snapshot("KXBTC-1", 90, 91, 360, 120);
        assert!(detector.check(&snap, MarketType::Btc).is_none());
    }

    #[test]
    fn allows_reentry_after_position_closed() {
        let mut detector = PriceDetector::new(default_cfg());
        detector.mark_active("KXBTC-1");
        detector.mark_closed("KXBTC-1");
        let snap = make_snapshot("KXBTC-1", 90, 91, 360, 120);
        assert!(detector.check(&snap, MarketType::Btc).is_some());
    }

    #[test]
    fn rejection_counts_are_tracked() {
        let mut detector = PriceDetector::new(default_cfg());
        // bid out of range (yes_bid=80 and no_bid=19, both out of [87,95])
        let snap1 = make_snapshot("KXBTC-1", 80, 81, 360, 120);
        detector.check(&snap1, MarketType::Btc);
        // yes ask too high (yes_bid=95 in range but yes_ask=97 > max; no side out of range)
        let snap2 = MarketSnapshot {
            ticker: "KXBTC-2".to_string(),
            event_ticker: "KXBTC-24DEC".to_string(),
            title: "BTC above 50k?".to_string(),
            yes_ask: Some(97),
            yes_bid: Some(95),
            no_ask: Some(5),
            no_bid: Some(3),
            last_price: Some(95),
            open_time: None,
            close_time: None,
            elapsed_seconds: 360,
            remaining_seconds: 120,
        };
        detector.check(&snap2, MarketType::Btc);
        let (checked, counts) = detector.take_rejection_summary();
        assert_eq!(checked, 2);
        assert_eq!(counts.bid_out_of_range, 1);
        assert_eq!(counts.ask_too_high, 1);
        // counters should be reset after taking summary
        let (checked2, _) = detector.take_rejection_summary();
        assert_eq!(checked2, 0);
    }

    #[test]
    fn detects_no_side_opportunity() {
        let mut detector = PriceDetector::new(default_cfg());
        // Yes bid is 10 (too low), but No bid = 90 (in range 87-95)
        let snap = MarketSnapshot {
            ticker: "KXBTC-1".to_string(),
            event_ticker: "KXBTC-24DEC".to_string(),
            title: "BTC above 50k?".to_string(),
            yes_ask: Some(11),
            yes_bid: Some(10),
            no_ask: Some(91),
            no_bid: Some(90),
            last_price: None,
            open_time: None,
            close_time: None,
            elapsed_seconds: 360,
            remaining_seconds: 120,
        };
        let opp = detector.check(&snap, MarketType::Btc);
        assert!(opp.is_some());
        assert_eq!(opp.unwrap().order_side, OrderSide::No);
    }

    #[test]
    fn detects_yes_side_directly() {
        let mut detector = PriceDetector::new(default_cfg());
        // yes_bid=90 in [87,95], yes_ask=92 <= 95 → Yes side approved directly
        let snap = make_snapshot("KXBTC-1", 90, 92, 360, 120);
        let opp = detector.check(&snap, MarketType::Btc);
        assert!(opp.is_some());
        assert_eq!(opp.unwrap().order_side, OrderSide::Yes);
    }

    #[test]
    fn detects_no_side_when_yes_out_of_range() {
        let mut detector = PriceDetector::new(default_cfg());
        // yes_bid=10 out of [87,95]; no_bid=90 in [87,95], no_ask=91 <= 95 → No side approved
        let snap = MarketSnapshot {
            ticker: "KXBTC-2".to_string(),
            event_ticker: "KXBTC-24DEC".to_string(),
            title: "BTC above 50k?".to_string(),
            yes_ask: Some(11),
            yes_bid: Some(10),
            no_ask: Some(91),
            no_bid: Some(90),
            last_price: None,
            open_time: None,
            close_time: None,
            elapsed_seconds: 360,
            remaining_seconds: 120,
        };
        let opp = detector.check(&snap, MarketType::Btc);
        assert!(opp.is_some());
        assert_eq!(opp.unwrap().order_side, OrderSide::No);
    }

    #[test]
    fn rejects_when_both_sides_out_of_range() {
        let mut detector = PriceDetector::new(default_cfg());
        // yes_bid=50, no_bid=45 — neither in [87,95]
        let snap = make_snapshot("KXBTC-3", 50, 55, 360, 120);
        assert!(detector.check(&snap, MarketType::Btc).is_none());
    }

    #[test]
    fn approves_yes_side_when_last_price_none() {
        let mut detector = PriceDetector::new(default_cfg());
        // last_price absent (WS only sent bid/ask); yes_bid=90 in [87,95], yes_ask=91 <= 95
        let snap = MarketSnapshot {
            ticker: "KXBTC-fallback".to_string(),
            event_ticker: "KXBTC-24DEC".to_string(),
            title: "BTC above 50k?".to_string(),
            yes_ask: Some(91),
            yes_bid: Some(90),
            no_ask: Some(10),
            no_bid: Some(9),
            last_price: None,
            open_time: None,
            close_time: None,
            elapsed_seconds: 360,
            remaining_seconds: 120,
        };
        let opp = detector.check(&snap, MarketType::Btc);
        assert!(opp.is_some());
        assert_eq!(opp.unwrap().order_side, OrderSide::Yes);
    }

    #[test]
    fn rejects_when_both_bids_none() {
        let mut detector = PriceDetector::new(default_cfg());
        let snap = MarketSnapshot {
            ticker: "KXBTC-noprice".to_string(),
            event_ticker: "KXBTC-24DEC".to_string(),
            title: "BTC above 50k?".to_string(),
            yes_ask: Some(91),
            yes_bid: None,
            no_ask: Some(10),
            no_bid: None,
            last_price: None,
            open_time: None,
            close_time: None,
            elapsed_seconds: 360,
            remaining_seconds: 120,
        };
        assert!(detector.check(&snap, MarketType::Btc).is_none());
        assert_eq!(detector.rejection_counts.missing_price, 1);
    }

    #[test]
    fn rejects_yes_ask_too_high_no_bid_out_of_range() {
        // yes_bid=96 in [87,97], yes_ask=98 > 97 → Yes side rejected (ask too high)
        // no_bid=4 not in [87,97] → No side rejected (out of range)
        // Expected: reject with ask_too_high
        let cfg = TradingConfig {
            max_buy_price: 97,
            ..default_cfg()
        };
        let mut detector = PriceDetector::new(cfg);
        let snap = MarketSnapshot {
            ticker: "KXBTC-askhigh".to_string(),
            event_ticker: "KXBTC-24DEC".to_string(),
            title: "BTC above 50k?".to_string(),
            yes_ask: Some(98),
            yes_bid: Some(96),
            no_ask: Some(6),
            no_bid: Some(4),
            last_price: None,
            open_time: None,
            close_time: None,
            elapsed_seconds: 360,
            remaining_seconds: 120,
        };
        assert!(detector.check(&snap, MarketType::Btc).is_none());
        assert_eq!(detector.rejection_counts.ask_too_high, 1);
    }

    #[test]
    fn approves_no_side_when_yes_bid_out_of_range() {
        // yes_bid=50 not in [87,95] → Yes side skipped
        // no_bid=92 in [87,95], no_ask=93 <= 95 → No side approved
        let mut detector = PriceDetector::new(default_cfg());
        let snap = MarketSnapshot {
            ticker: "KXBTC-noside".to_string(),
            event_ticker: "KXBTC-24DEC".to_string(),
            title: "BTC above 50k?".to_string(),
            yes_ask: Some(52),
            yes_bid: Some(50),
            no_ask: Some(93),
            no_bid: Some(92),
            last_price: None,
            open_time: None,
            close_time: None,
            elapsed_seconds: 360,
            remaining_seconds: 120,
        };
        let opp = detector.check(&snap, MarketType::Btc);
        assert!(opp.is_some());
        assert_eq!(opp.unwrap().order_side, OrderSide::No);
    }

    #[test]
    fn rejects_below_trigger_price() {
        // bid=85 is below trigger_price(87) — should NOT qualify
        let mut detector = PriceDetector::new(default_cfg());
        let snap = make_snapshot("KXBTC-1", 85, 86, 360, 120);
        assert!(detector.check(&snap, MarketType::Btc).is_none());
    }

    #[test]
    fn enforces_min_time_remaining() {
        // 5s remaining should be rejected
        let mut detector = PriceDetector::new(default_cfg());
        let snap = make_snapshot("KXBTC-1", 90, 91, 360, 5);
        assert!(detector.check(&snap, MarketType::Btc).is_none());
        assert_eq!(detector.rejection_counts.insufficient_remaining, 1);
    }

    // --- Early window tests ---

    fn early_window_cfg() -> TradingConfig {
        TradingConfig {
            early_window_enabled: true,
            early_window_start_minutes: 1.0,
            early_window_end_minutes: 5.0,
            early_window_trigger_price: 88,
            early_window_max_buy_price: 92,
            min_elapsed_minutes: 5.0, // late window opens when early window closes
            ..default_cfg()
        }
    }

    #[test]
    fn early_window_approved_within_time_range() {
        // elapsed=2min (within [1,5)), bid=90 (>= 88), ask=91 (<= 92) → approved as early window
        let mut detector = PriceDetector::new(early_window_cfg());
        let snap = make_snapshot("KXBTC-1", 90, 91, 120, 120); // 120s = 2min
        let opp = detector.check(&snap, MarketType::Btc);
        assert!(opp.is_some(), "should approve early window buy");
        let opp = opp.unwrap();
        assert!(opp.is_early_window, "opportunity should be flagged as early window");
    }

    #[test]
    fn early_window_rejected_before_start_minutes() {
        // elapsed=30s (< 1min start) → no early window
        let mut detector = PriceDetector::new(early_window_cfg());
        let snap = make_snapshot("KXBTC-1", 90, 91, 30, 120); // 30s < 1min
        let opp = detector.check(&snap, MarketType::Btc);
        assert!(opp.is_none(), "should reject before early window start");
        // elapsed < min_elapsed_minutes so insufficient_elapsed counted
        assert_eq!(detector.rejection_counts.insufficient_elapsed, 1);
    }

    #[test]
    fn early_window_rejected_after_end_minutes() {
        // elapsed=6min (>= end_minutes=5, late window check: >= min_elapsed=5 → late window)
        let mut detector = PriceDetector::new(early_window_cfg());
        // At 6min, early window is closed but late window is open; use a bid too low for late window.
        let snap2 = make_snapshot("KXBTC-1", 85, 86, 360, 120); // bid=85 < trigger=87
        let opp = detector.check(&snap2, MarketType::Btc);
        assert!(opp.is_none(), "should reject when early window closed and late window price too low");
    }

    #[test]
    fn early_window_not_active_at_exact_end_minutes() {
        // elapsed=5min (>= end_minutes=5) → early window closed, late window open
        // bid=88 (matches early trigger but NOT late trigger=87, actually 88 >= 87 so late window passes)
        // Let's verify the classification: at exactly end_minutes, it falls into late window path
        let mut detector = PriceDetector::new(early_window_cfg());
        let snap = make_snapshot("KXBTC-1", 90, 91, 300, 120); // 300s = 5min exactly
        let opp = detector.check(&snap, MarketType::Btc);
        assert!(opp.is_some(), "late window should be active at exactly end_minutes");
        assert!(!opp.unwrap().is_early_window, "should NOT be flagged as early window");
    }

    #[test]
    fn late_window_blocked_after_early_buy() {
        // After an early window buy, the ticker is blocked from late window
        let mut detector = PriceDetector::new(early_window_cfg());
        detector.mark_early_buy("KXBTC-1");
        // Move into late window (6min elapsed)
        let snap = make_snapshot("KXBTC-1", 90, 91, 360, 120);
        let opp = detector.check(&snap, MarketType::Btc);
        assert!(opp.is_none(), "late window should be blocked after early buy");
    }

    #[test]
    fn late_window_stays_blocked_after_early_buy_and_mark_closed() {
        // mark_closed only removes from active_tickers, NOT from early_window_bought.
        // So the ticker should remain blocked from late window after mark_closed.
        let mut detector = PriceDetector::new(early_window_cfg());
        detector.mark_early_buy("KXBTC-1");
        assert!(detector.is_blocked_for_late_window("KXBTC-1"), "should be blocked initially");

        detector.mark_closed("KXBTC-1");
        // early_window_bought still has it; should still be blocked
        assert!(detector.is_blocked_for_late_window("KXBTC-1"), "should still be blocked after mark_closed");
    }

    #[test]
    fn early_window_disabled_works_as_before() {
        // With early_window_enabled=false, behavior is unchanged from original
        let mut detector = PriceDetector::new(default_cfg()); // early_window_enabled=false
        // At 2min elapsed, should be rejected (early window disabled, late window not yet open)
        let snap = make_snapshot("KXBTC-1", 90, 91, 120, 120); // 2min
        assert!(detector.check(&snap, MarketType::Btc).is_none(), "should reject when early window disabled");
        assert_eq!(detector.rejection_counts.insufficient_elapsed, 1);

        // At 6min elapsed with good price, should approve (late window)
        let snap2 = make_snapshot("KXBTC-1", 90, 91, 360, 120);
        assert!(detector.check(&snap2, MarketType::Btc).is_some(), "late window should work normally when early window disabled");
    }

    // --- Momentum confirmation tests ---

    fn momentum_cfg() -> TradingConfig {
        TradingConfig {
            momentum_enabled: true,
            momentum_window_seconds: 10,
            momentum_min_trend: 0,
            ..default_cfg()
        }
    }

    #[test]
    fn momentum_allows_entry_when_bid_above_avg() {
        // With 5 historical samples at 88¢ and a current bid of 90¢, momentum is satisfied.
        let mut detector = PriceDetector::new(momentum_cfg());
        let ticker = "KXBTC-1";

        // Seed the yes_bid history manually (simulate N previous ticks at 88¢).
        let now = Utc::now();
        let hist = detector.yes_bid_history.entry(ticker.to_string()).or_default();
        for i in 0..5 {
            hist.push_back((now - chrono::Duration::seconds(5 + i), 88));
        }

        // Current bid 90¢ >= avg(88) + min_trend(0) → approved.
        let snap = make_snapshot(ticker, 90, 91, 360, 120);
        let opp = detector.check(&snap, MarketType::Btc);
        assert!(opp.is_some(), "should approve when bid >= avg");
    }

    #[test]
    fn momentum_rejects_entry_when_bid_below_avg() {
        // History at 92¢; current bid 90¢ < avg(92) → momentum blocks entry.
        let mut detector = PriceDetector::new(momentum_cfg());
        let ticker = "KXBTC-1";

        let now = Utc::now();
        let hist = detector.yes_bid_history.entry(ticker.to_string()).or_default();
        for i in 0..5 {
            hist.push_back((now - chrono::Duration::seconds(5 + i), 92));
        }

        // yes_bid=90¢ is in range [87,95] but below avg(92) → rejected.
        let snap = make_snapshot(ticker, 90, 91, 360, 120);
        let opp = detector.check(&snap, MarketType::Btc);
        assert!(opp.is_none(), "should reject when bid < avg");
        assert_eq!(detector.rejection_counts.momentum_rejected, 1);
    }

    #[test]
    fn momentum_allows_entry_when_bid_equals_avg() {
        // current bid == avg → allowed (>= with min_trend=0).
        let mut detector = PriceDetector::new(momentum_cfg());
        let ticker = "KXBTC-1";

        let now = Utc::now();
        let hist = detector.yes_bid_history.entry(ticker.to_string()).or_default();
        for i in 0..5 {
            hist.push_back((now - chrono::Duration::seconds(5 + i), 90));
        }

        let snap = make_snapshot(ticker, 90, 91, 360, 120);
        let opp = detector.check(&snap, MarketType::Btc);
        assert!(opp.is_some(), "should approve when bid equals avg");
    }

    #[test]
    fn momentum_allows_entry_with_empty_history() {
        // No history yet (cold start) → allowed.
        let mut detector = PriceDetector::new(momentum_cfg());
        let snap = make_snapshot("KXBTC-1", 90, 91, 360, 120);
        let opp = detector.check(&snap, MarketType::Btc);
        assert!(opp.is_some(), "should allow entry with empty momentum history (cold start)");
    }

    #[test]
    fn momentum_disabled_does_not_block_entry() {
        // With momentum_enabled=false, entries are never blocked by momentum.
        let cfg = TradingConfig {
            momentum_enabled: false,
            ..momentum_cfg()
        };
        let mut detector = PriceDetector::new(cfg);
        let ticker = "KXBTC-1";

        // Seed history at 99¢ to make current bid 90¢ look like downtrend.
        let now = Utc::now();
        let hist = detector.yes_bid_history.entry(ticker.to_string()).or_default();
        for i in 0..5 {
            hist.push_back((now - chrono::Duration::seconds(5 + i), 99));
        }

        let snap = make_snapshot(ticker, 90, 91, 360, 120);
        let opp = detector.check(&snap, MarketType::Btc);
        assert!(opp.is_some(), "momentum=false should never block an otherwise valid entry");
        assert_eq!(detector.rejection_counts.momentum_rejected, 0);
    }

    #[test]
    fn momentum_does_not_apply_to_early_window() {
        // Early-window buys are never filtered by the momentum check.
        let cfg = TradingConfig {
            momentum_enabled: true,
            momentum_window_seconds: 10,
            momentum_min_trend: 0,
            ..early_window_cfg()
        };
        let mut detector = PriceDetector::new(cfg);
        let ticker = "KXBTC-1";

        // Seed unfavourable history for yes side.
        let now = Utc::now();
        let hist = detector.yes_bid_history.entry(ticker.to_string()).or_default();
        for i in 0..5 {
            hist.push_back((now - chrono::Duration::seconds(5 + i), 95));
        }

        // 2min elapsed → early window; bid=90¢ in early window range [88,92].
        let snap = make_snapshot(ticker, 90, 91, 120, 120);
        let opp = detector.check(&snap, MarketType::Btc);
        assert!(opp.is_some(), "early window entry should bypass momentum check");
        assert!(opp.unwrap().is_early_window);
        assert_eq!(detector.rejection_counts.momentum_rejected, 0);
    }

    #[test]
    fn momentum_with_min_trend_requires_higher_bid() {
        // momentum_min_trend=3 means bid must be >= avg + 3.
        let cfg = TradingConfig {
            momentum_enabled: true,
            momentum_min_trend: 3,
            ..default_cfg()
        };
        let mut detector = PriceDetector::new(cfg);
        let ticker = "KXBTC-1";

        // Avg = 88¢; threshold = 88 + 3 = 91¢.
        let now = Utc::now();
        let hist = detector.yes_bid_history.entry(ticker.to_string()).or_default();
        for i in 0..5 {
            hist.push_back((now - chrono::Duration::seconds(5 + i), 88));
        }

        // bid=90¢ < 91 (threshold) → rejected.
        let snap = make_snapshot(ticker, 90, 91, 360, 120);
        assert!(detector.check(&snap, MarketType::Btc).is_none(), "bid=90 < avg(88)+3 should be rejected");
        assert_eq!(detector.rejection_counts.momentum_rejected, 1);

        // bid=91¢ == 91 (threshold) → approved.
        let snap2 = make_snapshot(ticker, 91, 92, 360, 120);
        assert!(detector.check(&snap2, MarketType::Btc).is_some(), "bid=91 == avg(88)+3 should be approved");
    }

    #[test]
    fn momentum_counter_incremented_once_per_market() {
        // Even if both sides fail momentum, the counter increments only once per market check.
        let mut detector = PriceDetector::new(momentum_cfg());
        let ticker = "KXBTC-1";

        let now = Utc::now();
        // Seed yes and no histories with high averages so both sides fail.
        let yes_hist = detector.yes_bid_history.entry(ticker.to_string()).or_default();
        for i in 0..5 {
            yes_hist.push_back((now - chrono::Duration::seconds(5 + i), 94));
        }
        let no_hist = detector.no_bid_history.entry(ticker.to_string()).or_default();
        for i in 0..5 {
            no_hist.push_back((now - chrono::Duration::seconds(5 + i), 94));
        }

        // Both yes_bid=90 and no_bid=90 are below avg(94).
        let snap = make_snapshot(ticker, 90, 91, 360, 120);
        assert!(detector.check(&snap, MarketType::Btc).is_none());
        assert_eq!(detector.rejection_counts.momentum_rejected, 1, "counter should be 1, not 2");
    }
}
