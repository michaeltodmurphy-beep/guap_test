use anyhow::Result;
use chrono::Utc;
use clap::Parser;
use std::time::Duration;

mod config;
mod detector;
mod kalshi_api;
mod models;
mod monitor;
mod trader;
mod ws_monitor;

use config::{CliArgs, Config};
use detector::PriceDetector;
use kalshi_api::KalshiApiClient;
use monitor::MarketMonitor;
use trader::Trader;
use ws_monitor::{build_ws_url, WsMarketMonitor};

#[tokio::main]
async fn main() -> Result<()> {
    // Initialise logging (RUST_LOG controls verbosity; default to info)
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args = CliArgs::parse();
    let simulation = Config::is_simulation(&args);

    log::info!(
        "Starting Kalshi crypto trading bot (simulation={})",
        simulation
    );

    let config = Config::load(&args.config)?;

    log::info!(
        "Config loaded — API base: {}  series: {:?}",
        config.kalshi.api_base_url,
        config.trading.market_series_tickers
    );

    let api = KalshiApiClient::new(&config.kalshi)?;

    // BUG 7: Validate credentials at startup when running in live mode.
    if !simulation && !api.has_credentials() {
        log::error!(
            "Live mode requires API credentials (api_key_id and private_key_path) in config. \
             Set them in your config file or run with --simulation."
        );
        std::process::exit(1);
    }

    // Check exchange status (best-effort; don't abort if unavailable)
    let exchange_status = match api.get_exchange_status().await {
        Ok(status) => {
            log::info!(
                "Exchange status: exchange_active={} trading_active={}",
                status.exchange_active,
                status.trading_active
            );
            Some(status)
        }
        Err(e) => {
            log::warn!("Could not fetch exchange status: {e}");
            None
        }
    };

    // --diagnose: print exchange status and market listings, then exit
    if args.diagnose {
        if let Some(status) = &exchange_status {
            println!(
                "Exchange status: exchange_active={} trading_active={}",
                status.exchange_active, status.trading_active
            );
        } else {
            println!("Exchange status: (unavailable)");
        }

        let now = Utc::now();
        for series in &config.trading.market_series_tickers {
            match api.list_series_markets(series).await {
                Err(e) => println!("Series {series}: ERROR fetching markets: {e}"),
                Ok(markets) => {
                    println!("Series {series}: {} markets found", markets.len());
                    for m in &markets {
                        let close = m.close_time.or(m.expiration_time);
                        let elapsed_seconds = m
                            .open_time
                            .map(|t| (now - t).num_seconds())
                            .unwrap_or(-1);
                        let remaining_seconds = close
                            .map(|t| (t - now).num_seconds())
                            .unwrap_or(-1);
                        println!(
                            "  {}: status={} yes_bid={:?}¢ yes_ask={:?}¢ \
                             no_bid={:?}¢ no_ask={:?}¢ elapsed={}s remaining={}s",
                            m.ticker,
                            m.status,
                            m.yes_bid(),
                            m.yes_ask(),
                            m.no_bid(),
                            m.no_ask(),
                            elapsed_seconds,
                            remaining_seconds,
                        );
                    }
                }
            }
        }
        return Ok(());
    }

    let monitor = MarketMonitor::new(api.clone(), config.clone());
    let mut detector = PriceDetector::new(config.trading.clone());
    let mut trader = Trader::new(api.clone(), config.clone(), simulation);

    // Optionally start the WebSocket market monitor
    let ws_mon = if config.trading.use_websocket {
        if api.has_credentials() {
            // Determine and log the WebSocket URL we will connect to
            match build_ws_url(&config.kalshi.api_base_url, config.kalshi.ws_base_url.as_deref()) {
                Ok(ws_url) => log::info!("WebSocket mode enabled — connecting to {ws_url}"),
                Err(e) => log::warn!("Could not determine WebSocket URL: {e}"),
            }

            let ws = WsMarketMonitor::new(api.clone(), config.clone());
            ws.start().await;
            log::info!("WebSocket market monitor started");
            Some(ws)
        } else {
            log::warn!(
                "use_websocket=true but no credentials configured; falling back to REST polling"
            );
            None
        }
    } else {
        log::info!("REST polling mode enabled (use_websocket=false)");
        None
    };

    // Log market counts for each configured series at startup
    for series in &config.trading.market_series_tickers {
        match api.list_series_markets(series).await {
            Ok(markets) => log::info!("Series {series}: {} markets found at startup", markets.len()),
            Err(e) => log::warn!("Series {series}: failed to fetch markets at startup: {e}"),
        }
    }

    let mut tick_interval = tokio::time::interval(Duration::from_millis(config.trading.check_interval_ms));
    let mut tick = 0u64;
    let mut last_sync = tokio::time::Instant::now();
    let mut last_log_summary = tokio::time::Instant::now();
    let sync_interval = Duration::from_secs(15);
    let log_summary_interval = Duration::from_millis(7500);

    log::info!("Entering main loop (interval={}ms) …", config.trading.check_interval_ms);

    loop {
        tick_interval.tick().await;
        tick += 1;

        // --- 1. Discover / refresh market snapshots ---
        let snapshots = if let Some(ref ws) = ws_mon {
            ws.get_snapshots().await
        } else {
            match monitor.fetch_snapshots().await {
                Ok(s) => s,
                Err(e) => {
                    log::error!("Failed to fetch market snapshots: {e}");
                    continue;
                }
            }
        };

        if snapshots.is_empty() {
            log::info!("No open markets found this tick");
        }

        // --- 1b. Check no-trade schedule ---
        let in_no_trade_window = config.no_trade_schedule.is_no_trade_time();
        if in_no_trade_window {
            if let Err(e) = trader.check_pending_trades(&snapshots, &mut detector).await {
                log::error!("Error checking pending trades: {e}");
            }
            continue;
        }

        // --- 2. Detect buy opportunities ---
        for (ticker, snapshot) in &snapshots {
            let mtype = monitor.market_type_for(&snapshot.event_ticker);

            // Skip if we already hold the maximum number of active positions for this asset type
            if trader.has_active_position_for_type(&mtype) {
                log::info!(
                    "Skipping {} — already holding active {:?} position(s) (max={})",
                    ticker, mtype, config.trading.max_concurrent_positions
                );
                continue;
            }

            // Skip if we already have an active/pending trade for this specific ticker
            if trader.has_active_position(ticker) {
                continue;
            }

            if let Some(opp) = detector.check(snapshot, mtype) {
                log::info!(
                    "Opportunity detected: {} @ bid={}¢  ask={}¢  elapsed={:.1}min  remaining={}s",
                    opp.ticker,
                    opp.bid_price,
                    opp.ask_price,
                    opp.elapsed_minutes,
                    opp.remaining_seconds,
                );
                if let Err(e) = trader.execute_buy(&opp, &mut detector).await {
                    log::error!("Failed to execute buy for {}: {e}", opp.ticker);
                }
            }
        }

        // --- 3. Check exit conditions for open trades ---
        if let Err(e) = trader.check_pending_trades(&snapshots, &mut detector).await {
            log::error!("Error checking pending trades: {e}");
        }

        // --- 4. Periodic portfolio sync (every 15 seconds) ---
        if last_sync.elapsed() >= sync_interval {
            if let Err(e) = trader.sync_positions(&mut detector).await {
                log::warn!("Portfolio sync failed: {e}");
            }
            last_sync = tokio::time::Instant::now();
        }

        // --- 5. Log summary periodically ---
        if last_log_summary.elapsed() >= log_summary_interval {
            log::info!(
                "Status: tick={} open_positions={} trades={} total_pnl=${:.4}",
                tick,
                trader.pending_trades.len(),
                trader.trades_executed,
                trader.total_pnl
            );
            let (checked, rejections) = detector.take_rejection_summary();
            log::info!(
                "Detector summary (last period): markets_checked={} \
                 active_position={} missing_price={} bid_out_of_range={} \
                 ask_too_high={} insufficient_elapsed={} insufficient_remaining={} \
                 momentum_rejected={}",
                checked,
                rejections.active_position,
                rejections.missing_price,
                rejections.bid_out_of_range,
                rejections.ask_too_high,
                rejections.insufficient_elapsed,
                rejections.insufficient_remaining,
                rejections.momentum_rejected,
            );
            last_log_summary = tokio::time::Instant::now();
        }
    }
}
