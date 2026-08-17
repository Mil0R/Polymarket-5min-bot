mod config;
mod connection_supervisor;
mod live_safety;
mod market;
mod monitor;
mod network_retry;
mod observability_server;
mod opportunity_admission;
#[cfg(test)]
mod protocol_contract_tests;
mod risk;
mod runtime_metrics;
mod strategy_gate;
mod strategy_service;
mod trading;
mod utils;
mod wind_down_service;
mod window;
mod window_event_loop;
mod window_session;
mod window_session_runner;
use polypulse::ui::{decimal_to_f64, spawn_dashboard_thread, symbol_short, DashboardHandle};

use anyhow::Result;
use futures::StreamExt;
use rust_decimal_macros::dec;
use std::io::{stdout, IsTerminal};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

use crate::config::Config;
use crate::connection_supervisor::ConnectionSupervisor;
use crate::live_safety::{CanaryPolicy, LiveSafetyGuard};
use crate::market::{MarketDiscoverer, MarketScheduler};
use crate::monitor::OrderBookMonitor;
use crate::observability_server::ServiceHealth;
use crate::risk::{HedgeMonitor, PositionBalancer, RiskManager};
use crate::runtime_metrics::RuntimeMetrics;
use crate::strategy_service::{StrategyService, StrategyServiceDeps};
use crate::trading::{
    create_authenticated_clob_client, parse_signature_type, v1_address_to_v2, ExecutionPolicy,
    TradingExecutor,
};
use crate::wind_down_service::{ScheduledMergeService, WindDownService};
use crate::window_event_loop::{WindowEventLoopState, WindowExitReason};
use crate::window_session::WindowSession;
use crate::window_session_runner::WindowSessionRunner;

