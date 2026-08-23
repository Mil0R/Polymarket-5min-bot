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
mod winner_buy;
mod winner_model;
use polypulse::ui::control::{StrategyControls, StrategyKind};
use polypulse::ui::{decimal_to_f64, spawn_dashboard_thread, symbol_short, DashboardHandle};

use anyhow::Result;
use futures::StreamExt;
use rust_decimal_macros::dec;
use std::collections::HashMap;
use std::io::{stdout, IsTerminal};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tokio::time::sleep;
use tracing::{debug, error, info, trace, warn};

use crate::config::Config;
use crate::connection_supervisor::ConnectionSupervisor;
use crate::live_safety::{CanaryPolicy, LiveSafetyGuard};
use crate::market::{MarketDiscoverer, MarketScheduler};
use crate::monitor::{MarketWsConnectionState, OrderBookMonitor};
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
use crate::winner_buy::{
    detect_winner_asks, fee_inclusive_capped_size, received_at_is_fresh, settle_winner_buy,
    PendingWinnerBuy, SettlementBackoff, WinnerBuyJournal, WinnerBuyRiskPolicy, WinnerEvaluation,
    WinnerEvaluationJournal,
};
use crate::winner_model::{
    attempt_allowed, run_chainlink_twap_feed, ChainlinkTwapStore, WinnerConfirmationTracker,
    WinnerModelInput, WinnerModelPolicy,
};

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
        tracing::info!("Polymarket 5-minute bot starting (dashboard mode)");
    } else {
        tracing::info!("Polymarket 5-minute arbitrage bot starting");
    }

    // Load config
    let config = Config::from_env()?;
    tracing::info!("Config loaded");
    // Dry-run TUI may hot-start Winner Buy after provisioning its read-only
    // dependencies. Live mode requires explicit startup configuration so all
    // credential and compatibility validation remains fail-closed.
    let winner_buy_available = config.winner_buy_enabled || (use_tui && config.dry_run);
    let strategy_controls = Arc::new(StrategyControls::new(
        config.arbitrage_enabled,
        config.winner_buy_enabled,
        winner_buy_available,
        !config.dry_run,
    ));
    let run_id = uuid::Uuid::new_v4();
    let execution_mode = if config.dry_run {
        "paper"
    } else if config.canary_mode {
        "canary"
    } else {
        "live"
    };
    info!(%run_id, mode = execution_mode, "Run started");
    info!(
        target: "polypulse::strategy::spread_arbitrage",
        %run_id,
        mode = execution_mode,
        state_file = %config.state_journal_file,
        opportunity_file = ?config.arbitrage_log_file,
        shadow_file = ?config.arbitrage_shadow_log_file,
        enabled = config.arbitrage_enabled,
        "Spread Arbitrage run started"
    );
    if winner_buy_available {
        info!(
            target: "polypulse::strategy::winner_buy",
            %run_id,
            mode = execution_mode,
            state_file = %config.winner_buy_state_journal_file,
            evaluation_file = %config.winner_buy_evaluation_journal_file,
            enabled = config.winner_buy_enabled,
            "Winner Buy run started"
        );
    }

    let runtime_metrics = Arc::new(RuntimeMetrics::default());
    let winner_twap_store = Arc::new(ChainlinkTwapStore::default());
    if winner_buy_available {
        tokio::spawn(run_chainlink_twap_feed(
            config.winner_buy_rtds_url.clone(),
            winner_twap_store.clone(),
            shutdown.clone(),
            runtime_metrics.clone(),
        ));
    }

    let order_mode = format!("{:?}", config.arbitrage_order_type);
    let dashboard = DashboardHandle::new_live(order_mode, config.risk_max_exposure_usdc);
    dashboard.with_mut(|dashboard| {
        dashboard.set_execution_mode(if config.dry_run {
            "DRY RUN"
        } else if config.canary_mode {
            "CANARY"
        } else {
            "LIVE"
        });
        dashboard.set_strategy_states(
            strategy_controls.state(StrategyKind::Arbitrage),
            strategy_controls.state(StrategyKind::WinnerBuy),
            strategy_controls.blocked_reason(),
        );
    });
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
        spawn_dashboard_thread(dashboard.arc(), shutdown.clone(), strategy_controls.clone());
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
            paper_winner_buy_delay_ms: config.paper_winner_buy_delay_ms,
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

    let winner_buy_journal = if winner_buy_available {
        let journal = Arc::new(WinnerBuyJournal::open(
            &config.winner_buy_state_journal_file,
        )?);
        let restored = journal.pending().len();
        if restored > 0 {
            warn!(target: "polypulse::strategy::winner_buy",
                count = restored,
                path = %journal.path().display(),
                "Restored pending winner-buy positions"
            );
        }
        let awaiting_redeem = journal.awaiting_redeem().len();
        if awaiting_redeem > 0 {
            warn!(target: "polypulse::strategy::winner_buy",
                count = awaiting_redeem,
                path = %journal.path().display(),
                "Restored resolved winner-buy positions awaiting redemption"
            );
        }
        let uncertain_redeems = journal.uncertain_redeem_count();
        if uncertain_redeems > 0 {
            live_safety.trip(format!(
                "{uncertain_redeems} winner-buy redemption(s) have uncertain write state"
            ));
            service_health.set_safety_ok(false);
        }
        info!(target: "polypulse::strategy::winner_buy", snapshot = ?journal.risk_snapshot(), "Restored Winner Buy risk history");
        let summary = journal.paper_summary();
        if let Some(snapshot) = executor.restore_paper_winner_history(
            summary.pending_cost,
            summary.total_spent,
            summary.realized_pnl,
            summary.open_positions,
        ) {
            info!(target: "polypulse::strategy::winner_buy",
                cash_remaining = %snapshot.cash_remaining,
                pending_cost = %summary.pending_cost,
                realized_pnl = %summary.realized_pnl,
                open_positions = summary.open_positions,
                "Restored Winner Buy paper cash lifecycle"
            );
        }
        let position_tracker = _risk_manager.position_tracker();
        for pending in journal
            .pending()
            .into_iter()
            .chain(journal.awaiting_redeem())
        {
            let Ok(token) = polymarket_client_sdk::types::U256::from_str(&pending.token_id) else {
                warn!(target: "polypulse::strategy::winner_buy",
                    position_id = %pending.position_id,
                    token_id = %pending.token_id,
                    "Could not restore Winner Buy token into PositionTracker"
                );
                continue;
            };
            let existing = position_tracker.get_position(token);
            let missing = (pending.filled - existing).max(dec!(0));
            if missing > dec!(0) {
                position_tracker.record_buy_fill(token, pending.price, missing);
            }
        }
        Some(journal)
    } else {
        None
    };
    let winner_evaluation_journal = if winner_buy_available {
        let journal = Arc::new(WinnerEvaluationJournal::open(
            &config.winner_buy_evaluation_journal_file,
        )?);
        info!(target: "polypulse::strategy::winner_buy", path = %journal.path().display(), "Winner Buy counterfactual journal ready");
        Some(journal)
    } else {
        None
    };
    let protected_winner_tokens = Arc::new(RwLock::new(
        winner_buy_journal
            .as_ref()
            .map(|journal| {
                journal
                    .pending()
                    .into_iter()
                    .chain(journal.awaiting_redeem())
                    .filter_map(|buy| {
                        polymarket_client_sdk::types::U256::from_str(&buy.token_id).ok()
                    })
                    .collect()
            })
            .unwrap_or_default(),
    ));
    if let Some(journal) = winner_buy_journal.as_ref() {
        runtime_metrics.set_winner_buy_pending(journal.pending().len());
    }
    let mut last_winner_settle_check = Instant::now();
    let mut winner_position_settlement_backoff = SettlementBackoff::default();
    let mut winner_evaluation_settlement_backoff = SettlementBackoff::default();
    let winner_buy_risk_policy = WinnerBuyRiskPolicy {
        max_strategy_loss: rust_decimal::Decimal::try_from(
            config.winner_buy_max_strategy_loss_usdc,
        )?,
        max_asset_loss: rust_decimal::Decimal::try_from(config.winner_buy_max_asset_loss_usdc)?,
        max_consecutive_losses: config.winner_buy_max_consecutive_losses,
    };

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

        // V2 fees are market-specific. Resolve them once per window instead of
        // treating the legacy static arbitrage buffer as a Winner Buy fee.
        let mut winner_fee_policies = HashMap::new();
        if winner_buy_available {
            for market in &markets {
                match executor.winner_fee_policy(market.market_id).await {
                    Ok(policy) => {
                        debug!(target: "polypulse::strategy::winner_buy",
                            market = %market.crypto_symbol,
                            fee_rate = %policy.rate,
                            fee_exponent = policy.exponent,
                            "Loaded Winner Buy V2 fee policy"
                        );
                        winner_fee_policies.insert(market.slug.clone(), policy);
                    }
                    Err(error) => {
                        runtime_metrics.record_winner_buy_fee_read_error();
                        warn!(
                            %error,
                            market = %market.crypto_symbol,
                            "Winner Buy fee policy unavailable; this market will be skipped"
                        );
                    }
                }
            }
        }

        // Create orderbook stream
        let mut stream = match monitor.create_orderbook_stream() {
            Ok(stream) => stream,
            Err(e) => {
                error!(error = %e, "Failed to create orderbook stream");
                runtime_metrics.record_stream_connect_failure();
                runtime_metrics.record_stream_reconnect();
                sleep(connection_supervisor.connect_failed()).await;
                continue;
            }
        };
        let mut market_ws_reconnecting = false;
        let mut market_ws_seen_connected = matches!(
            monitor.connection_state(),
            MarketWsConnectionState::Connected
        );

        info!(market_count = markets.len(), "Monitoring orderbook");

        // Window-end winner-buy: attempt counter reset on each window rollover.
        let winner_buy_count = Arc::new(AtomicU32::new(0));
        let mut winner_attempts_by_market: HashMap<String, u32> = HashMap::new();
        let mut winner_confirmations = WinnerConfirmationTracker::default();
        if winner_buy_available {
            info!(target: "polypulse::strategy::winner_buy",
                before_end_secs = config.winner_buy_before_end_secs,
                min_price = config.winner_buy_min_price,
                max_price = config.winner_buy_max_price,
                max_order_usdc = config.winner_buy_max_order_usdc,
                "Winner-buy strategy enabled"
            );
        }

        use chrono::Utc;
        let mut window_session =
            WindowSession::new(&markets, Utc::now(), config.market_period_minutes)?;
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
                runtime_metrics.record_stream_connect_failure();
                runtime_metrics.record_stream_reconnect();
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
                runtime_metrics.record_stream_connect_failure();
                runtime_metrics.record_stream_reconnect();
                sleep(connection_supervisor.connect_failed()).await;
                continue;
            }
        };
        let mut user_ws_seen_connected = user_ws_client
            .connection_state(polymarket_client_sdk_v2::clob::ws::ChannelType::User)
            .is_connected();
        let mut user_ws_reconnecting = false;
        if connection_supervisor.subscription_ready() {
            runtime_metrics.record_stream_recovery();
            info!("CLOB streams recovered after reconnect");
        }
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
                let protected_winner_tokens = protected_winner_tokens.clone();
                window_session.start_wind_down(async move {
                    wind_down
                        .run(allowed_conditions, allowed_tokens, protected_winner_tokens)
                        .await;
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
                            runtime_metrics.record_market_ws_book(book.timestamp);
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
                                        debug!(target: "polypulse::strategy::spread_arbitrage", %error, market_id = %pair.market_id, "Rejected unsafe orderbook pair");
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
                                let shadow_candidate = tick.shadow_candidate;

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
                                    info!(target: "polypulse::strategy::spread_arbitrage",
                                        "{} {} | {} | {} | {}",
                                        prefix, market_display, yes_info, no_info, spread_info
                                    );
                                }
                                trace!(target: "polypulse::strategy::spread_arbitrage",
                                    market_id = %pair.market_id,
                                    yes_token = %pair.yes_book.asset_id,
                                    no_token = %pair.no_book.asset_id,
                                    "Orderbook pair details"
                                );
                                if let (Some(candidate), Some(path)) = (
                                    shadow_candidate,
                                    config.arbitrage_shadow_log_file.clone(),
                                ) {
                                    let logged_market = market_display.clone();
                                    tokio::task::spawn_blocking(move || {
                                        let observed_at_ms = candidate
                                            .first_seen_at_ms
                                            .saturating_add(candidate.observed_duration_ms);
                                        if let Err(error) = utils::arbitrage_logger::log_arbitrage_shadow_candidate(
                                            &candidate,
                                            &logged_market,
                                            observed_at_ms,
                                            &path,
                                        ) {
                                            error!(target: "polypulse::strategy::spread_arbitrage", %error, "Failed to persist spread shadow candidate");
                                        }
                                    });
                                }
                                let Some(opp) = tick.opportunity else {
                                    continue;
                                };
                                if !strategy_controls.arbitrage_enabled() {
                                    debug!(target: "polypulse::strategy::spread_arbitrage", market = %market_display, "Spread arbitrage entry paused by operator");
                                    continue;
                                }
                                            if let Some(path) = config.arbitrage_log_file.clone() {
                                                let logged_opp = opp.clone();
                                                let logged_market = market_display.clone();
                                                tokio::task::spawn_blocking(move || {
                                                    if let Err(e) = utils::arbitrage_logger::log_arbitrage_opportunity(
                                                        &logged_opp,
                                                        &logged_market,
                                                        &path,
                                                    ) {
                                                        error!(target: "polypulse::strategy::spread_arbitrage", error = %e, "Failed to persist arbitrage opportunity");
                                                    }
                                                });
                                            }
                                            // Use the exact slippage-adjusted quote for profitability,
                                            // notional sizing and risk reservation.
                                            let quote = match executor.execution_quote(&opp, yes_dir, no_dir).await {
                                                Ok(quote) => quote,
                                                Err(e) => {
                                                    runtime_metrics.record_quote_rejection();
                                                    debug!(target: "polypulse::strategy::spread_arbitrage", error = %e, market = %market_display, "Final quote rejected");
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
                                                warn!(target: "polypulse::strategy::spread_arbitrage",
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
                                                warn!(target: "polypulse::strategy::spread_arbitrage",
                                                    "�� ️ Positions severely imbalanced, skip arbitrage | market:{}",
                                                    market_display
                                                );
                                                continue; // Skip this arbitrage
                                            }

                                            if !strategy_controls.arbitrage_enabled() {
                                                debug!(target: "polypulse::strategy::spread_arbitrage", market = %market_display, "Spread arbitrage paused before submission");
                                                continue;
                                            }

                                            // Check trade interval: min 3s between trades
                                            {
                                                let mut guard = last_trade_time.lock().await;
                                                let now = Instant::now();
                                                if let Some(last) = *guard {
                                                    if now.saturating_duration_since(last) < MIN_TRADE_INTERVAL {
                                                        let elapsed = now.saturating_duration_since(last).as_secs_f32();
                                                        debug!(target: "polypulse::strategy::spread_arbitrage",
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
                                                warn!(target: "polypulse::strategy::spread_arbitrage", %reason, "Live/canary safety gate rejected order pair");
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
                                                info!(target: "polypulse::strategy::spread_arbitrage",
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
                    match monitor.connection_state() {
                        MarketWsConnectionState::Connected if market_ws_reconnecting => {
                            runtime_metrics.record_sdk_market_ws_recovery();
                            market_ws_reconnecting = false;
                            market_ws_seen_connected = true;
                            info!("SDK market WebSocket recovered internally");
                        }
                        MarketWsConnectionState::Reconnecting(attempt) => {
                            if market_ws_seen_connected && !market_ws_reconnecting {
                                runtime_metrics.record_sdk_market_ws_reconnect();
                                warn!(attempt, "SDK market WebSocket reconnecting internally");
                            }
                            market_ws_reconnecting = market_ws_seen_connected;
                        }
                        MarketWsConnectionState::Disconnected
                        | MarketWsConnectionState::Connecting => {
                            if market_ws_seen_connected && !market_ws_reconnecting {
                                runtime_metrics.record_sdk_market_ws_reconnect();
                            }
                            market_ws_reconnecting = market_ws_seen_connected;
                        }
                        MarketWsConnectionState::Connected => {
                            market_ws_seen_connected = true;
                        }
                    }
                    let user_ws_connected = user_ws_client
                        .connection_state(polymarket_client_sdk_v2::clob::ws::ChannelType::User)
                        .is_connected();
                    if user_ws_connected {
                        user_ws_seen_connected = true;
                        if user_ws_reconnecting {
                            runtime_metrics.record_sdk_user_ws_recovery();
                            user_ws_reconnecting = false;
                            info!("SDK user WebSocket recovered internally");
                        }
                    } else if user_ws_seen_connected && !user_ws_reconnecting {
                        runtime_metrics.record_sdk_user_ws_reconnect();
                        user_ws_reconnecting = true;
                        warn!("SDK user WebSocket reconnecting internally");
                    }
                    let seconds_until_end = window_session.remaining_seconds(now);
                    let exposure = _risk_manager.position_tracker().calculate_exposure();
                    let journal_writable = journal.probe_writable().is_ok();
                    service_health.set_journal_writable(journal_writable);
                    let disk_healthy = fs2::available_space(journal_dir)
                        .map(|bytes| bytes >= MIN_READY_DISK_BYTES)
                        .unwrap_or(false);
                    service_health.set_disk_healthy(disk_healthy);
                    service_health.set_no_unresolved_orders(!_risk_manager.has_unresolved_state());
                    service_health.set_safety_ok(!live_safety.is_tripped());
                    let entries_ready = matches!(
                        monitor.connection_state(),
                        MarketWsConnectionState::Connected
                    ) && user_ws_connected
                        && connection_supervisor.is_healthy()
                        && journal_writable
                        && disk_healthy
                        && !_risk_manager.has_unresolved_state()
                        && !live_safety.is_tripped();
                    let blocked_reason = if live_safety.is_tripped() {
                        "safety gate tripped"
                    } else if _risk_manager.has_unresolved_state() {
                        "unresolved order state"
                    } else if !journal_writable || !disk_healthy {
                        "journal or disk unhealthy"
                    } else if !user_ws_connected
                        || !matches!(monitor.connection_state(), MarketWsConnectionState::Connected)
                    {
                        "market/user stream disconnected"
                    } else if !connection_supervisor.is_healthy() {
                        "connection supervisor unhealthy"
                    } else {
                        "ready"
                    };
                    strategy_controls.set_resume_allowed(entries_ready, blocked_reason);
                    dashboard.with_mut(|d| {
                        d.set_window(&window_label, seconds_until_end);
                        d.set_exposure(decimal_to_f64(exposure));
                        let metrics = runtime_metrics.snapshot();
                        d.set_winner_buy_funnel(
                            metrics.winner_buy_scans,
                            metrics.winner_buy_signals,
                            metrics.winner_buy_attempts,
                            metrics.winner_buy_model_data_rejections
                                + metrics.winner_buy_model_market_rejections
                                + metrics.winner_buy_model_boundary_rejections
                                + metrics.winner_buy_model_ev_rejections,
                            metrics.winner_buy_awaiting_confirmation,
                            metrics.winner_buy_pending,
                            metrics.winner_buy_resolved,
                            metrics.winner_buy_redeemed,
                        );
                        d.set_strategy_metrics(
                            metrics.opportunities_admitted,
                            metrics.opportunities_rejected,
                            metrics.depth_rejections,
                            metrics.risk_rejections,
                            metrics.quote_rejections,
                            metrics.trades_started,
                            metrics.trades_succeeded,
                            metrics.trades_failed,
                            metrics.paper_one_sided,
                            metrics.paper_net_pnl_microusd as f64 / 1_000_000.0,
                            metrics.winner_buy_model_data_rejections,
                            metrics.winner_buy_model_market_rejections,
                            metrics.winner_buy_model_boundary_rejections,
                            metrics.winner_buy_model_ev_rejections,
                            metrics.winner_buy_shadow_ev_failures,
                        );
                        d.set_strategy_states(
                            strategy_controls.state(StrategyKind::Arbitrage),
                            strategy_controls.state(StrategyKind::WinnerBuy),
                            strategy_controls.blocked_reason(),
                        );
                        let rtds_healthy = !winner_buy_available
                            || now.timestamp_millis().saturating_sub(
                                metrics.winner_buy_rtds_last_observed_at_ms,
                            ) <= config.winner_buy_feed_max_age_ms;
                        d.set_runtime_health(
                            matches!(
                                monitor.connection_state(),
                                MarketWsConnectionState::Connected
                            ),
                            user_ws_connected,
                            rtds_healthy,
                            !live_safety.is_tripped(),
                        );
                    });

                    if !strategy_controls.winner_buy_enabled() {
                        winner_confirmations.clear();
                    }

                    // Window-end winner-buy (strategy B): when the window is
                    // nearly over and one side's ask is decided (>= min price)
                    // while the other has no/cheap asks, buy the winner and hold
                    // to settlement (pays 1.00). One attempt per window.
                    if strategy_controls.winner_buy_enabled() && seconds_until_end <= 120 {
                        debug!(target: "polypulse::strategy::winner_buy",
                            seconds_until_end,
                            enabled = strategy_controls.winner_buy_enabled(),
                            count = winner_buy_count.load(Ordering::Relaxed),
                            max_per_window = config.winner_buy_max_per_window,
                            "winner-buy window check"
                        );
                    }

                    // Settle paper winner-buy positions whose markets have
                    // resolved (~every 10s). Realized PnL reflects the ACTUAL
                    // win rate, unlike the optimistic est_pnl.
                    if winner_buy_available
                        && last_winner_settle_check.elapsed() >= Duration::from_secs(10)
                    {
                        last_winner_settle_check = Instant::now();
                        let winner_journal = winner_buy_journal
                            .as_ref()
                            .expect("winner-buy journal initialized");
                        let pending = winner_journal.pending();
                        if !pending.is_empty() {
                            for buy in pending {
                                if !winner_position_settlement_backoff
                                    .should_query(&buy.slug, now.timestamp_millis())
                                {
                                    continue;
                                }
                                match _scheduler
                                    .fetch_closed_markets_by_slug(vec![buy.slug.clone()])
                                    .await
                                {
                                    Ok(markets) => {
                                        if markets.is_empty() {
                                            runtime_metrics
                                                .record_winner_buy_settlement_empty_result();
                                            let retry_in_secs = winner_position_settlement_backoff
                                                .record_retry(&buy.slug, now.timestamp_millis());
                                            debug!(target: "polypulse::strategy::winner_buy",
                                                slug = %buy.slug,
                                                retry_in_secs,
                                                "Winner-buy closed-market settlement query returned no market"
                                            );
                                            continue;
                                        }
                                        let settlement = markets.iter().find_map(|m| {
                                            let prices = m.outcome_prices.as_ref()?;
                                            if prices.len() == 2 {
                                                Some((
                                                    m.closed.unwrap_or(false),
                                                    (prices[0], prices[1]),
                                                ))
                                            } else {
                                                None
                                            }
                                        });
                                        if let Some(pnl) = settlement.and_then(|(closed, prices)| {
                                            settle_winner_buy(&buy, now, closed, &prices)
                                        }) {
                                            winner_position_settlement_backoff.clear(&buy.slug);
                                            match winner_journal.record_settled(&buy, pnl) {
                                                Ok(true) => {
                                                    runtime_metrics.record_winner_buy_resolved();
                                                    runtime_metrics
                                                        .record_winner_buy_asset_resolved(
                                                            &buy.asset_key(),
                                                        );
                                                }
                                                Ok(false) => continue,
                                                Err(e) => {
                                                    error!(target: "polypulse::strategy::winner_buy",
                                                        error = %e,
                                                        position_id = %buy.position_id,
                                                        slug = %buy.slug,
                                                        "Winner-buy settlement persistence failed"
                                                    );
                                                    continue;
                                                }
                                            }
                                            let won = pnl > dec!(0);
                                            let cost = buy.filled * buy.price;
                                            let payout = if won { buy.filled } else { dec!(0) };
                                            if let Some(snapshot) =
                                                executor.settle_paper_winner(cost, payout)
                                            {
                                                info!(target: "polypulse::strategy::winner_buy",
                                                    cash_remaining = %snapshot.cash_remaining,
                                                    winner_realized_pnl = %snapshot.winner_realized_pnl,
                                                    winner_positions_open = snapshot.winner_positions_open,
                                                    "PAPER winner-buy cash settled"
                                                );
                                            }
                                            let completed_without_live_redeem =
                                                config.dry_run || !won;
                                            if completed_without_live_redeem {
                                                let lifecycle_result = if won {
                                                    winner_journal
                                                        .record_redeem_submitted(&buy)
                                                        .and_then(|_| {
                                                            winner_journal.record_redeemed(
                                                                &buy,
                                                                "paper:payout-confirmed".into(),
                                                            )
                                                        })
                                                } else {
                                                    winner_journal.record_redeemed(
                                                        &buy,
                                                        "not-required:losing-token".into(),
                                                    )
                                                };
                                                match lifecycle_result {
                                                    Ok(true) => {
                                                        runtime_metrics.record_winner_buy_redeemed();
                                                        if let Ok(token) = polymarket_client_sdk::types::U256::from_str(&buy.token_id) {
                                                            protected_winner_tokens
                                                                .write()
                                                                .expect("winner token lock poisoned")
                                                                .remove(&token);
                                                            _risk_manager
                                                                .position_tracker()
                                                                .record_sell_fill(token, buy.filled);
                                                        }
                                                    }
                                                    Ok(false) => {
                                                        live_safety.trip(format!(
                                                            "winner-buy completion {} violated lifecycle state",
                                                            buy.position_id
                                                        ));
                                                        service_health.set_safety_ok(false);
                                                    }
                                                    Err(error) => {
                                                        live_safety.trip(format!(
                                                            "winner-buy completion {} could not be persisted: {error}",
                                                            buy.position_id
                                                        ));
                                                        service_health.set_safety_ok(false);
                                                    }
                                                }
                                            }
                                            live_safety.record_winner_settlement(cost, won);
                                            runtime_metrics.record_winner_buy_settlement(pnl);
                                            runtime_metrics
                                                .set_winner_buy_pending(winner_journal.pending().len());
                                            dashboard.with_mut(|d| {
                                                d.record_winner_buy_settlement(
                                                    decimal_to_f64(pnl),
                                                    pnl > dec!(0),
                                                );
                                            });
                                            info!(target: "polypulse::strategy::winner_buy",
                                                slug = %buy.slug,
                                                %pnl,
                                                "🏁 Winner-buy settled (realized)"
                                            );
                                        } else {
                                            runtime_metrics
                                                .record_winner_buy_settlement_not_terminal();
                                            let retry_in_secs = winner_position_settlement_backoff
                                                .record_retry(&buy.slug, now.timestamp_millis());
                                            debug!(target: "polypulse::strategy::winner_buy",
                                                slug = %buy.slug,
                                                retry_in_secs,
                                                "Winner-buy market found but outcome is not terminal"
                                            );
                                        }
                                    }
                                    Err(e) => {
                                        runtime_metrics.record_winner_buy_settlement_query_failure();
                                        let retry_in_secs = winner_position_settlement_backoff
                                            .record_retry(&buy.slug, now.timestamp_millis());
                                        warn!(target: "polypulse::strategy::winner_buy",
                                            error = %e,
                                            slug = %buy.slug,
                                            retry_in_secs,
                                            "Winner-buy settlement query failed"
                                        );
                                    }
                                }
                            }
                        }
                        let evaluation_journal = winner_evaluation_journal
                            .as_ref()
                            .expect("Winner Buy evaluation journal initialized");
                        for slug in evaluation_journal.due_slugs(now) {
                            if !winner_evaluation_settlement_backoff
                                .should_query(&slug, now.timestamp_millis())
                            {
                                continue;
                            }
                            match _scheduler
                                .fetch_closed_markets_by_slug(vec![slug.clone()])
                                .await
                            {
                                Ok(markets) => {
                                    if markets.is_empty() {
                                        runtime_metrics
                                            .record_winner_buy_settlement_empty_result();
                                        let retry_in_secs = winner_evaluation_settlement_backoff
                                            .record_retry(&slug, now.timestamp_millis());
                                        debug!(target: "polypulse::strategy::winner_buy",
                                            %slug,
                                            retry_in_secs,
                                            "Winner Buy evaluation closed-market query returned no market"
                                        );
                                        continue;
                                    }
                                    let settlement = markets.iter().find_map(|market| {
                                        let prices = market.outcome_prices.as_ref()?;
                                        (prices.len() == 2).then_some((
                                            market.closed.unwrap_or(false),
                                            (prices[0], prices[1]),
                                        ))
                                    });
                                    if let Some((closed, prices)) = settlement {
                                        match evaluation_journal
                                            .record_resolution(&slug, now, closed, &prices)
                                        {
                                            Ok(resolved) if resolved > 0 => {
                                                winner_evaluation_settlement_backoff.clear(&slug);
                                                runtime_metrics
                                                    .record_winner_buy_evaluations_resolved(resolved);
                                                debug!(target: "polypulse::strategy::winner_buy",
                                                    %slug,
                                                    resolved,
                                                    "Winner Buy counterfactual evaluations resolved"
                                                );
                                            }
                                            Ok(_) => {
                                                runtime_metrics
                                                    .record_winner_buy_settlement_not_terminal();
                                                winner_evaluation_settlement_backoff
                                                    .record_retry(&slug, now.timestamp_millis());
                                            }
                                            Err(error) => {
                                                winner_evaluation_settlement_backoff
                                                    .record_retry(&slug, now.timestamp_millis());
                                                error!(target: "polypulse::strategy::winner_buy",
                                                    %error,
                                                    %slug,
                                                    "Winner Buy counterfactual resolution persistence failed"
                                                );
                                            }
                                        }
                                    } else {
                                        runtime_metrics
                                            .record_winner_buy_settlement_not_terminal();
                                        winner_evaluation_settlement_backoff
                                            .record_retry(&slug, now.timestamp_millis());
                                    }
                                }
                                Err(error) => {
                                    runtime_metrics.record_winner_buy_settlement_query_failure();
                                    let retry_in_secs = winner_evaluation_settlement_backoff
                                        .record_retry(&slug, now.timestamp_millis());
                                    warn!(target: "polypulse::strategy::winner_buy",
                                        %error,
                                        %slug,
                                        retry_in_secs,
                                        "Winner Buy counterfactual resolution query failed"
                                    );
                                }
                            }
                        }
                        if !config.dry_run && config.winner_buy_auto_redeem {
                            let proxy = config
                                .proxy_address
                                .expect("auto-redeem configuration requires proxy");
                            for buy in winner_journal.awaiting_redeem() {
                                let condition_id = match polymarket_client_sdk::types::B256::from_str(
                                    &buy.condition_id,
                                ) {
                                    Ok(value) => value,
                                    Err(error) => {
                                        live_safety.trip(format!(
                                            "winner-buy {} has invalid condition id: {error}",
                                            buy.position_id
                                        ));
                                        service_health.set_safety_ok(false);
                                        continue;
                                    }
                                };
                                let token = match polymarket_client_sdk::types::U256::from_str(
                                    &buy.token_id,
                                ) {
                                    Ok(value) => value,
                                    Err(error) => {
                                        live_safety.trip(format!(
                                            "winner-buy {} has invalid token id: {error}",
                                            buy.position_id
                                        ));
                                        service_health.set_safety_ok(false);
                                        continue;
                                    }
                                };
                                let raw_size = match polymarket_client_sdk::types::U256::from_str(
                                    &(buy.filled * dec!(1_000_000)).floor().to_string(),
                                ) {
                                    Ok(value) => value,
                                    Err(error) => {
                                        live_safety.trip(format!(
                                            "winner-buy {} has invalid redeem size: {error}",
                                            buy.position_id
                                        ));
                                        service_health.set_safety_ok(false);
                                        continue;
                                    }
                                };
                                match winner_journal.record_redeem_submitted(&buy) {
                                    Ok(true) => {}
                                    Ok(false) => continue,
                                    Err(error) => {
                                        live_safety.trip(format!(
                                            "winner-buy redemption intent {} could not be persisted: {error}",
                                            buy.position_id
                                        ));
                                        service_health.set_safety_ok(false);
                                        continue;
                                    }
                                }
                                match polypulse::redeem::redeem_one(
                                    condition_id,
                                    buy.neg_risk,
                                    proxy,
                                    &config.private_key,
                                    None,
                                    Some(if buy.token_is_yes { 0 } else { 1 }),
                                    Some(raw_size),
                                    &[token],
                                )
                                .await
                                {
                                    Ok(tx_hash) => {
                                        if let Err(error) = winner_journal
                                            .record_redeemed(&buy, tx_hash.clone())
                                        {
                                            live_safety.trip(format!(
                                                "winner-buy redemption confirmation {} could not be persisted: {error}",
                                                buy.position_id
                                            ));
                                            service_health.set_safety_ok(false);
                                            continue;
                                        }
                                        protected_winner_tokens
                                            .write()
                                            .expect("winner token lock poisoned")
                                            .remove(&token);
                                        _risk_manager
                                            .position_tracker()
                                            .record_sell_fill(token, buy.filled);
                                        runtime_metrics.record_winner_buy_redeemed();
                                        info!(target: "polypulse::strategy::winner_buy",
                                            position_id = %buy.position_id,
                                            %tx_hash,
                                            "Winner-buy redemption confirmed"
                                        );
                                    }
                                    Err(error) => {
                                        error!(target: "polypulse::strategy::winner_buy",
                                            position_id = %buy.position_id,
                                            %error,
                                            "Winner-buy redemption write is unresolved"
                                        );
                                        live_safety.trip(format!(
                                            "winner-buy redemption {} is unresolved",
                                            buy.position_id
                                        ));
                                        service_health.set_safety_ok(false);
                                    }
                                }
                            }
                        }
                    }
                    if strategy_controls.winner_buy_enabled()
                        && (config.winner_buy_max_per_window == 0
                            || winner_buy_count.load(Ordering::Relaxed)
                                < config.winner_buy_max_per_window)
                        && seconds_until_end > 0
                        && seconds_until_end <= config.winner_buy_before_end_secs
                        && matches!(
                            monitor.connection_state(),
                            MarketWsConnectionState::Connected
                        )
                        && user_ws_connected
                    {
                        let min_price = rust_decimal::Decimal::try_from(config.winner_buy_min_price)
                            .unwrap_or(dec!(0.95));
                        let max_price = rust_decimal::Decimal::try_from(config.winner_buy_max_price)
                            .unwrap_or(dec!(0.99));
                        let max_order = rust_decimal::Decimal::try_from(
                            config.winner_buy_max_order_usdc,
                        )
                        .unwrap_or(dec!(50));
                        for market in &markets {
                            runtime_metrics.record_winner_buy_scan();
                            runtime_metrics
                                .record_winner_buy_asset_scan(&market.crypto_symbol);
                            let market_attempts = winner_attempts_by_market
                                .get(&market.slug)
                                .copied()
                                .unwrap_or(0);
                            if !attempt_allowed(
                                winner_buy_count.load(Ordering::Relaxed),
                                market_attempts,
                                config.winner_buy_max_per_window,
                                config.winner_buy_max_attempts_per_market,
                            ) {
                                if config.winner_buy_max_per_window > 0
                                    && winner_buy_count.load(Ordering::Relaxed)
                                        >= config.winner_buy_max_per_window
                                {
                                    break;
                                }
                                continue;
                            }
                            let yes_token = match polymarket_client_sdk::types::U256::from_str(
                                &market.yes_token_id.to_string(),
                            ) {
                                Ok(t) => t,
                                Err(e) => {
                                    warn!(target: "polypulse::strategy::winner_buy", error = %e, "Winner-buy token parse failed");
                                    continue;
                                }
                            };
                            let no_token = match polymarket_client_sdk::types::U256::from_str(
                                &market.no_token_id.to_string(),
                            ) {
                                Ok(t) => t,
                                Err(e) => {
                                    warn!(target: "polypulse::strategy::winner_buy", error = %e, "Winner-buy token parse failed");
                                    continue;
                                }
                            };
                            // The live SDK WebSocket cache is the primary source.
                            // REST is only a fallback before the first WS snapshot.
                            let mut yes_top = monitor.winner_book_top(yes_token);
                            let mut no_top = monitor.winner_book_top(no_token);
                            let mut yes_from_rest = false;
                            let mut no_from_rest = false;
                            let max_book_age_ms = config.max_book_age_ms;
                            if yes_top.is_none() {
                                runtime_metrics.record_winner_buy_ws_missing_book();
                            }
                            if no_top.is_none() {
                                runtime_metrics.record_winner_buy_ws_missing_book();
                            }
                            if yes_top.as_ref().is_some_and(|top| {
                                !received_at_is_fresh(
                                    top.received_at_ms,
                                    now.timestamp_millis(),
                                    max_book_age_ms,
                                )
                            }) {
                                runtime_metrics.record_winner_buy_stale_book();
                                runtime_metrics.record_winner_buy_stale_ws_book();
                                yes_top = None;
                            }
                            if no_top.as_ref().is_some_and(|top| {
                                !received_at_is_fresh(
                                    top.received_at_ms,
                                    now.timestamp_millis(),
                                    max_book_age_ms,
                                )
                            }) {
                                runtime_metrics.record_winner_buy_stale_book();
                                runtime_metrics.record_winner_buy_stale_ws_book();
                                no_top = None;
                            }
                            if yes_top.is_none() || no_top.is_none() {
                                let yes_requested = yes_top.is_none();
                                let no_requested = no_top.is_none();
                                if yes_top.is_none() {
                                    runtime_metrics.record_winner_buy_rest_fallback();
                                    yes_from_rest = true;
                                }
                                if no_top.is_none() {
                                    runtime_metrics.record_winner_buy_rest_fallback();
                                    no_from_rest = true;
                                }
                                let rest_started = Instant::now();
                                let (yes_rest, no_rest) = tokio::join!(
                                    async {
                                        if yes_top.is_none() {
                                            executor.winner_book_top(yes_token).await
                                        } else {
                                            Ok(yes_top)
                                        }
                                    },
                                    async {
                                        if no_top.is_none() {
                                            executor.winner_book_top(no_token).await
                                        } else {
                                            Ok(no_top)
                                        }
                                    },
                                );
                                let rest_latency_ms = u64::try_from(
                                    rest_started.elapsed().as_millis(),
                                )
                                .unwrap_or(u64::MAX);
                                if yes_requested {
                                    if let Ok(top) = &yes_rest {
                                        runtime_metrics.record_winner_buy_rest_result(
                                            rest_latency_ms,
                                            top.is_some(),
                                        );
                                    }
                                }
                                if no_requested {
                                    if let Ok(top) = &no_rest {
                                        runtime_metrics.record_winner_buy_rest_result(
                                            rest_latency_ms,
                                            top.is_some(),
                                        );
                                    }
                                }
                                yes_top = match yes_rest {
                                    Ok(top) => top,
                                    Err(error) => {
                                        runtime_metrics.record_winner_buy_book_read_error();
                                        debug!(target: "polypulse::strategy::winner_buy", %error, token = %yes_token, "Winner-buy YES book REST fallback failed");
                                        None
                                    }
                                };
                                no_top = match no_rest {
                                    Ok(top) => top,
                                    Err(error) => {
                                        runtime_metrics.record_winner_buy_book_read_error();
                                        debug!(target: "polypulse::strategy::winner_buy", %error, token = %no_token, "Winner-buy NO book REST fallback failed");
                                        None
                                    }
                                };
                            }
                            let rest_checked_at_ms = Utc::now().timestamp_millis();
                            if yes_top.as_ref().is_some_and(|top| {
                                !received_at_is_fresh(
                                    top.received_at_ms,
                                    rest_checked_at_ms,
                                    max_book_age_ms,
                                )
                            }) {
                                runtime_metrics.record_winner_buy_stale_book();
                                if yes_from_rest {
                                    runtime_metrics.record_winner_buy_stale_rest_book();
                                } else {
                                    runtime_metrics.record_winner_buy_stale_ws_book();
                                }
                                yes_top = None;
                            }
                            if no_top.as_ref().is_some_and(|top| {
                                !received_at_is_fresh(
                                    top.received_at_ms,
                                    rest_checked_at_ms,
                                    max_book_age_ms,
                                )
                            }) {
                                runtime_metrics.record_winner_buy_stale_book();
                                if no_from_rest {
                                    runtime_metrics.record_winner_buy_stale_rest_book();
                                } else {
                                    runtime_metrics.record_winner_buy_stale_ws_book();
                                }
                                no_top = None;
                            }
                            let yes_ask = yes_top.and_then(|top| top.ask_price);
                            let no_ask = no_top.and_then(|top| top.ask_price);
                            if yes_ask.is_none() && no_ask.is_none() {
                                runtime_metrics.record_winner_buy_no_book();
                                runtime_metrics
                                    .record_winner_buy_asset_no_book(&market.crypto_symbol);
                                debug!(
                                    market = %market.crypto_symbol,
                                    "Winner-buy: no executable asks, confirmation state paused"
                                );
                                continue;
                            }
                            let Some(signal) =
                                detect_winner_asks(yes_ask, no_ask, min_price, max_price)
                            else {
                                runtime_metrics.record_winner_buy_no_signal();
                                runtime_metrics
                                    .record_winner_buy_asset_no_signal(&market.crypto_symbol);
                                debug!(
                                    market = %market.crypto_symbol,
                                    yes_ask = ?yes_ask,
                                    no_ask = ?no_ask,
                                    "Winner-buy: winner not identifiable, skipping"
                                );
                                continue;
                            };
                            let token = if signal.buy_yes { yes_token } else { no_token };
                            runtime_metrics.record_winner_buy_signal();
                            runtime_metrics
                                .record_winner_buy_asset_signal(&market.crypto_symbol);
                            let selected_top = if signal.buy_yes { yes_top } else { no_top };
                            let Some(selected_top) = selected_top else {
                                continue;
                            };
                            let (Some(bid_price), Some(ask_price), Some(ask_size)) = (
                                selected_top.bid_price,
                                selected_top.ask_price,
                                selected_top.ask_size,
                            ) else {
                                runtime_metrics.record_winner_buy_incomplete_book();
                                debug!(
                                    market = %market.crypto_symbol,
                                    buy_yes = signal.buy_yes,
                                    bid = ?selected_top.bid_price,
                                    ask = ?selected_top.ask_price,
                                    "Winner-buy selected book is one-sided; confirmation state paused"
                                );
                                continue;
                            };
                            let Some(fee_policy) = winner_fee_policies.get(&market.slug).copied()
                            else {
                                continue;
                            };
                            let fee_per_share = fee_policy.fee_per_share(signal.price);
                            let model = winner_twap_store.evaluate(
                                WinnerModelInput {
                                    asset: &market.crypto_symbol,
                                    window_start_ms: window_session.timestamp() * 1_000,
                                    now_ms: now.timestamp_millis(),
                                    seconds_remaining: i64::from(seconds_until_end),
                                    buy_yes: signal.buy_yes,
                                    bid_price,
                                    ask_price,
                                    ask_size,
                                },
                                winner_model_policy(
                                    &config,
                                    &market.crypto_symbol,
                                    fee_per_share,
                                ),
                            );
                            let evaluation_id = uuid::Uuid::new_v4().to_string();
                            let evaluation = WinnerEvaluation::from_model(
                                evaluation_id.clone(),
                                market.crypto_symbol.clone(),
                                market.slug.clone(),
                                market.end_date,
                                now,
                                window_session.timestamp() * 1_000,
                                i64::from(seconds_until_end),
                                signal.buy_yes,
                                bid_price,
                                ask_price,
                                ask_size,
                                fee_per_share,
                                &model,
                            );
                            if let Err(error) = winner_evaluation_journal
                                .as_ref()
                                .expect("Winner Buy evaluation journal initialized")
                                .record(evaluation)
                            {
                                error!(target: "polypulse::strategy::winner_buy",
                                    %error,
                                    market = %market.crypto_symbol,
                                    "Winner Buy evaluation persistence failed"
                                );
                                continue;
                            }
                            runtime_metrics.record_winner_buy_evaluation();
                            runtime_metrics
                                .record_winner_buy_asset_evaluation(&market.crypto_symbol);
                            if model.failed_checks.contains(
                                &crate::winner_model::ModelRejection::NetEvTooLow,
                            ) {
                                runtime_metrics.record_winner_buy_shadow_ev_failure();
                            }
                            debug!(target: "polypulse::strategy::winner_buy",
                                %evaluation_id,
                                market = %market.crypto_symbol,
                                slug = %market.slug,
                                rejection = ?model.rejection,
                                fee_per_share = %fee_per_share,
                                p_model = %model.probability,
                                net_ev = %model.net_ev,
                                "Winner Buy candidate evaluation persisted"
                            );
                            if !model.admitted() {
                                runtime_metrics.record_winner_model_rejection(
                                    model.rejection.expect("rejected model has reason"),
                                );
                                if model.failed_checks.contains(
                                    &crate::winner_model::ModelRejection::ReversalBoundary,
                                ) {
                                    winner_confirmations.reject(&market.slug);
                                }
                                debug!(target: "polypulse::strategy::winner_buy",
                                    market = %market.crypto_symbol,
                                    rejection = ?model.rejection,
                                    opening_twap = ?model.opening_twap,
                                    current_twap = ?model.current_twap,
                                    p_model = %model.probability,
                                    effective_price = %model.effective_price,
                                    net_ev = %model.net_ev,
                                    margin_bps = model.directional_margin_bps,
                                    reversal_boundary_bps = model.reversal_boundary_bps,
                                    spread = %model.spread,
                                    depth_usdc = %model.depth_notional,
                                    "Winner-buy rejected by independent Chainlink model"
                                );
                                continue;
                            }
                            let observed_at_ms = model
                                .observed_at_ms
                                .expect("admitted model has observation timestamp");
                            if !winner_confirmations.confirm(
                                &market.slug,
                                signal.buy_yes,
                                observed_at_ms,
                                config.winner_buy_confirmation_observations,
                                config.winner_buy_feed_max_age_ms.saturating_mul(3),
                            ) {
                                runtime_metrics.record_winner_buy_awaiting_confirmation();
                                debug!(target: "polypulse::strategy::winner_buy",
                                    market = %market.crypto_symbol,
                                    p_model = %model.probability,
                                    net_ev = %model.net_ev,
                                    "Winner-buy awaiting consecutive Chainlink confirmations"
                                );
                                continue;
                            }
                            // The configured maximum is a cash limit, so fees
                            // must be included when converting it to shares.
                            let size =
                                fee_inclusive_capped_size(signal.price, fee_per_share, max_order);
                            if size <= dec!(0) {
                                continue;
                            }
                            if !strategy_controls.winner_buy_enabled() {
                                winner_confirmations.clear();
                                debug!(target: "polypulse::strategy::winner_buy", market = %market.crypto_symbol, "Winner Buy paused before submission");
                                continue;
                            }
                            let position_id = uuid::Uuid::new_v4().to_string();
                            let planned_cost =
                                executor.winner_planned_cost(signal.price, size, fee_per_share);
                            let winner_journal = winner_buy_journal
                                .as_ref()
                                .expect("winner-buy journal initialized");
                            if let Err(reason) = winner_journal
                                .check_risk(&market.crypto_symbol, winner_buy_risk_policy)
                            {
                                runtime_metrics.record_risk_rejection();
                                warn!(target: "polypulse::strategy::winner_buy",
                                    market = %market.crypto_symbol,
                                    %reason,
                                    "Winner-buy rejected by strategy/asset loss circuit breaker"
                                );
                                continue;
                            }
                            let position_tracker = _risk_manager.position_tracker();
                            let Some(risk_reservation) = position_tracker.reserve(
                                format!("winner:{position_id}"),
                                planned_cost,
                            ) else {
                                runtime_metrics.record_risk_rejection();
                                warn!(target: "polypulse::strategy::winner_buy",
                                    market = %market.crypto_symbol,
                                    %planned_cost,
                                    exposure = %position_tracker.calculate_exposure(),
                                    limit = %position_tracker.max_exposure(),
                                    "Winner-buy rejected by exposure limit"
                                );
                                continue;
                            };
                            if let Err(reason) = live_safety.try_start_winner(
                                window_session.timestamp(),
                                planned_cost,
                            ) {
                                runtime_metrics.record_risk_rejection();
                                warn!(target: "polypulse::strategy::winner_buy", %reason, "Live/canary safety gate rejected winner-buy");
                                continue;
                            }
                            let attempt = PendingWinnerBuy {
                                position_id,
                                evaluation_id,
                                asset: market.crypto_symbol.clone(),
                                slug: market.slug.clone(),
                                condition_id: market.market_id.to_string(),
                                neg_risk: market.neg_risk,
                                token_id: token.to_string(),
                                token_is_yes: signal.buy_yes,
                                requested: size,
                                limit_price: signal.price,
                                planned_cost,
                                opening_twap: model
                                    .opening_twap
                                    .expect("admitted model has opening TWAP"),
                                current_twap: model
                                    .current_twap
                                    .expect("admitted model has current TWAP"),
                                model_observed_at_ms: observed_at_ms,
                                model_probability: model.probability,
                                model_net_ev: model.net_ev,
                                directional_margin_bps: rust_decimal::Decimal::try_from(
                                    model.directional_margin_bps,
                                )
                                .unwrap_or(dec!(0)),
                                reversal_boundary_bps: rust_decimal::Decimal::try_from(
                                    model.reversal_boundary_bps,
                                )
                                .unwrap_or(dec!(0)),
                                book_spread: model.spread,
                                book_depth_usdc: model.depth_notional,
                                filled: dec!(0),
                                price: dec!(0),
                                fee: dec!(0),
                                end_date: market.end_date,
                            };
                            match winner_journal.record_attempt(attempt.clone()) {
                                Ok(true) => {
                                    winner_buy_count.fetch_add(1, Ordering::Relaxed);
                                    *winner_attempts_by_market
                                        .entry(market.slug.clone())
                                        .or_default() += 1;
                                }
                                Ok(false) => {
                                    live_safety.trip(format!(
                                        "duplicate winner-buy attempt id {}",
                                        attempt.position_id
                                    ));
                                    service_health.set_safety_ok(false);
                                    continue;
                                }
                                Err(error) => {
                                    live_safety.trip(format!(
                                        "winner-buy attempt {} could not be persisted: {error}",
                                        attempt.position_id
                                    ));
                                    service_health.set_safety_ok(false);
                                    continue;
                                }
                            }
                            runtime_metrics.record_winner_buy_attempt();
                            runtime_metrics
                                .record_winner_buy_asset_attempt(&market.crypto_symbol);
                            let side = if signal.buy_yes { "YES" } else { "NO" };
                            info!(target: "polypulse::strategy::winner_buy",
                                market = %market.crypto_symbol,
                                %side,
                                price = %signal.price,
                                opening_twap = %model.opening_twap.expect("admitted model has opening TWAP"),
                                current_twap = %model.current_twap.expect("admitted model has current TWAP"),
                                p_model = %model.probability,
                                net_ev = %model.net_ev,
                                margin_bps = model.directional_margin_bps,
                                reversal_boundary_bps = model.reversal_boundary_bps,
                                spread = %model.spread,
                                depth_usdc = %model.depth_notional,
                                %size,
                                secs_left = seconds_until_end,
                                "🛒 Winner-buy order submitted"
                            );
                            match executor
                                .buy_winner(token, signal.price, size, fee_per_share)
                                .await
                            {
                                Ok(execution) => {
                                    if execution.filled > dec!(0) {
                                        let filled = execution.filled;
                                        let fill_price = execution.average_price;
                                        position_tracker.record_buy_fill(
                                            token,
                                            fill_price,
                                            filled,
                                        );
                                        runtime_metrics.record_winner_buy_fill();
                                        runtime_metrics
                                            .record_winner_buy_asset_fill(&market.crypto_symbol);
                                        runtime_metrics.record_winner_buy_pnl(filled, fill_price);
                                        let est_pnl = filled * (dec!(1) - fill_price);
                                        dashboard.with_mut(|d| {
                                            d.record_winner_buy_fill(decimal_to_f64(est_pnl));
                                        });
                                        let pending = PendingWinnerBuy {
                                            filled,
                                            price: fill_price,
                                            fee: execution.estimated_fee,
                                            ..attempt.clone()
                                        };
                                        protected_winner_tokens
                                            .write()
                                            .expect("winner token lock poisoned")
                                            .insert(token);
                                        match winner_journal.record_pending(pending) {
                                            Ok(true) => {
                                                runtime_metrics.set_winner_buy_pending(
                                                    winner_journal.pending().len(),
                                                );
                                            }
                                            Ok(false) => {
                                                live_safety.trip(format!(
                                                    "duplicate winner-buy fill {}",
                                                    attempt.position_id
                                                ));
                                                service_health.set_safety_ok(false);
                                            }
                                            Err(e) => {
                                                error!(target: "polypulse::strategy::winner_buy",
                                                    error = %e,
                                                    market = %market.crypto_symbol,
                                                    %side,
                                                    "Winner-buy fill persistence failed"
                                                );
                                                live_safety.trip(format!(
                                                    "winner-buy fill {} could not be persisted",
                                                    market.slug
                                                ));
                                                service_health.set_safety_ok(false);
                                            }
                                        }
                                        info!(target: "polypulse::strategy::winner_buy",
                                            market = %market.crypto_symbol,
                                            %side,
                                            price = %fill_price,
                                            %filled,
                                            cost = %execution.cost,
                                            fee = %execution.estimated_fee,
                                            "✅ Winner-buy filled; holding to settlement"
                                        );
                                    } else {
                                        if let Err(error) = winner_journal.record_no_fill(&attempt)
                                        {
                                            live_safety.trip(format!(
                                                "winner-buy no-fill {} could not be persisted: {error}",
                                                attempt.position_id
                                            ));
                                            service_health.set_safety_ok(false);
                                        }
                                        warn!(target: "polypulse::strategy::winner_buy",
                                            market = %market.crypto_symbol,
                                            %side,
                                            price = %signal.price,
                                            "Winner-buy order not filled (no depth at limit)"
                                        );
                                    }
                                }
                                Err(e) => {
                                    if let Err(journal_error) =
                                        winner_journal.record_failed(&attempt, &e.to_string())
                                    {
                                        live_safety.trip(format!(
                                            "winner-buy failure {} could not be persisted: {journal_error}",
                                            attempt.position_id
                                        ));
                                        service_health.set_safety_ok(false);
                                    }
                                    warn!(target: "polypulse::strategy::winner_buy",
                                        market = %market.crypto_symbol,
                                        %side,
                                        error = %e,
                                        "Winner-buy failed"
                                    );
                                }
                            }
                            drop(risk_reservation);
                        }
                    }

                    if window_session.has_rolled_over(now, config.market_period_minutes) {
                        let new_window_timestamp =
                            _scheduler.calculate_current_window_timestamp(now);
                        info!(
                            old_window = window_session.timestamp(),
                            new_window = new_window_timestamp,
                            "New window detected, cancelling old subscriptions and switching"
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
            Ok(snapshot) => {
                info!(metrics = %snapshot, "Runtime metrics snapshot");
                info!(target: "polypulse::strategy::spread_arbitrage", metrics = %snapshot, "Spread Arbitrage metrics snapshot");
                if winner_buy_available {
                    info!(target: "polypulse::strategy::winner_buy", metrics = %snapshot, "Winner Buy metrics snapshot");
                }
            }
            Err(error) => warn!(%error, "Failed to serialize runtime metrics"),
        }

        info!("Current window monitoring ended, refreshing markets for next round");
    }
}

fn winner_model_policy(
    config: &Config,
    asset: &str,
    fee_per_share: rust_decimal::Decimal,
) -> WinnerModelPolicy {
    let asset = asset.trim().to_ascii_lowercase();
    let (min_volatility, model_risk_buffer) = match asset.as_str() {
        "btc" => (
            config.winner_buy_btc_min_volatility_bps,
            config.winner_buy_btc_model_risk_buffer,
        ),
        "eth" => (
            config.winner_buy_eth_min_volatility_bps,
            config.winner_buy_eth_model_risk_buffer,
        ),
        _ => (
            config.winner_buy_other_min_volatility_bps,
            config.winner_buy_other_model_risk_buffer,
        ),
    };
    WinnerModelPolicy {
        max_feed_age_ms: config.winner_buy_feed_max_age_ms,
        max_opening_skew_ms: config.winner_buy_opening_max_skew_ms,
        volatility_lookback_secs: config.winner_buy_volatility_lookback_secs,
        min_observations: config.winner_buy_min_observations,
        min_volatility_bps_per_sqrt_second: min_volatility,
        min_margin_bps: config.winner_buy_min_margin_bps,
        reversal_sigma_multiplier: config.winner_buy_reversal_sigma,
        max_spread: rust_decimal::Decimal::try_from(config.winner_buy_max_spread)
            .unwrap_or(dec!(0.03)),
        min_depth_usdc: rust_decimal::Decimal::try_from(config.winner_buy_min_depth_usdc)
            .unwrap_or(dec!(10)),
        fee_buffer: fee_per_share,
        model_risk_buffer: rust_decimal::Decimal::try_from(model_risk_buffer).unwrap_or(dec!(0.01)),
        min_net_ev: rust_decimal::Decimal::try_from(config.winner_buy_min_net_ev)
            .unwrap_or(dec!(0.002)),
    }
}