#[tokio::main]
async fn main() -> Result<()> {
    // Must set rustls default crypto provider first, otherwise reqwest/alloy etc. will panic when using TLS

    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    let use_tui = utils::logger::tui_enabled_from_env() && stdout().is_terminal();
    utils::logger::init_logger(use_tui)?;

    let shutdown = Arc::new(AtomicBool::new(false));

    if use_tui {
        tracing::info!(
            "Polymarket 5-minute arbitrage bot starting (dashboard mode, logs → bot.log)"
        );
    } else {
        tracing::info!("Polymarket 5-minute arbitrage bot starting");
    }

    // Load config
    let config = Config::from_env()?;
    tracing::info!("Config loaded");

    let order_mode = format!("{:?}", config.arbitrage_order_type);
    let dashboard = DashboardHandle::new_live(order_mode, config.risk_max_exposure_usdc);
    let runtime_metrics = Arc::new(RuntimeMetrics::default());
    let live_safety = Arc::new(LiveSafetyGuard::new(CanaryPolicy {
        enabled: config.canary_mode,
        max_pair_notional: rust_decimal::Decimal::try_from(config.canary_max_pair_notional_usdc)?,
        max_total_notional: rust_decimal::Decimal::try_from(config.canary_max_total_notional_usdc)?,
        max_loss_budget: rust_decimal::Decimal::try_from(config.canary_max_loss_usdc)?,
        max_pairs_per_window: config.canary_max_pairs_per_window,
        max_pairs_total: config.canary_max_pairs_total,
        max_consecutive_one_sided: config.one_sided_max_consecutive,
    }));
    let service_health = Arc::new(ServiceHealth::default());
    info!(
        enabled = live_safety.canary_enabled(),
        snapshot = ?live_safety.snapshot(),
        "Live safety gate initialized"
    );
    if let Ok(bind) = std::env::var("OBSERVABILITY_BIND") {
        if !bind.trim().is_empty() {
            let health_server = service_health.clone();
            let metrics_server = runtime_metrics.clone();
            let shutdown_server = shutdown.clone();
            tokio::spawn(async move {
                if let Err(error) = observability_server::serve(
                    bind.trim(),
                    health_server,
                    metrics_server,
                    shutdown_server,
                )
                .await
                {
                    error!(%error, "Observability server stopped");
                }
            });
        }
    }
    if use_tui {
        spawn_dashboard_thread(dashboard.arc(), shutdown.clone());
        dashboard.with_mut(|d| {
            d.push_event("🚀 Dashboard ready — hunting arbitrage opportunities");
        });
    }

    // Initialize market discovery and strategy components.
    let _discoverer =
        MarketDiscoverer::new(config.crypto_symbols.clone(), config.market_period_minutes);
    let _scheduler = MarketScheduler::new(_discoverer, config.market_refresh_advance_secs);

    // Validate private key format
    info!("Verifying private key format...");
    use alloy::signers::local::LocalSigner;
    use std::str::FromStr;

    let _signer_test = LocalSigner::from_str(&config.private_key)
        .map_err(|e| anyhow::anyhow!("Invalid private key format: {}", e))?;
    info!("Private key format validated");

    // Initialize trading executor (requires API auth)
    info!("Initializing trading executor (requires API auth)...");
    if let Some(ref proxy) = config.proxy_address {
        info!(
            proxy_address = %proxy,
            signature_type = %config.signature_type,
            "Using funder address for V2 CLOB (default SIGNATURE_TYPE=Poly1271 for deposit wallet; use Proxy only for legacy Magic proxy)"
        );
    } else {
        info!("Using EOA signature type (direct trading)");
    }
    info!("Note: A 'Could not create api key' warning is normal. The SDK tries to create a new API key first; if it fails, it will use the derived method and auth will still succeed.");
    info!(clob_url = %config.clob_api_url, signature_type = %config.signature_type, "CLOB V2 client config");

    let v2_proxy = config.proxy_address.map(v1_address_to_v2);
    let sig_type = parse_signature_type(&config.signature_type);

    let clob_client = match create_authenticated_clob_client(
        &config.private_key,
        &config.clob_api_url,
        v2_proxy,
        sig_type,
    )
    .await
    {
        Ok(client) => {
            info!("CLOB V2 client authenticated");
            client
        }
        Err(e) => {
            error!(error = %e, "CLOB V2 authentication failed! Cannot continue.");
            error!("Please check:");
            error!("  1. POLYMARKET_PRIVATE_KEY is correctly set");
            error!("  2. Private key format (64-char hex, no 0x prefix)");
            error!("  3. Network connectivity");
            error!("  4. Polymarket API availability");
            error!("  5. CLOB_API_URL (use https://clob.polymarket.com, not clob-v2 host)");
            return Err(anyhow::anyhow!("Authentication failed, exiting: {}", e));
        }
    };

    let executor = Arc::new(TradingExecutor::from_client(
        clob_client.clone(),
        config.private_key.clone(),
        ExecutionPolicy {
            max_order_size_usdc: config.max_order_size_usdc,
            slippage: config.slippage,
            min_profit_threshold: config.min_profit_threshold,
            execution_spread: config.arbitrage_execution_spread,
            fee_buffer: config.arbitrage_fee_buffer,
            gtd_expiration_secs: config.gtd_expiration_secs,
            order_type: config.arbitrage_order_type.clone(),
            dry_run: config.dry_run,
            paper_initial_balance_usdc: config.paper_initial_balance_usdc,
            paper_liquidity_haircut: config.paper_liquidity_haircut,
            paper_second_leg_delay_ms: config.paper_second_leg_delay_ms,
        },
    ));

    let _risk_manager = Arc::new(RiskManager::new(&config)?);

    // Create hedge monitor (pass PositionTracker Arc for exposure updates)
    let position_tracker = _risk_manager.position_tracker();
    let hedge_monitor = Arc::new(HedgeMonitor::new(
        clob_client.clone(),
        config.private_key.clone(),
        position_tracker,
        _risk_manager.journal(),
        _risk_manager.owned_orders(),
    ));
    let strategy_service = StrategyService::new(StrategyServiceDeps {
        executor: executor.clone(),
        risk_manager: _risk_manager.clone(),
        hedge_monitor: hedge_monitor.clone(),
        dashboard: dashboard.clone(),
        metrics: runtime_metrics.clone(),
        live_safety: live_safety.clone(),
        service_health: service_health.clone(),
        dry_run: config.dry_run,
    });

    // Verify auth actually succeeded - try a simple API call
    info!("Verifying authentication (via API call test)...");
    match executor.verify_authentication().await {
        Ok(_) => {
            info!("✅ Auth verified, API calls OK");
            dashboard.with_mut(|d| d.push_event("✅ CLOB authenticated — ready to trade"));
        }
        Err(e) => {
            error!(error = %e, "❌ Auth verification failed! authenticate() did not error but API calls fail.");
            error!("This indicates auth did not actually succeed. Possible causes:");
            error!("  1. API key creation failed ('Could not create api key' warning)");
            error!("  2. Account may not be registered on Polymarket");
            error!("  3. Account may be restricted or suspended");
            error!("  4. Network issues");
            error!("Exiting. Please fix authentication before running again.");
            return Err(anyhow::anyhow!("Auth verification failed: {}", e));
        }
    }

    let (startup_actions, restored_recoveries) = if config.dry_run {
        info!("DRY_RUN: startup recovery restoration disabled; no recovery writes can be armed");
        (Vec::new(), 0)
    } else {
        let actions = _risk_manager
            .reconcile_startup(&config, &clob_client)
            .await?;
        let restored = hedge_monitor.restore_from_journal()?;
        for action in &actions {
            hedge_monitor.add_position(action)?;
        }
        (actions, restored)
    };
    if restored_recoveries > 0 || !startup_actions.is_empty() {
        warn!(
            restored = restored_recoveries,
            created = startup_actions.len(),
            "Startup reconciliation restored active recovery monitors; new live entries remain paused"
        );
    }

    let journal = _risk_manager.journal();
    service_health.set_journal_writable(journal.probe_writable().is_ok());
    let journal_dir = journal
        .path()
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    const MIN_READY_DISK_BYTES: u64 = 64 * 1024 * 1024;
    let disk_healthy = fs2::available_space(journal_dir)
        .map(|bytes| bytes >= MIN_READY_DISK_BYTES)
        .unwrap_or(false);
    service_health.set_disk_healthy(disk_healthy);
    service_health.set_no_unresolved_orders(!_risk_manager.has_unresolved_state());
    service_health.set_safety_ok(!live_safety.is_tripped());

    info!("✅ All components initialized, auth verified");
    service_health.mark_initialized();

    // Create position balancer
    let position_balancer = Arc::new(PositionBalancer::new(
        clob_client.clone(),
        _risk_manager.position_tracker(),
        &config,
    ));

    // Scheduled position sync: every N seconds fetch latest positions from API, overwrite local cache
    let position_sync_interval = if config.dry_run {
        0
    } else {
        config.position_sync_interval_secs
    };
    if position_sync_interval > 0 {
        let position_tracker_sync = _risk_manager.position_tracker();
        tokio::spawn(async move {
            let interval = Duration::from_secs(position_sync_interval);
            loop {
                match position_tracker_sync.sync_from_api().await {
                    Ok(_) => {
                        // Positions printed in sync_from_api
                    }
                    Err(e) => {
                        warn!(error = %e, "Position sync failed, will retry next loop");
                    }
                }
                sleep(interval).await;
            }
        });
        info!(
            interval_secs = position_sync_interval,
            "Started position sync task, every {}s fetch latest positions from API",
            position_sync_interval
        );
    } else {
        warn!("POSITION_SYNC_INTERVAL_SECS=0, position sync disabled");
    }

    // Scheduled position balance: every N seconds check positions and orders, cancel excess orders
    // Note: balance task runs in main loop as it needs market mapping
    let balance_interval = if config.dry_run {
        0
    } else {
        config.position_balance_interval_secs
    };
    if balance_interval > 0 {
        info!(
            interval_secs = balance_interval,
            "Position balance task runs in main loop every {}s", balance_interval
        );
    } else {
        info!("Position balance not enabled (POSITION_BALANCE_INTERVAL_SECS=0)");
    }

    // Wind-down in progress flag: scheduled merge checks and skips to avoid race with wind-down merge
    let wind_down_in_progress = Arc::new(AtomicBool::new(false));

    // Minimum interval between two arbitrage trades
    const MIN_TRADE_INTERVAL: Duration = Duration::from_secs(3);
    let last_trade_time: Arc<tokio::sync::Mutex<Option<Instant>>> =
        Arc::new(tokio::sync::Mutex::new(None));

    // Scheduled Merge: every N minutes run merge by positions, only for markets with both YES+NO
    let merge_interval = config.merge_interval_minutes;
    if merge_interval > 0 && !config.dry_run {
        if let Some(proxy) = config.proxy_address {
            tokio::spawn(
                ScheduledMergeService {
                    interval_minutes: merge_interval,
                    proxy,
                    private_key: config.private_key.clone(),
                    positions: _risk_manager.position_tracker(),
                    wind_down_in_progress: wind_down_in_progress.clone(),
                }
                .run(),
            );
            info!(
                interval_minutes = merge_interval,
                "Started scheduled Merge task, every {} minutes (YES+NO both positions only)",
                merge_interval
            );
        } else {
            warn!("MERGE_INTERVAL_MINUTES={} but POLYMARKET_PROXY_ADDRESS not set, scheduled Merge disabled", merge_interval);
        }
    } else if config.dry_run && merge_interval > 0 {
        info!("DRY_RUN: scheduled Merge disabled");
    } else {
        info!("Scheduled Merge not enabled (MERGE_INTERVAL_MINUTES=0). To enable, set MERGE_INTERVAL_MINUTES in .env (e.g. 5 or 15)");
    }

    // Main loop enabled, start monitoring and trading
    let mut connection_supervisor = ConnectionSupervisor::new(
        Duration::from_secs(1),
        Duration::from_secs(30),
        Duration::from_secs(30),
    );
    #[allow(unreachable_code)]
    loop {
        if shutdown.load(Ordering::Relaxed) {
            tracing::info!("Shutdown requested from dashboard");
            return Ok(());
        }

        // Fetch markets for current window immediately, or wait for next window on failure
        let markets = match _scheduler.get_markets_immediately_or_wait().await {
            Ok(markets) => markets,
            Err(e) => {
                error!(error = %e, "Failed to fetch markets");
                sleep(Duration::from_secs(60)).await;
                continue;
            }
        };

        if markets.is_empty() {
            warn!("No markets found, skipping current window");
            continue;
        }

        dashboard.with_mut(|d| {
            d.window_pnl = 0.0;
            d.markets.clear();
        });

        // Initialize orderbook monitor
        let mut monitor = OrderBookMonitor::new();

        // Subscribe to all markets
        for market in &markets {
            dashboard.with_mut(|d| d.ensure_market(symbol_short(&market.crypto_symbol)));
            if let Err(e) = monitor.subscribe_market(market) {
                error!(error = %e, market_id = %market.market_id, "Failed to subscribe to market");
            }
        }
        for token_id in hedge_monitor.monitored_tokens() {
            monitor.subscribe_recovery_token(token_id);
        }

        // Create orderbook stream
        let mut stream = match monitor.create_orderbook_stream() {
            Ok(stream) => stream,
            Err(e) => {
                error!(error = %e, "Failed to create orderbook stream");
                continue;
            }
        };

        info!(market_count = markets.len(), "Monitoring orderbook");

        use chrono::Utc;
        let mut window_session = WindowSession::new(&markets, Utc::now(), config.market_period_minutes)?;
        let window_label = window_session.label().to_string();

        dashboard.with_mut(|d| {
            d.set_window(&window_label, 300);
            d.set_connected(true);
            d.push_event(format!(
                "📡 Live — {} markets in {}",
                markets.len(),
                window_label
            ));
        });

        // Create market mapping (condition_id -> (yes_token_id, no_token_id)) for position balance
        let market_token_map = window_session.market_tokens().clone();
        let window_scope = window_session.scope();

        // Authenticated user stream closes the gap between the immediate post_order
        // response and later maker/partial fills.
        let user_ws_client = match polymarket_client_sdk_v2::clob::ws::Client::default()
            .authenticate(clob_client.credentials().clone(), clob_client.address())
        {
            Ok(client) => client,
            Err(e) => {
                error!(error = %e, "Failed to authenticate user order stream");
                sleep(connection_supervisor.connect_failed()).await;
                continue;
            }
        };
        let mut user_order_stream = match user_ws_client
            .subscribe_orders(markets.iter().map(|market| market.market_id).collect())
        {
            Ok(stream) => Box::pin(stream),
            Err(e) => {
                error!(error = %e, "Failed to subscribe authenticated user order stream");
                sleep(connection_supervisor.connect_failed()).await;
                continue;
            }
        };
        connection_supervisor.subscription_ready();
        service_health.set_connected(true);

        // Create position balance timer
        let balance_interval = if config.dry_run {
            0
        } else {
            config.position_balance_interval_secs
        };
        let mut balance_timer = if balance_interval > 0 {
            let mut timer = tokio::time::interval(Duration::from_secs(balance_interval));
            timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            timer.tick().await; // Fire first tick immediately
            Some(timer)
        } else {
            None
        };

        let session_runner = WindowSessionRunner::new(&markets, &config, runtime_metrics.clone());
        let mut event_loop_state = WindowEventLoopState::default();
        // Monitor orderbook updates
        loop {
            // Wind-down check: run once when <= N minutes to window end (stay in loop until natural switch by new-window detection)
            // Use second precision; num_minutes() truncation may miss in 5min windows
            if window_session
                .start_wind_down_if_due(Utc::now(), config.wind_down_before_window_end_minutes)
            {
                let seconds_until_end = window_session.remaining_seconds(Utc::now());
                info!(
                    "🛑 Wind-down triggered | {}s until window end",
                    seconds_until_end
                );
                wind_down_in_progress.store(true, Ordering::Relaxed);

                let wind_down = WindDownService::new(
                    config.clone(),
                    executor.clone(),
                    _risk_manager.clone(),
                    wind_down_in_progress.clone(),
                );
                let allowed_conditions = window_scope.condition_ids();
                let allowed_tokens = window_scope.token_ids();
                window_session.start_wind_down(async move {
                    wind_down.run(allowed_conditions, allowed_tokens).await;
                });
            }

            tokio::select! {
                user_event = user_order_stream.next() => {
                    match user_event {
                        Some(Ok(event)) => {
                            _risk_manager.observe_order_event(&event);
                            service_health.set_no_unresolved_orders(
                                !_risk_manager.has_unresolved_state(),
                            );
                        }
                        Some(Err(e)) => {
                            error!(error = %e, "Authenticated user order stream failed");
                            window_session.mark_user_stream_failed();
                            event_loop_state.exit(WindowExitReason::UserStreamDisconnected);
                            break;
                        }
                        None => {
                            warn!("Authenticated user order stream ended; recreating window streams");
                            window_session.mark_user_stream_failed();
                            event_loop_state.exit(WindowExitReason::UserStreamDisconnected);
                            break;
                        }
                    }
                }
                // Handle orderbook updates
                book_result = stream.next() => {
                    match book_result {
                        Some(Ok(book)) => {
                            // Recovery monitor observes the same public book stream as the strategy.
                            if let Err(e) = hedge_monitor.check_and_execute(&book).await {
                                warn!(error = %e, "Recovery monitor update failed");
                            }
                            // Process orderbook update (book will be moved)
                            if let Some(pair) = monitor.handle_book_update(book) {
                                let tick = match session_runner.process_pair(
                                    &pair,
                                    Utc::now().timestamp_millis(),
                                    window_scope.trading_paused() || live_safety.is_tripped(),
                                    connection_supervisor.is_healthy(),
                                    _risk_manager.has_unresolved_state(),
                                ) {
                                    Ok(tick) => tick,
                                    Err(error) => {
                                        debug!(%error, market_id = %pair.market_id, "Rejected unsafe orderbook pair");
                                        continue;
                                    }
                                };
                                let yes_best_ask = tick.yes_best_ask;
                                let no_best_ask = tick.no_best_ask;
                                let total_ask_price = tick.total_ask_price;
                                let yes_dir = tick.yes_direction;
                                let no_dir = tick.no_direction;
                                let market_display = tick.market_display;
                                let market_symbol = tick.market_symbol;

                                let (prefix, spread_info) = total_ask_price
                                    .map(|total| {
                                        if total < dec!(1) {
                                            let profit_pct = (dec!(1) - total) * dec!(100);
                                            ("🚨 Arbitrage", format!("total:{total:.4} profit:{profit_pct:.2}%"))
                                        } else {
                                            ("📊", format!("total:{total:.4} (no arb)"))
                                        }
                                    })
                                    .unwrap_or_else(|| ("📊", "no data".to_string()));
                                let is_arbitrage = prefix == "🚨 Arbitrage";
                                let yes_info = yes_best_ask
                                    .map(|(price, size)| {
                                        if is_arbitrage && !yes_dir.is_empty() {
                                            format!("Yes:{price:.4} size:{size} {yes_dir}")
                                        } else {
                                            format!("Yes:{price:.4} size:{size}")
                                        }
                                    })
                                    .unwrap_or_else(|| "Yes:none".to_string());
                                let no_info = no_best_ask
                                    .map(|(price, size)| {
                                        if is_arbitrage && !no_dir.is_empty() {
                                            format!("No:{price:.4} size:{size} {no_dir}")
                                        } else {
                                            format!("No:{price:.4} size:{size}")
                                        }
                                    })
                                    .unwrap_or_else(|| "No:none".to_string());

                                if use_tui {
                                    dashboard.with_mut(|state| {
                                        state.update_market(
                                            &symbol_short(&market_symbol),
                                            yes_best_ask.map(|(price, _)| decimal_to_f64(price)),
                                            no_best_ask.map(|(price, _)| decimal_to_f64(price)),
                                            is_arbitrage,
                                        );
                                    });
                                } else {
                                    info!(
                                        "{} {} | {} | {} | {}",
                                        prefix, market_display, yes_info, no_info, spread_info
                                    );
                                }
                                debug!(
                                    market_id = %pair.market_id,
                                    yes_token = %pair.yes_book.asset_id,
                                    no_token = %pair.no_book.asset_id,
                                    "Orderbook pair details"
                                );
                                let Some(opp) = tick.opportunity else {
                                    continue;
                                };
                                            if let Some(path) = config.arbitrage_log_file.clone() {
                                                let logged_opp = opp.clone();
                                                let logged_market = market_display.clone();
                                                tokio::task::spawn_blocking(move || {
                                                    if let Err(e) = utils::arbitrage_logger::log_arbitrage_opportunity(
                                                        &logged_opp,
                                                        &logged_market,
                                                        &path,
                                                    ) {
                                                        error!(error = %e, "Failed to persist arbitrage opportunity");
                                                    }
                                                });
                                            }
                                            // Use the exact slippage-adjusted quote for profitability,
                                            // notional sizing and risk reservation.
                                            let quote = match executor.execution_quote(&opp, yes_dir, no_dir).await {
                                                Ok(quote) => quote,
                                                Err(e) => {
                                                    runtime_metrics.record_quote_rejection();
                                                    debug!(error = %e, market = %market_display, "Final quote rejected");
                                                    continue;
                                                }
                                            };
                                            let order_size = quote.size;
                                            let total_cost = quote.total_cost;

                                            // Check risk exposure limit
                                            let position_tracker = _risk_manager.position_tracker();
                                            let current_exposure = position_tracker.calculate_exposure();

                                            let reservation_id = uuid::Uuid::new_v4().to_string();
                                            let Some(reservation) = position_tracker.reserve(reservation_id, total_cost) else {
                                                runtime_metrics.record_risk_rejection();
                                                warn!(
                                                    "�� ️ Risk exposure limit exceeded, skip arbitrage | market:{} | exposure:{:.2} USD | order cost:{:.2} USD | limit:{:.2} USD",
                                                    market_display,
                                                    current_exposure,
                                                    total_cost,
                                                    position_tracker.max_exposure()
                                                );
                                                continue; // Skip this arbitrage
                                            };

                                            // Check position balance (local cache, zero latency)
                                            if position_balancer.should_skip_arbitrage(opp.yes_token_id, opp.no_token_id) {
                                                runtime_metrics.record_balance_rejection();
                                                warn!(
                                                    "�� ️ Positions severely imbalanced, skip arbitrage | market:{}",
                                                    market_display
                                                );
                                                continue; // Skip this arbitrage
                                            }

                                            // Check trade interval: min 3s between trades
                                            {
                                                let mut guard = last_trade_time.lock().await;
                                                let now = Instant::now();
                                                if let Some(last) = *guard {
                                                    if now.saturating_duration_since(last) < MIN_TRADE_INTERVAL {
                                                        let elapsed = now.saturating_duration_since(last).as_secs_f32();
                                                        debug!(
                                                            "⏱️ Trade interval < 3s, skip | market:{} | since last:{}s",
                                                            market_display,
                                                            elapsed
                                                        );
                                                        continue; // Skip this arb
                                                    }
                                                }
                                                *guard = Some(now);
                                            }

                                            if let Err(reason) = live_safety.try_start_pair(
                                                window_session.timestamp(),
                                                &quote,
                                            ) {
                                                runtime_metrics.record_risk_rejection();
                                                warn!(%reason, "Live/canary safety gate rejected order pair");
                                                continue;
                                            }

                                            let trade_sym = symbol_short(&market_symbol);
                                            if use_tui {
                                                dashboard.with_mut(|d| {
                                                    d.set_exposure(decimal_to_f64(current_exposure));
                                                    d.record_trade_attempt(
                                                        &trade_sym,
                                                        decimal_to_f64(quote.edge_per_share * dec!(100)),
                                                        decimal_to_f64(order_size),
                                                        decimal_to_f64(total_cost),
                                                    );
                                                });
                                            } else {
                                                info!(
                                                    "⚡ Execute arbitrage | market:{} | profit:{:.2}% | size:{} | cost:{:.2} USD | exposure:{:.2} USD",
                                                    market_display,
                                                    quote.edge_per_share * dec!(100),
                                                    order_size,
                                                    total_cost,
                                                    current_exposure
                                                );
                                            }
                                            window_session.spawn_trade(strategy_service.clone().execute(
                                                opp,
                                                quote,
                                                trade_sym,
                                                reservation,
                                            ));
                            }
                        }
                        Some(Err(e)) => {
                            error!(error = %e, "Orderbook update error");
                            event_loop_state.exit(WindowExitReason::MarketStreamDisconnected);
                            // Stream error, recreate stream
                            break;
                        }
                        None => {
                            warn!("Orderbook stream ended, recreating");
                            event_loop_state.exit(WindowExitReason::MarketStreamDisconnected);
                            break;
                        }
                    }
                }

                // Position balance task
                _ = async {
                    if let Some(ref mut timer) = balance_timer {
                        timer.tick().await;
                        if let Err(e) = position_balancer.check_and_balance_positions(&market_token_map).await {
                            warn!(error = %e, "Position balance check failed");
                        }
                    } else {
                        futures::future::pending::<()>().await;
                    }
                } => {
                    // Balance task done
                }

                // Periodic check: 1) new 5min window 2) wind-down trigger
                _ = sleep(Duration::from_secs(1)) => {
                    if shutdown.load(Ordering::Relaxed) {
                        return Ok(());
                    }

                    let now = Utc::now();
                    let seconds_until_end = window_session.remaining_seconds(now);
                    let exposure = _risk_manager.position_tracker().calculate_exposure();
                    service_health.set_journal_writable(journal.probe_writable().is_ok());
                    let disk_healthy = fs2::available_space(journal_dir)
                        .map(|bytes| bytes >= MIN_READY_DISK_BYTES)
                        .unwrap_or(false);
                    service_health.set_disk_healthy(disk_healthy);
                    service_health.set_no_unresolved_orders(!_risk_manager.has_unresolved_state());
                    service_health.set_safety_ok(!live_safety.is_tripped());
                    dashboard.with_mut(|d| {
                        d.set_window(&window_label, seconds_until_end);
                        d.set_exposure(decimal_to_f64(exposure));
                    });

                    if window_session.has_rolled_over(now, config.market_period_minutes) {
                        let new_window_timestamp =
                            _scheduler.calculate_current_window_timestamp(now);
                        info!(
                            old_window = window_session.timestamp(),
                            new_window = new_window_timestamp,
                            "New 5min window detected, cancelling old subscriptions and switching"
                        );
                        // Drop stream to release monitor borrow, then clear old subs
                        drop(stream);
                        monitor.clear();
                        event_loop_state.exit(WindowExitReason::Rollover);
                        break;
                    }
                }
            }
        }

        // Avoid starting a new window while old tasks still change account state.
        window_session.finish(Duration::from_secs(30)).await;

        if _risk_manager.has_unresolved_state() {
            _risk_manager.reconcile_owned_orders(&clob_client).await;
            hedge_monitor.reconcile_owned_orders().await;
        }
        service_health.set_no_unresolved_orders(!_risk_manager.has_unresolved_state());
        service_health.set_safety_ok(!live_safety.is_tripped());
        if event_loop_state.requires_reconnect_backoff() {
            service_health.set_connected(false);
            runtime_metrics.record_stream_reconnect();
            dashboard.with_mut(|d| d.set_connected(false));
            let reconnect_delay = connection_supervisor.disconnected(window_session.elapsed());
            warn!(
                delay_secs = reconnect_delay.as_secs(),
                "User stream disconnected; new entries paused during reconnect backoff"
            );
            sleep(reconnect_delay).await;
        } else {
            service_health.set_connected(false);
            connection_supervisor.normal_session_end();
        }
        debug!(reason = ?event_loop_state.exit_reason(), "Window event loop exited");

        match serde_json::to_string(&runtime_metrics.snapshot()) {
            Ok(snapshot) => info!(metrics = %snapshot, "Runtime metrics snapshot"),
            Err(error) => warn!(%error, "Failed to serialize runtime metrics"),
        }

        info!("Current window monitoring ended, refreshing markets for next round");
    }
}
