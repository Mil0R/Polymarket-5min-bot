use polypulse::ui::control::{StrategyControls, StrategyKind};
use polypulse::ui::{decimal_to_f64, spawn_dashboard_thread, symbol_short, DashboardHandle};
use polypulse::wallet_copy_trading::WalletCopyTradingRuntime;

use anyhow::{Context, Result};
use futures::{future::join_all, StreamExt};
use rust_decimal_macros::dec;
use std::collections::{HashMap, HashSet};
use std::io::{stdout, IsTerminal};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
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
use crate::wallet_copy_trading_embedded;
use crate::{observability_server, utils};

use crate::late_resolution_winner_model::{
    attempt_allowed, run_chainlink_twap_feed, ChainlinkTwapStore, WinnerConfirmationTracker,
    WinnerModelInput, WinnerModelPolicy,
};
use crate::late_resolution_winner_trading::{
    detect_winner_asks, fee_inclusive_depth_capped_size, received_at_is_fresh,
    settle_late_resolution_winner_trading, winner_books_need_rest, winner_research_cohorts,
    LateResolutionWinnerTradingJournal, LateResolutionWinnerTradingRiskPolicy,
    PendingLateResolutionWinnerTrading, SettlementBackoff, WinnerConfirmationObservation,
    WinnerEvaluation, WinnerEvaluationJournal, WinnerShadowExecution,
};
use crate::risk::positions::PositionTracker;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeProfile {
    Integrated,
    BinaryComplementArbitrageDryRun,
    LateResolutionWinnerTradingDryRun,
}

fn apply_runtime_profile(profile: RuntimeProfile) {
    if profile == RuntimeProfile::Integrated {
        return;
    }

    // Standalone strategy binaries are stage-2 research tools. These process-local
    // overrides are applied before Config is loaded so `.env` can never turn them
    // into account-writing executables.
    std::env::set_var("DRY_RUN", "true");
    std::env::set_var("CANARY_MODE", "false");
    std::env::set_var("TUI_ENABLED", "false");
    std::env::set_var("LOG_STDOUT_TEE", "true");
    std::env::set_var("LOG_STDOUT_KEY_EVENTS_ONLY", "true");
    std::env::set_var("MERGE_INTERVAL_MINUTES", "0");
    std::env::set_var("WIND_DOWN_BEFORE_WINDOW_END_MINUTES", "0");
    std::env::set_var("WALLET_COPY_TRADING_ENABLED", "false");
    match profile {
        RuntimeProfile::BinaryComplementArbitrageDryRun => {
            std::env::set_var("BINARY_COMPLEMENT_ARBITRAGE_ENABLED", "true");
            std::env::set_var("LATE_RESOLUTION_WINNER_TRADING_ENABLED", "false");
        }
        RuntimeProfile::LateResolutionWinnerTradingDryRun => {
            std::env::set_var("BINARY_COMPLEMENT_ARBITRAGE_ENABLED", "false");
            std::env::set_var("LATE_RESOLUTION_WINNER_TRADING_ENABLED", "true");
        }
        RuntimeProfile::Integrated => unreachable!(),
    }
}

pub async fn run(profile: RuntimeProfile) -> Result<()> {
    apply_runtime_profile(profile);
    match profile {
        RuntimeProfile::BinaryComplementArbitrageDryRun => {
            utils::logger::select_strategy_system_log(
                "BINARY_COMPLEMENT_ARBITRAGE",
                "logs/binary-complement-arbitrage/system.log",
            )
        }
        RuntimeProfile::LateResolutionWinnerTradingDryRun => {
            utils::logger::select_strategy_system_log(
                "LATE_RESOLUTION_WINNER_TRADING",
                "logs/late-resolution-winner-trading/system.log",
            )
        }
        RuntimeProfile::Integrated => {}
    }
    // Must set rustls default crypto provider first, otherwise reqwest/alloy etc. will panic when using TLS

    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    let use_tui = utils::logger::tui_enabled_from_env() && stdout().is_terminal();
    utils::logger::init_logger_for_strategies(
        use_tui,
        profile != RuntimeProfile::LateResolutionWinnerTradingDryRun,
        profile != RuntimeProfile::BinaryComplementArbitrageDryRun,
        profile == RuntimeProfile::Integrated,
    )?;

    let shutdown = Arc::new(AtomicBool::new(false));

    if use_tui {
        tracing::info!("Polymarket 5-minute bot starting (dashboard mode)");
    } else {
        tracing::info!("Polymarket 5-minute arbitrage bot starting");
    }

    // Load config
    let config = Config::from_env()?;
    let winner_sensitivity_enabled = config.dry_run
        && std::env::var("LATE_RESOLUTION_WINNER_TRADING_SENSITIVITY_ENABLED")
            .unwrap_or_else(|_| "false".into())
            .parse::<bool>()
            .context("invalid LATE_RESOLUTION_WINNER_TRADING_SENSITIVITY_ENABLED")?;
    if !config.dry_run && !use_tui {
        anyhow::bail!(
            "live/canary execution is restricted to the interactive polypulse TUI; standalone and headless execution must use DRY_RUN=true"
        );
    }
    tracing::info!("Config loaded");
    // Dry-run TUI may hot-start Late-Resolution Winner Trading after provisioning its read-only
    // dependencies. Live mode requires explicit startup configuration so all
    // credential and compatibility validation remains fail-closed.
    let late_resolution_winner_trading_available =
        config.late_resolution_winner_trading_enabled || (use_tui && config.dry_run);
    // Wallet-Copy Trading is embedded PAPER-first. Live coexistence remains fail-closed
    // until all strategies share an account-wide reservation/ownership ledger.
    let wallet_copy_trading_available =
        config.dry_run && (config.wallet_copy_trading_enabled || use_tui);
    let copy_runtime =
        WalletCopyTradingRuntime::new(shutdown.clone(), config.wallet_copy_trading_enabled);
    let strategy_controls = Arc::new(StrategyControls::new(
        config.arbitrage_enabled,
        config.late_resolution_winner_trading_enabled,
        late_resolution_winner_trading_available,
        wallet_copy_trading_available,
        wallet_copy_trading_available.then_some(copy_runtime.clone()),
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
    info!(%run_id, mode = execution_mode, ?profile, "Run started");
    info!(
        arbitrage_budget_usdc = config.arbitrage_budget_usdc,
        late_resolution_winner_trading_budget_usdc =
            config.late_resolution_winner_trading_budget_usdc,
        wallet_copy_trading_budget_usdc = config.wallet_copy_trading_budget_usdc,
        "Per-strategy capital budgets loaded"
    );
    if profile != RuntimeProfile::LateResolutionWinnerTradingDryRun {
        info!(
            target: "polypulse::strategy::binary_complement_arbitrage",
            %run_id,
            mode = execution_mode,
            state_file = %config.state_journal_file,
            opportunity_file = ?config.arbitrage_log_file,
            shadow_file = ?config.arbitrage_shadow_log_file,
            max_book_age_ms = config.max_book_age_ms,
            max_book_skew_ms = config.max_book_skew_ms,
            signal_confirmation_ms = config.arbitrage_signal_confirmation_ms,
            dynamic_fee_gate_paper = config.arbitrage_dynamic_fee_gate_paper,
            fixed_gross_edge = config.arbitrage_execution_spread,
            fee_buffer = config.arbitrage_fee_buffer,
            max_order_size_usdc = config.max_order_size_usdc,
            rest_shadow_delays_ms = ?config.arbitrage_rest_shadow_delays_ms,
            rest_shadow_notionals_usdc = ?config.arbitrage_rest_shadow_notionals_usdc,
            enabled = config.arbitrage_enabled,
            "Binary Complement Arbitrage run started"
        );
    }
    if late_resolution_winner_trading_available {
        info!(
            target: "polypulse::strategy::late_resolution_winner_trading",
            %run_id,
            mode = execution_mode,
            state_file = %config.late_resolution_winner_trading_state_journal_file,
            evaluation_file = %config.late_resolution_winner_trading_evaluation_journal_file,
            enabled = config.late_resolution_winner_trading_enabled,
            "Late-Resolution Winner Trading run started"
        );
    }
    let _copy_task = if wallet_copy_trading_available {
        let runtime = copy_runtime.clone();
        let budget = rust_decimal::Decimal::try_from(config.wallet_copy_trading_budget_usdc)?;
        Some(tokio::spawn(async move {
            if let Err(error) =
                wallet_copy_trading_embedded::run_embedded(runtime.clone(), budget).await
            {
                runtime.set_healthy(false);
                error!(target: "polypulse::strategy::wallet_copy_trading", %error, "Embedded Wallet-Copy Trading stopped");
            }
        }))
    } else {
        warn!(target: "polypulse::strategy::wallet_copy_trading", "Wallet-Copy Trading TUI integration is unavailable in LIVE mode; run PAPER validation first");
        None
    };

    let runtime_metrics = Arc::new(RuntimeMetrics::default());
    let winner_twap_store = Arc::new(ChainlinkTwapStore::default());
    if late_resolution_winner_trading_available {
        tokio::spawn(run_chainlink_twap_feed(
            config.late_resolution_winner_trading_rtds_url.clone(),
            winner_twap_store.clone(),
            shutdown.clone(),
            runtime_metrics.clone(),
        ));
    }

    let order_mode = format!("{:?}", config.arbitrage_order_type);
    let dashboard = DashboardHandle::new_live(order_mode, config.arbitrage_budget_usdc);
    dashboard.with_mut(|dashboard| {
        dashboard.set_execution_mode(if config.dry_run {
            "DRY RUN"
        } else if config.canary_mode {
            "CANARY"
        } else {
            "LIVE"
        });
        dashboard.set_strategy_states(
            strategy_controls.state(StrategyKind::BinaryComplementArbitrage),
            strategy_controls.state(StrategyKind::LateResolutionWinnerTrading),
            strategy_controls.state(StrategyKind::WalletCopyTrading),
            strategy_controls.blocked_reason(),
        );
        dashboard.set_strategy_budgets(
            config.arbitrage_budget_usdc,
            config.late_resolution_winner_trading_budget_usdc,
            config.wallet_copy_trading_budget_usdc,
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
            dynamic_fee_gate_paper: config.arbitrage_dynamic_fee_gate_paper,
            gtd_expiration_secs: config.gtd_expiration_secs,
            order_type: config.arbitrage_order_type.clone(),
            dry_run: config.dry_run,
            arbitrage_budget_usdc: config.arbitrage_budget_usdc,
            late_resolution_winner_trading_budget_usdc: config
                .late_resolution_winner_trading_budget_usdc,
            paper_liquidity_haircut: config.paper_liquidity_haircut,
            arbitrage_paper_liquidity_haircut: config.arbitrage_paper_liquidity_haircut,
            paper_second_leg_delay_ms: config.paper_second_leg_delay_ms,
            arbitrage_paper_emergency_unwind_delay_ms: config
                .arbitrage_paper_emergency_unwind_delay_ms,
            arbitrage_paper_emergency_unwind_min_price: config
                .arbitrage_paper_emergency_unwind_min_price,
            paper_late_resolution_winner_trading_delay_ms: config
                .paper_late_resolution_winner_trading_delay_ms,
        },
    ));

    let _risk_manager = Arc::new(
        if profile == RuntimeProfile::LateResolutionWinnerTradingDryRun {
            RiskManager::new_isolated_paper(&config)?
        } else {
            RiskManager::new(&config)?
        },
    );
    let winner_position_tracker = Arc::new(PositionTracker::new(rust_decimal::Decimal::try_from(
        config.late_resolution_winner_trading_budget_usdc,
    )?));

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

    if config.dry_run && config.arbitrage_enabled {
        let summary_executor = executor.clone();
        let summary_metrics = runtime_metrics.clone();
        let summary_shutdown = shutdown.clone();
        let summary_interval_secs = config.arbitrage_paper_summary_interval_secs;
        tokio::spawn(async move {
            let interval = Duration::from_secs(summary_interval_secs);
            loop {
                sleep(interval).await;
                if summary_shutdown.load(Ordering::Relaxed) {
                    break;
                }
                log_binary_complement_paper_summary(
                    &summary_executor,
                    &summary_metrics,
                    summary_interval_secs,
                );
            }
        });
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
    let min_ready_disk_bytes = if late_resolution_winner_trading_available {
        config.late_resolution_winner_trading_journal_min_free_bytes
    } else {
        MIN_READY_DISK_BYTES
    };
    let disk_healthy = fs2::available_space(journal_dir)
        .map(|bytes| bytes >= min_ready_disk_bytes)
        .unwrap_or(false);
    service_health.set_disk_healthy(disk_healthy);
    service_health.set_no_unresolved_orders(!_risk_manager.has_unresolved_state());
    service_health.set_safety_ok(!live_safety.is_tripped());

    let late_resolution_winner_trading_journal = if late_resolution_winner_trading_available {
        let journal = Arc::new(
            LateResolutionWinnerTradingJournal::open_with_min_free_bytes(
                &config.late_resolution_winner_trading_state_journal_file,
                config.late_resolution_winner_trading_journal_min_free_bytes,
            )?,
        );
        let restored = journal.pending().len();
        if restored > 0 {
            warn!(target: "polypulse::strategy::late_resolution_winner_trading",
                count = restored,
                path = %journal.path().display(),
                "Restored pending late-resolution-winner-trading positions"
            );
        }
        let awaiting_redeem = journal.awaiting_redeem().len();
        if awaiting_redeem > 0 {
            warn!(target: "polypulse::strategy::late_resolution_winner_trading",
                count = awaiting_redeem,
                path = %journal.path().display(),
                "Restored resolved late-resolution-winner-trading positions awaiting redemption"
            );
        }
        let uncertain_redeems = journal.uncertain_redeem_count();
        if uncertain_redeems > 0 {
            live_safety.trip(format!(
                "{uncertain_redeems} late-resolution-winner-trading redemption(s) have uncertain write state"
            ));
            service_health.set_safety_ok(false);
        }
        info!(target: "polypulse::strategy::late_resolution_winner_trading", snapshot = ?journal.risk_snapshot(), "Restored Late-Resolution Winner Trading risk history");
        let summary = journal.paper_summary();
        if let Some(snapshot) = executor.restore_paper_winner_history(
            summary.pending_cost,
            summary.total_spent,
            summary.realized_pnl,
            summary.open_positions,
        ) {
            info!(target: "polypulse::strategy::late_resolution_winner_trading",
                cash_remaining = %snapshot.cash_remaining,
                pending_cost = %summary.pending_cost,
                realized_pnl = %summary.realized_pnl,
                open_positions = summary.open_positions,
                "Restored Late-Resolution Winner Trading paper cash lifecycle"
            );
        }
        let position_tracker = winner_position_tracker.clone();
        for pending in journal
            .pending()
            .into_iter()
            .chain(journal.awaiting_redeem())
        {
            let Ok(token) = polymarket_client_sdk::types::U256::from_str(&pending.token_id) else {
                warn!(target: "polypulse::strategy::late_resolution_winner_trading",
                    position_id = %pending.position_id,
                    token_id = %pending.token_id,
                    "Could not restore Late-Resolution Winner Trading token into PositionTracker"
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
    let winner_evaluation_journal = if late_resolution_winner_trading_available {
        let journal = Arc::new(WinnerEvaluationJournal::open_with_min_free_bytes(
            &config.late_resolution_winner_trading_evaluation_journal_file,
            config.late_resolution_winner_trading_journal_min_free_bytes,
        )?);
        journal.record_run_started(
            &run_id.to_string(),
            execution_mode,
            config.late_resolution_winner_trading_hybrid_paper_enabled,
        )?;
        info!(target: "polypulse::strategy::late_resolution_winner_trading", path = %journal.path().display(), "Late-Resolution Winner Trading counterfactual journal ready");
        Some(journal)
    } else {
        None
    };
    let protected_winner_tokens = Arc::new(RwLock::new(
        late_resolution_winner_trading_journal
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
    if let Some(journal) = late_resolution_winner_trading_journal.as_ref() {
        runtime_metrics.set_late_resolution_winner_trading_pending(journal.pending().len());
    }
    let winner_loop_heartbeat = Arc::new(AtomicU64::new(0));
    if config.dry_run {
        if let Some(summary_journal) = late_resolution_winner_trading_journal.clone() {
            let summary_metrics = runtime_metrics.clone();
            let summary_shutdown = shutdown.clone();
            let summary_interval_secs =
                config.late_resolution_winner_trading_paper_summary_interval_secs;
            let summary_heartbeat = Arc::new(AtomicU64::new(0));
            let watchdog_summary = summary_heartbeat.clone();
            let watchdog_loop = winner_loop_heartbeat.clone();
            let watchdog_shutdown = shutdown.clone();
            // Independent of Tokio: a blocked runtime must not silence its own watchdog.
            std::thread::Builder::new().name("winner-watchdog".into()).spawn(move || {
                let mut last_summary = 0;
                let mut last_loop = 0;
                let mut summary_progress = Instant::now();
                let mut loop_progress = Instant::now();
                let mut last_check = Instant::now();
                let mut unhealthy = false;
                while !watchdog_shutdown.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_secs(1));
                    let summary = watchdog_summary.load(Ordering::Relaxed);
                    let event_loop = watchdog_loop.load(Ordering::Relaxed);
                    if summary != last_summary { summary_progress = Instant::now(); last_summary = summary; }
                    if event_loop != last_loop { loop_progress = Instant::now(); last_loop = event_loop; }
                    if last_check.elapsed() < Duration::from_secs(60) { continue; }
                    last_check = Instant::now();
                    let summary_stale = summary_progress.elapsed().as_secs() > summary_interval_secs.saturating_mul(3).max(180);
                    // Market discovery may normally wait for a 15-minute window.
                    let loop_stale = loop_progress.elapsed().as_secs() > 1800;
                    if summary_stale || loop_stale {
                        error!(target: "polypulse::console::late_resolution_winner_trading",
                            summary_stale, loop_stale,
                            summary_silence_secs = summary_progress.elapsed().as_secs(),
                            loop_silence_secs = loop_progress.elapsed().as_secs(),
                            "Winner runtime heartbeat stalled; feed connectivity does not imply strategy health; investigate before counting runtime");
                    } else if unhealthy {
                        info!(target: "polypulse::console::late_resolution_winner_trading", "Winner runtime heartbeat recovered");
                    }
                    unhealthy = summary_stale || loop_stale;
                }
                info!(target: "polypulse::console::late_resolution_winner_trading", "Winner watchdog stopped: shutdown requested");
            })?;
            let summary_task = tokio::spawn(async move {
                let interval = Duration::from_secs(summary_interval_secs);
                loop {
                    sleep(interval).await;
                    if summary_shutdown.load(Ordering::Relaxed) {
                        info!(target: "polypulse::console::late_resolution_winner_trading", "Winner summary stopped: shutdown requested");
                        break;
                    }
                    log_late_resolution_winner_paper_summary(
                        &summary_metrics,
                        &summary_journal,
                        summary_interval_secs,
                    );
                    summary_heartbeat.fetch_add(1, Ordering::Relaxed);
                }
            });
            tokio::spawn(async move {
                if let Err(error) = summary_task.await {
                    error!(target: "polypulse::console::late_resolution_winner_trading", %error, "Winner summary task terminated unexpectedly");
                }
            });
        }
    }
    let mut last_winner_settle_check = Instant::now();
    let mut winner_position_settlement_backoff = SettlementBackoff::default();
    let mut winner_evaluation_settlement_backoff = SettlementBackoff::default();
    let late_resolution_winner_trading_risk_policy = LateResolutionWinnerTradingRiskPolicy {
        max_strategy_loss: rust_decimal::Decimal::try_from(
            config.late_resolution_winner_trading_max_strategy_loss_usdc,
        )?,
        max_asset_loss: rust_decimal::Decimal::try_from(
            config.late_resolution_winner_trading_max_asset_loss_usdc,
        )?,
        max_consecutive_losses: config.late_resolution_winner_trading_max_consecutive_losses,
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
    // The SDK's connection and reconnection tasks are tied to the client, not
    // to a stream. Recreating clients every five minutes leaves old sockets
    // alive, eventually exhausting the process file-descriptor limit.
    let market_ws_client = polymarket_client_sdk::clob::ws::Client::default();
    let user_ws_client = polymarket_client_sdk_v2::clob::ws::Client::default()
        .authenticate(clob_client.credentials().clone(), clob_client.address())
        .context("Failed to authenticate user order stream")?;
    let mut previous_market_tokens = Vec::new();
    let mut previous_user_markets = Vec::new();
    #[allow(unreachable_code)]
    loop {
        winner_loop_heartbeat.fetch_add(1, Ordering::Relaxed);
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
        let mut monitor = OrderBookMonitor::with_ws_client(market_ws_client.clone());

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
        // treating the legacy static arbitrage buffer as a Late-Resolution Winner Trading fee.
        let mut winner_fee_policies = HashMap::new();
        if late_resolution_winner_trading_available {
            for market in &markets {
                match executor.winner_fee_policy(market.market_id).await {
                    Ok(policy) => {
                        debug!(target: "polypulse::strategy::late_resolution_winner_trading",
                            market = %market.crypto_symbol,
                            fee_rate = %policy.rate,
                            fee_exponent = policy.exponent,
                            "Loaded Late-Resolution Winner Trading V2 fee policy"
                        );
                        winner_fee_policies.insert(market.slug.clone(), policy);
                    }
                    Err(error) => {
                        runtime_metrics.record_late_resolution_winner_trading_fee_read_error();
                        warn!(
                            %error,
                            market = %market.crypto_symbol,
                            "Late-Resolution Winner Trading fee policy unavailable; this market will be skipped"
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
        // Subscribe the new window first so the SDK channel never has zero
        // subscriptions. Removing its last subscription discards the channel
        // and creates another background connection on the next window.
        let current_market_tokens = monitor.subscribed_token_ids();
        if !previous_market_tokens.is_empty() {
            if let Err(error) = market_ws_client.unsubscribe_orderbook(&previous_market_tokens) {
                warn!(%error, "Failed to unsubscribe previous market window");
            }
        }
        previous_market_tokens = current_market_tokens;
        let mut market_ws_reconnecting = false;
        let mut market_ws_disconnect_started_at: Option<Instant> = None;
        let mut market_ws_seen_connected = matches!(
            monitor.connection_state(),
            MarketWsConnectionState::Connected
        );

        info!(market_count = markets.len(), "Monitoring orderbook");

        // Window-end late-resolution-winner-trading: attempt counter reset on each window rollover.
        let late_resolution_winner_trading_count = Arc::new(AtomicU32::new(0));
        let late_resolution_winner_trading_production_count = Arc::new(AtomicU32::new(0));
        let late_resolution_winner_trading_hybrid_count = Arc::new(AtomicU32::new(0));
        let mut winner_attempts_by_market: HashMap<String, u32> = HashMap::new();
        let mut winner_confirmations = WinnerConfirmationTracker::default();
        let mut winner_shadow_confirmations = WinnerConfirmationTracker::default();
        let mut winner_shadow_executed = HashSet::new();
        let mut winner_sensitivity_confirmations: Vec<_> = (0..8)
            .map(|_| WinnerConfirmationTracker::default())
            .collect();
        let mut winner_sensitivity_executed = HashSet::new();
        if late_resolution_winner_trading_available {
            info!(target: "polypulse::strategy::late_resolution_winner_trading",
                before_end_secs = config.late_resolution_winner_trading_before_end_secs,
                min_price = config.late_resolution_winner_trading_min_price,
                max_price = config.late_resolution_winner_trading_max_price,
                hybrid_paper_enabled = config.late_resolution_winner_trading_hybrid_paper_enabled,
                hybrid_paper_max_order_usdc = config.late_resolution_winner_trading_hybrid_paper_max_order_usdc,
                hybrid_paper_max_per_window = config.late_resolution_winner_trading_hybrid_paper_max_per_window,
                max_order_usdc = config.late_resolution_winner_trading_max_order_usdc,
                max_book_age_ms = config.late_resolution_winner_trading_max_book_age_ms,
                confirmation_observations = config.late_resolution_winner_trading_confirmation_observations,
                min_depth_usdc = config.late_resolution_winner_trading_min_depth_usdc,
                min_net_ev = config.late_resolution_winner_trading_min_net_ev,
                reversal_sigma = config.late_resolution_winner_trading_reversal_sigma,
                depth_adaptive_sizing = true,
                "Late-Resolution Winner Trading strategy enabled"
            );
        }

        use chrono::Utc;
        let mut window_session =
            WindowSession::new(&markets, Utc::now(), config.market_period_minutes)?;
        let window_label = window_session.label().to_string();

        dashboard.with_mut(|d| {
            d.set_window(&window_label, window_session.remaining_seconds(Utc::now()));
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
        let current_user_markets: Vec<_> = markets.iter().map(|market| market.market_id).collect();
        let mut user_order_stream =
            match user_ws_client.subscribe_orders(current_user_markets.clone()) {
                Ok(stream) => Box::pin(stream),
                Err(e) => {
                    error!(error = %e, "Failed to subscribe authenticated user order stream");
                    runtime_metrics.record_stream_connect_failure();
                    runtime_metrics.record_stream_reconnect();
                    sleep(connection_supervisor.connect_failed()).await;
                    continue;
                }
            };
        if !previous_user_markets.is_empty() {
            if let Err(error) = user_ws_client.unsubscribe_orders(&previous_user_markets) {
                warn!(%error, "Failed to unsubscribe previous user window");
            }
        }
        previous_user_markets = current_user_markets;
        #[cfg(target_os = "linux")]
        if let Ok(open_fds) = std::fs::read_dir("/proc/self/fd") {
            info!(
                open_fds = open_fds.count(),
                "Window WebSocket subscriptions ready"
            );
        }
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
            winner_loop_heartbeat.fetch_add(1, Ordering::Relaxed);
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
                                // Shared book ingestion and recovery stay active. Disabled Binary
                                // must not evaluate, log candidates, or schedule REST research.
                                let tick = match session_runner.process_pair_if_enabled(
                                    strategy_controls.binary_complement_arbitrage_enabled(),
                                    &pair,
                                    Utc::now().timestamp_millis(),
                                    window_scope.trading_paused() || live_safety.is_tripped(),
                                    connection_supervisor.is_healthy(),
                                    _risk_manager.has_unresolved_state(),
                                ) {
                                    Ok(Some(tick)) => tick,
                                    Ok(None) => continue,
                                    Err(error) => {
                                        debug!(target: "polypulse::strategy::binary_complement_arbitrage", %error, market_id = %pair.market_id, "Rejected unsafe orderbook pair");
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
                                let rest_shadow_candidate = tick.rest_shadow_candidate;

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
                                    info!(target: "polypulse::strategy::binary_complement_arbitrage",
                                        "{} {} | {} | {} | {}",
                                        prefix, market_display, yes_info, no_info, spread_info
                                    );
                                }
                                trace!(target: "polypulse::strategy::binary_complement_arbitrage",
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
                                            error!(target: "polypulse::strategy::binary_complement_arbitrage", %error, "Failed to persist spread shadow candidate");
                                        }
                                    });
                                }
                                if config.dry_run {
                                    if let (Some(candidate), Some(path)) = (
                                        rest_shadow_candidate,
                                        config.arbitrage_shadow_log_file.clone(),
                                    ) {
                                        let shadow_executor = executor.clone();
                                        let shadow_market = market_display.clone();
                                        let shadow_yes_dir = yes_dir.to_string();
                                        let shadow_no_dir = no_dir.to_string();
                                        let shadow_delays =
                                            config.arbitrage_rest_shadow_delays_ms.clone();
                                        let shadow_notionals = config
                                            .arbitrage_rest_shadow_notionals_usdc
                                            .iter()
                                            .filter_map(|value| rust_decimal::Decimal::try_from(*value).ok())
                                            .collect::<Vec<_>>();
                                        let shadow_metrics = runtime_metrics.clone();
                                        window_session.spawn_trade(async move {
                                            let max_shadow_notional = shadow_notionals
                                                .iter()
                                                .copied()
                                                .max()
                                                .unwrap_or(dec!(1));
                                            let assessment = match shadow_executor
                                                .shadow_execution_assessment(
                                                    &candidate.opportunity,
                                                    &shadow_yes_dir,
                                                    &shadow_no_dir,
                                                    max_shadow_notional,
                                                )
                                                .await
                                            {
                                                Ok(quote) => quote,
                                                Err(error) => {
                                                    debug!(target: "polypulse::strategy::binary_complement_arbitrage",
                                                        %error,
                                                        source = candidate.source,
                                                        market = %shadow_market,
                                                        "REST shadow observation quote unavailable"
                                                    );
                                                    return;
                                                }
                                            };
                                            if let Some(rejection) = candidate.admission_rejection {
                                                shadow_metrics.record_arbitrage_admission_shadow(
                                                    assessment.passes_gross_edge,
                                                    assessment.passes_net_edge,
                                                );
                                                let logged_candidate = candidate.clone();
                                                let logged_assessment = assessment.clone();
                                                let logged_market = shadow_market.clone();
                                                let logged_path = path.clone();
                                                let logged_yes_dir = shadow_yes_dir.clone();
                                                let logged_no_dir = shadow_no_dir.clone();
                                                if let Err(error) = tokio::task::spawn_blocking(move || {
                                                    utils::arbitrage_logger::log_arbitrage_admission_shadow(
                                                        rejection,
                                                        &logged_candidate.opportunity,
                                                        &logged_assessment,
                                                        &logged_market,
                                                        &logged_yes_dir,
                                                        &logged_no_dir,
                                                        logged_candidate.observed_at_ms,
                                                        &logged_path,
                                                    )
                                                })
                                                .await
                                                .unwrap_or_else(|error| {
                                                    Err(anyhow::anyhow!(
                                                        "admission shadow logger task failed: {error}"
                                                    ))
                                                }) {
                                                    warn!(target: "polypulse::strategy::binary_complement_arbitrage",
                                                        %error,
                                                        source = candidate.source,
                                                        market = %shadow_market,
                                                        "Failed to persist admission shadow assessment"
                                                    );
                                                }
                                            }
                                            let samples = shadow_delays.into_iter().map(|delay_ms| {
                                                let sample_executor = shadow_executor.clone();
                                                let sample_candidate = candidate.clone();
                                                let sample_assessment = assessment.clone();
                                                let sample_market = shadow_market.clone();
                                                let sample_path = path.clone();
                                                let sample_metrics = shadow_metrics.clone();
                                                let sample_notionals = shadow_notionals.clone();
                                                async move {
                                                    let target_at = sample_candidate
                                                        .observed_at_ms
                                                        .saturating_add(
                                                            i64::try_from(delay_ms)
                                                                .unwrap_or(i64::MAX),
                                                        );
                                                    let remaining_ms = target_at.saturating_sub(
                                                        Utc::now().timestamp_millis(),
                                                    );
                                                    if remaining_ms > 0 {
                                                        sleep(Duration::from_millis(
                                                            u64::try_from(remaining_ms)
                                                                .unwrap_or(u64::MAX),
                                                        ))
                                                        .await;
                                                    }
                                                    let tiers = match sample_executor
                                                        .shadow_arbitrage_ladder(
                                                            &sample_candidate.opportunity,
                                                            &sample_assessment.quote,
                                                            &sample_notionals,
                                                        )
                                                        .await
                                                    {
                                                        Ok(tiers) => tiers,
                                                        Err(error) => {
                                                            warn!(target: "polypulse::strategy::binary_complement_arbitrage",
                                                                %error,
                                                                source = sample_candidate.source,
                                                                market = %sample_market,
                                                                %delay_ms,
                                                                "REST shadow snapshot failed"
                                                            );
                                                            return;
                                                        }
                                                    };
                                                    for tier in tiers {
                                                        sample_metrics.record_arbitrage_rest_shadow(
                                                            tier.snapshot.yes_fill.filled,
                                                            tier.snapshot.no_fill.filled,
                                                            tier.snapshot.error.is_some(),
                                                        );
                                                        let logged_candidate = sample_candidate.clone();
                                                        let logged_assessment = sample_assessment.clone();
                                                        let logged_market = sample_market.clone();
                                                        let logged_path = sample_path.clone();
                                                        if let Err(error) = tokio::task::spawn_blocking(move || {
                                                            utils::arbitrage_logger::log_arbitrage_rest_shadow(
                                                                logged_candidate.source,
                                                                logged_candidate.admission_rejection,
                                                                &logged_candidate.opportunity,
                                                                &logged_assessment,
                                                                &logged_market,
                                                                logged_candidate.observed_at_ms,
                                                                delay_ms,
                                                                tier.max_leg_notional_usdc,
                                                                &tier.snapshot,
                                                                &logged_path,
                                                            )
                                                        })
                                                        .await
                                                        .unwrap_or_else(|error| {
                                                            Err(anyhow::anyhow!(
                                                                "REST shadow logger task failed: {error}"
                                                            ))
                                                        }) {
                                                            warn!(target: "polypulse::strategy::binary_complement_arbitrage",
                                                                %error,
                                                                source = sample_candidate.source,
                                                                market = %sample_market,
                                                                %delay_ms,
                                                                "Failed to persist REST shadow snapshot"
                                                            );
                                                        }
                                                    }
                                                }
                                            });
                                            join_all(samples).await;
                                        });
                                    }
                                }
                                let Some(opp) = tick.opportunity else {
                                    continue;
                                };
                                if !strategy_controls.binary_complement_arbitrage_enabled() {
                                    debug!(target: "polypulse::strategy::binary_complement_arbitrage", market = %market_display, "Binary complement arbitrage entry paused by operator");
                                    continue;
                                }
                                            // Use the exact slippage-adjusted quote for profitability,
                                            // notional sizing and risk reservation.
                                            let quote = match executor.execution_quote(&opp, yes_dir, no_dir).await {
                                                Ok(quote) => quote,
                                                Err(e) => {
                                                    runtime_metrics.record_quote_rejection();
                                                    debug!(target: "polypulse::strategy::binary_complement_arbitrage", error = %e, market = %market_display, "Final quote rejected");
                                                    continue;
                                                }
                                            };
                                            if let Some(path) = config.arbitrage_log_file.clone() {
                                                let logged_opp = opp.clone();
                                                let logged_quote = quote.clone();
                                                let logged_market = market_display.clone();
                                                let observed_at_ms = Utc::now().timestamp_millis();
                                                tokio::task::spawn_blocking(move || {
                                                    if let Err(e) = utils::arbitrage_logger::log_arbitrage_opportunity(
                                                        &logged_opp,
                                                        &logged_quote,
                                                        &logged_market,
                                                        observed_at_ms,
                                                        &path,
                                                    ) {
                                                        error!(target: "polypulse::strategy::binary_complement_arbitrage", error = %e, "Failed to persist executable arbitrage opportunity");
                                                    }
                                                });
                                            }
                                            let order_size = quote.size;
                                            let total_cost = quote.total_cost;

                                            // Check risk exposure limit
                                            let position_tracker = _risk_manager.position_tracker();
                                            let current_exposure = position_tracker.calculate_exposure();

                                            let reservation_id = uuid::Uuid::new_v4().to_string();
                                            let Some(reservation) = position_tracker.reserve(reservation_id, total_cost) else {
                                                runtime_metrics.record_risk_rejection();
                                                warn!(target: "polypulse::strategy::binary_complement_arbitrage",
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
                                                warn!(target: "polypulse::strategy::binary_complement_arbitrage",
                                                    "�� ️ Positions severely imbalanced, skip arbitrage | market:{}",
                                                    market_display
                                                );
                                                continue; // Skip this arbitrage
                                            }

                                            if !strategy_controls.binary_complement_arbitrage_enabled() {
                                                debug!(target: "polypulse::strategy::binary_complement_arbitrage", market = %market_display, "Binary complement arbitrage paused before submission");
                                                continue;
                                            }

                                            // Check trade interval: min 3s between trades
                                            {
                                                let mut guard = last_trade_time.lock().await;
                                                let now = Instant::now();
                                                if let Some(last) = *guard {
                                                    if now.saturating_duration_since(last) < MIN_TRADE_INTERVAL {
                                                        let elapsed = now.saturating_duration_since(last).as_secs_f32();
                                                        debug!(target: "polypulse::strategy::binary_complement_arbitrage",
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
                                                warn!(target: "polypulse::strategy::binary_complement_arbitrage", %reason, "Live/canary safety gate rejected order pair");
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
                                                let observed_at_ms = Utc::now().timestamp_millis();
                                                info!(target: "polypulse::console::binary_complement_arbitrage",
                                                    market = %market_display,
                                                    market_id = %opp.market_id,
                                                    yes_ask = %opp.yes_ask_price,
                                                    no_ask = %opp.no_ask_price,
                                                    yes_signal_depth = %opp.yes_size,
                                                    no_signal_depth = %opp.no_size,
                                                    total_signal_price = %opp.total_cost,
                                                    gross_edge_per_share = %quote.gross_edge_per_share,
                                                    fee_per_share = %(quote.yes_fee_per_share + quote.no_fee_per_share),
                                                    net_edge_per_share = %quote.edge_per_share,
                                                    size = %order_size,
                                                    execution_cost = %total_cost,
                                                    expected_profit = %(quote.edge_per_share * order_size),
                                                    yes_book_age_ms = observed_at_ms.saturating_sub(opp.yes_book_timestamp_ms),
                                                    no_book_age_ms = observed_at_ms.saturating_sub(opp.no_book_timestamp_ms),
                                                    book_skew_ms = (opp.yes_book_timestamp_ms - opp.no_book_timestamp_ms).abs(),
                                                    exposure = %current_exposure,
                                                    "⚡ Executable arbitrage opportunity"
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
                            if let Some(started_at) = market_ws_disconnect_started_at.take() {
                                runtime_metrics.record_sdk_market_ws_downtime(
                                    u64::try_from(started_at.elapsed().as_millis())
                                        .unwrap_or(u64::MAX),
                                );
                            }
                            market_ws_reconnecting = false;
                            market_ws_seen_connected = true;
                            info!("SDK market WebSocket recovered internally");
                        }
                        MarketWsConnectionState::Reconnecting(attempt) => {
                            if market_ws_seen_connected && !market_ws_reconnecting {
                                runtime_metrics.record_sdk_market_ws_reconnect();
                                runtime_metrics.record_sdk_market_ws_reconnecting_transition();
                                market_ws_disconnect_started_at = Some(Instant::now());
                                monitor.invalidate_books();
                                session_runner.invalidate_transient_state();
                                warn!(attempt, "SDK market WebSocket reconnecting internally");
                            }
                            market_ws_reconnecting = market_ws_seen_connected;
                        }
                        MarketWsConnectionState::Disconnected
                        | MarketWsConnectionState::Connecting => {
                            if market_ws_seen_connected && !market_ws_reconnecting {
                                runtime_metrics.record_sdk_market_ws_reconnect();
                                runtime_metrics.record_sdk_market_ws_disconnected_transition();
                                market_ws_disconnect_started_at = Some(Instant::now());
                                monitor.invalidate_books();
                                session_runner.invalidate_transient_state();
                                warn!("SDK market WebSocket disconnected; cached books and arbitrage candidates invalidated");
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
                    let winner_exposure = winner_position_tracker.calculate_exposure();
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
                    let copy_snapshot = copy_runtime.snapshot();
                    dashboard.with_mut(|d| {
                        d.set_window(&window_label, seconds_until_end);
                        d.set_exposure(decimal_to_f64(exposure));
                        d.set_late_resolution_winner_trading_exposure(decimal_to_f64(winner_exposure));
                        let metrics = runtime_metrics.snapshot();
                        d.set_late_resolution_winner_trading_funnel(
                            metrics.late_resolution_winner_trading_scans,
                            metrics.late_resolution_winner_trading_signals,
                            metrics.late_resolution_winner_trading_attempts,
                            metrics.late_resolution_winner_trading_model_data_rejections
                                + metrics.late_resolution_winner_trading_model_market_rejections
                                + metrics.late_resolution_winner_trading_model_boundary_rejections
                                + metrics.late_resolution_winner_trading_model_ev_rejections,
                            metrics.late_resolution_winner_trading_awaiting_confirmation,
                            metrics.late_resolution_winner_trading_pending,
                            metrics.late_resolution_winner_trading_resolved,
                            metrics.late_resolution_winner_trading_redeemed,
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
                            metrics.late_resolution_winner_trading_model_data_rejections,
                            metrics.late_resolution_winner_trading_model_market_rejections,
                            metrics.late_resolution_winner_trading_model_boundary_rejections,
                            metrics.late_resolution_winner_trading_model_ev_rejections,
                            metrics.late_resolution_winner_trading_shadow_ev_failures,
                        );
                        d.set_strategy_states(
                            strategy_controls.state(StrategyKind::BinaryComplementArbitrage),
                            strategy_controls.state(StrategyKind::LateResolutionWinnerTrading),
                            strategy_controls.state(StrategyKind::WalletCopyTrading),
                            strategy_controls.blocked_reason(),
                        );
                        d.set_wallet_copy_trading_metrics(
                            copy_snapshot.whitelist_wallets,
                            copy_snapshot.signals_seen,
                            copy_snapshot.signals_rejected,
                            copy_snapshot.proposals,
                            copy_snapshot.fills,
                            copy_snapshot.open_positions,
                            copy_snapshot.nav_microusd as f64 / 1_000_000.0,
                            copy_snapshot.cash_microusd as f64 / 1_000_000.0,
                            copy_snapshot.last_poll_at_ms,
                            copy_snapshot.last_signal_at_ms,
                        );
                        let rtds_healthy = !late_resolution_winner_trading_available
                            || now.timestamp_millis().saturating_sub(
                                metrics.late_resolution_winner_trading_rtds_last_observed_at_ms,
                            ) <= config.late_resolution_winner_trading_feed_max_age_ms;
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

                    if !strategy_controls.late_resolution_winner_trading_enabled() {
                        winner_confirmations.clear();
                        winner_shadow_confirmations.clear();
                        for tracker in &mut winner_sensitivity_confirmations { tracker.clear(); }
                    }

                    // Window-end late-resolution-winner-trading (strategy B): when the window is
                    // nearly over and one side's ask is decided (>= min price)
                    // while the other has no/cheap asks, buy the winner and hold
                    // to settlement (pays 1.00). One attempt per window.
                    if strategy_controls.late_resolution_winner_trading_enabled() && seconds_until_end <= 120 {
                        debug!(target: "polypulse::strategy::late_resolution_winner_trading",
                            seconds_until_end,
                            enabled = strategy_controls.late_resolution_winner_trading_enabled(),
                            count = late_resolution_winner_trading_count.load(Ordering::Relaxed),
                            max_per_window = config.late_resolution_winner_trading_max_per_window,
                            "late-resolution-winner-trading window check"
                        );
                    }

                    // Settle paper late-resolution-winner-trading positions whose markets have
                    // resolved (~every 10s). Realized PnL reflects the ACTUAL
                    // win rate, unlike the optimistic est_pnl.
                    if late_resolution_winner_trading_available
                        && last_winner_settle_check.elapsed() >= Duration::from_secs(10)
                    {
                        last_winner_settle_check = Instant::now();
                        let winner_journal = late_resolution_winner_trading_journal
                            .as_ref()
                            .expect("late-resolution-winner-trading journal initialized");
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
                                                .record_late_resolution_winner_trading_settlement_empty_result();
                                            let retry_in_secs = winner_position_settlement_backoff
                                                .record_retry(&buy.slug, now.timestamp_millis());
                                            debug!(target: "polypulse::strategy::late_resolution_winner_trading",
                                                slug = %buy.slug,
                                                retry_in_secs,
                                                "Late-Resolution Winner Trading closed-market settlement query returned no market"
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
                                            settle_late_resolution_winner_trading(&buy, now, closed, &prices)
                                        }) {
                                            winner_position_settlement_backoff.clear(&buy.slug);
                                            match winner_journal.record_settled(&buy, pnl) {
                                                Ok(true) => {
                                                    runtime_metrics.record_late_resolution_winner_trading_resolved();
                                                    runtime_metrics
                                                        .record_late_resolution_winner_trading_asset_resolved(
                                                            &buy.asset_key(),
                                                        );
                                                }
                                                Ok(false) => continue,
                                                Err(e) => {
                                                    error!(target: "polypulse::strategy::late_resolution_winner_trading",
                                                        error = %e,
                                                        position_id = %buy.position_id,
                                                        slug = %buy.slug,
                                                        "Late-Resolution Winner Trading settlement persistence failed"
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
                                                info!(target: "polypulse::strategy::late_resolution_winner_trading",
                                                    cash_remaining = %snapshot.cash_remaining,
                                                    winner_realized_pnl = %snapshot.winner_realized_pnl,
                                                    winner_positions_open = snapshot.winner_positions_open,
                                                    "PAPER late-resolution-winner-trading cash settled"
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
                                                        runtime_metrics.record_late_resolution_winner_trading_redeemed();
                                                        if let Ok(token) = polymarket_client_sdk::types::U256::from_str(&buy.token_id) {
                                                            protected_winner_tokens
                                                                .write()
                                                                .expect("winner token lock poisoned")
                                                                .remove(&token);
                                                            winner_position_tracker
                                                                .record_sell_fill(token, buy.filled);
                                                        }
                                                    }
                                                    Ok(false) => {
                                                        live_safety.trip(format!(
                                                            "late-resolution-winner-trading completion {} violated lifecycle state",
                                                            buy.position_id
                                                        ));
                                                        service_health.set_safety_ok(false);
                                                    }
                                                    Err(error) => {
                                                        live_safety.trip(format!(
                                                            "late-resolution-winner-trading completion {} could not be persisted: {error}",
                                                            buy.position_id
                                                        ));
                                                        service_health.set_safety_ok(false);
                                                    }
                                                }
                                            }
                                            live_safety.record_winner_settlement(cost, won);
                                            runtime_metrics.record_late_resolution_winner_trading_settlement(pnl);
                                            runtime_metrics
                                                .set_late_resolution_winner_trading_pending(winner_journal.pending().len());
                                            dashboard.with_mut(|d| {
                                                d.record_late_resolution_winner_trading_settlement(
                                                    decimal_to_f64(pnl),
                                                    pnl > dec!(0),
                                                );
                                            });
                                            let winner_summary = winner_journal.paper_summary();
                                            info!(target: "polypulse::console::late_resolution_winner_trading",
                                                position_id = %buy.position_id,
                                                asset = %buy.asset_key(),
                                                slug = %buy.slug,
                                                side = if buy.token_is_yes { "YES" } else { "NO" },
                                                entry_price = %buy.price,
                                                filled = %buy.filled,
                                                entry_cost = %cost,
                                                payout = %payout,
                                                expected_profit_if_wins = %(buy.filled * (dec!(1) - buy.price)),
                                                realized_pnl = %pnl,
                                                cumulative_realized_pnl = %winner_summary.realized_pnl,
                                                open_positions = winner_summary.open_positions,
                                                "🏁 Late-Resolution Winner Trading settled (realized)"
                                            );
                                        } else {
                                            runtime_metrics
                                                .record_late_resolution_winner_trading_settlement_not_terminal();
                                            let retry_in_secs = winner_position_settlement_backoff
                                                .record_retry(&buy.slug, now.timestamp_millis());
                                            debug!(target: "polypulse::strategy::late_resolution_winner_trading",
                                                slug = %buy.slug,
                                                retry_in_secs,
                                                "Late-Resolution Winner Trading market found but outcome is not terminal"
                                            );
                                        }
                                    }
                                    Err(e) => {
                                        runtime_metrics.record_late_resolution_winner_trading_settlement_query_failure();
                                        let retry_in_secs = winner_position_settlement_backoff
                                            .record_retry(&buy.slug, now.timestamp_millis());
                                        warn!(target: "polypulse::strategy::late_resolution_winner_trading",
                                            error = %e,
                                            slug = %buy.slug,
                                            retry_in_secs,
                                            "Late-Resolution Winner Trading settlement query failed"
                                        );
                                    }
                                }
                            }
                        }
                        let evaluation_journal = winner_evaluation_journal
                            .as_ref()
                            .expect("Late-Resolution Winner Trading evaluation journal initialized");
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
                                            .record_late_resolution_winner_trading_settlement_empty_result();
                                        let retry_in_secs = winner_evaluation_settlement_backoff
                                            .record_retry(&slug, now.timestamp_millis());
                                        debug!(target: "polypulse::strategy::late_resolution_winner_trading",
                                            %slug,
                                            retry_in_secs,
                                            "Late-Resolution Winner Trading evaluation closed-market query returned no market"
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
                                                    .record_late_resolution_winner_trading_evaluations_resolved(resolved);
                                                debug!(target: "polypulse::strategy::late_resolution_winner_trading",
                                                    %slug,
                                                    resolved,
                                                    "Late-Resolution Winner Trading counterfactual evaluations resolved"
                                                );
                                            }
                                            Ok(_) => {
                                                runtime_metrics
                                                    .record_late_resolution_winner_trading_settlement_not_terminal();
                                                winner_evaluation_settlement_backoff
                                                    .record_retry(&slug, now.timestamp_millis());
                                            }
                                            Err(error) => {
                                                winner_evaluation_settlement_backoff
                                                    .record_retry(&slug, now.timestamp_millis());
                                                error!(target: "polypulse::strategy::late_resolution_winner_trading",
                                                    %error,
                                                    %slug,
                                                    "Late-Resolution Winner Trading counterfactual resolution persistence failed"
                                                );
                                            }
                                        }
                                    } else {
                                        runtime_metrics
                                            .record_late_resolution_winner_trading_settlement_not_terminal();
                                        winner_evaluation_settlement_backoff
                                            .record_retry(&slug, now.timestamp_millis());
                                    }
                                }
                                Err(error) => {
                                    runtime_metrics.record_late_resolution_winner_trading_settlement_query_failure();
                                    let retry_in_secs = winner_evaluation_settlement_backoff
                                        .record_retry(&slug, now.timestamp_millis());
                                    warn!(target: "polypulse::strategy::late_resolution_winner_trading",
                                        %error,
                                        %slug,
                                        retry_in_secs,
                                        "Late-Resolution Winner Trading counterfactual resolution query failed"
                                    );
                                }
                            }
                        }
                        if !config.dry_run && config.late_resolution_winner_trading_auto_redeem {
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
                                            "late-resolution-winner-trading {} has invalid condition id: {error}",
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
                                            "late-resolution-winner-trading {} has invalid token id: {error}",
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
                                            "late-resolution-winner-trading {} has invalid redeem size: {error}",
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
                                            "late-resolution-winner-trading redemption intent {} could not be persisted: {error}",
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
                                                "late-resolution-winner-trading redemption confirmation {} could not be persisted: {error}",
                                                buy.position_id
                                            ));
                                            service_health.set_safety_ok(false);
                                            continue;
                                        }
                                        protected_winner_tokens
                                            .write()
                                            .expect("winner token lock poisoned")
                                            .remove(&token);
                                        winner_position_tracker
                                            .record_sell_fill(token, buy.filled);
                                        runtime_metrics.record_late_resolution_winner_trading_redeemed();
                                        info!(target: "polypulse::strategy::late_resolution_winner_trading",
                                            position_id = %buy.position_id,
                                            %tx_hash,
                                            "Late-Resolution Winner Trading redemption confirmed"
                                        );
                                    }
                                    Err(error) => {
                                        error!(target: "polypulse::strategy::late_resolution_winner_trading",
                                            position_id = %buy.position_id,
                                            %error,
                                            "Late-Resolution Winner Trading redemption write is unresolved"
                                        );
                                        live_safety.trip(format!(
                                            "late-resolution-winner-trading redemption {} is unresolved",
                                            buy.position_id
                                        ));
                                        service_health.set_safety_ok(false);
                                    }
                                }
                            }
                        }
                    }
                    if strategy_controls.late_resolution_winner_trading_enabled()
                        && seconds_until_end > 0
                        && seconds_until_end <= config.late_resolution_winner_trading_before_end_secs
                        && matches!(
                            monitor.connection_state(),
                            MarketWsConnectionState::Connected
                        )
                        && user_ws_connected
                    {
                        let min_price = rust_decimal::Decimal::try_from(config.late_resolution_winner_trading_min_price)
                            .unwrap_or(dec!(0.95));
                        let max_price = rust_decimal::Decimal::try_from(config.late_resolution_winner_trading_max_price)
                            .unwrap_or(dec!(0.99));
                        let max_order = rust_decimal::Decimal::try_from(
                            config.late_resolution_winner_trading_max_order_usdc,
                        )
                        .unwrap_or(dec!(50));
                        let hybrid_max_order = rust_decimal::Decimal::try_from(
                            config.late_resolution_winner_trading_hybrid_paper_max_order_usdc,
                        )
                        .unwrap_or(dec!(5));
                        for market in &markets {
                            runtime_metrics.record_late_resolution_winner_trading_scan();
                            runtime_metrics
                                .record_late_resolution_winner_trading_asset_scan(&market.crypto_symbol);
                            let market_attempts = winner_attempts_by_market
                                .get(&market.slug)
                                .copied()
                                .unwrap_or(0);
                            if !attempt_allowed(
                                0,
                                market_attempts,
                                0,
                                config.late_resolution_winner_trading_max_attempts_per_market,
                            ) {
                                if config.late_resolution_winner_trading_max_per_window > 0
                                    && late_resolution_winner_trading_count.load(Ordering::Relaxed)
                                        >= config.late_resolution_winner_trading_max_per_window
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
                                    warn!(target: "polypulse::strategy::late_resolution_winner_trading", error = %e, "Late-Resolution Winner Trading token parse failed");
                                    continue;
                                }
                            };
                            let no_token = match polymarket_client_sdk::types::U256::from_str(
                                &market.no_token_id.to_string(),
                            ) {
                                Ok(t) => t,
                                Err(e) => {
                                    warn!(target: "polypulse::strategy::late_resolution_winner_trading", error = %e, "Late-Resolution Winner Trading token parse failed");
                                    continue;
                                }
                            };
                            // The live SDK WebSocket cache is the primary source.
                            // REST is only a fallback before the first WS snapshot.
                            let mut yes_top = monitor.winner_book_top(yes_token);
                            let mut no_top = monitor.winner_book_top(no_token);
                            let mut yes_from_rest = false;
                            let mut no_from_rest = false;
                            let max_book_age_ms = config.late_resolution_winner_trading_max_book_age_ms;
                            let yes_present = yes_top.is_some();
                            let no_present = no_top.is_some();
                            let preliminary_signal = detect_winner_asks(
                                yes_top.as_ref().and_then(|top| top.ask_price),
                                no_top.as_ref().and_then(|top| top.ask_price),
                                min_price,
                                max_price,
                            );
                            if yes_top.is_none() {
                                runtime_metrics.record_late_resolution_winner_trading_ws_missing_book();
                            }
                            if no_top.is_none() {
                                runtime_metrics.record_late_resolution_winner_trading_ws_missing_book();
                            }
                            let yes_fresh = yes_top.as_ref().is_some_and(|top| {
                                received_at_is_fresh(
                                    top.received_at_ms,
                                    now.timestamp_millis(),
                                    max_book_age_ms,
                                )
                            });
                            let no_fresh = no_top.as_ref().is_some_and(|top| {
                                received_at_is_fresh(
                                    top.received_at_ms,
                                    now.timestamp_millis(),
                                    max_book_age_ms,
                                )
                            });
                            if yes_present && !yes_fresh {
                                runtime_metrics.record_late_resolution_winner_trading_stale_book();
                                runtime_metrics.record_late_resolution_winner_trading_stale_ws_book();
                                yes_top = None;
                            }
                            if no_present && !no_fresh {
                                runtime_metrics.record_late_resolution_winner_trading_stale_book();
                                runtime_metrics.record_late_resolution_winner_trading_stale_ws_book();
                                no_top = None;
                            }
                            if yes_top.is_none() || no_top.is_none() {
                                if !winner_books_need_rest(
                                    matches!(
                                        monitor.connection_state(),
                                        MarketWsConnectionState::Connected
                                    ),
                                    yes_present,
                                    no_present,
                                    yes_fresh,
                                    no_fresh,
                                    preliminary_signal.is_some(),
                                ) {
                                    winner_confirmations.reject(&market.slug);
                                    winner_shadow_confirmations.reject(&market.slug);
                                    for tracker in &mut winner_sensitivity_confirmations { tracker.reject(&market.slug); }
                                    runtime_metrics.record_late_resolution_winner_trading_no_signal();
                                    runtime_metrics.record_late_resolution_winner_trading_asset_no_signal(
                                        &market.crypto_symbol,
                                    );
                                    continue;
                                }
                                let yes_requested = yes_top.is_none();
                                let no_requested = no_top.is_none();
                                if yes_top.is_none() {
                                    runtime_metrics.record_late_resolution_winner_trading_rest_fallback();
                                    yes_from_rest = true;
                                }
                                if no_top.is_none() {
                                    runtime_metrics.record_late_resolution_winner_trading_rest_fallback();
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
                                // A REST response can carry a freshly received
                                // timestamp even when the request itself spent
                                // tens of seconds blocked.  For a near-expiry
                                // strategy that is not actionable data, so fail
                                // closed on request latency before considering the
                                // returned book.
                                let rest_too_slow = rest_latency_ms
                                    > config.late_resolution_winner_trading_max_rest_latency_ms;
                                if rest_too_slow {
                                    runtime_metrics.record_late_resolution_winner_trading_stale_book();
                                    warn!(
                                        target: "polypulse::strategy::late_resolution_winner_trading",
                                        latency_ms = rest_latency_ms,
                                        limit_ms = config.late_resolution_winner_trading_max_rest_latency_ms,
                                        "Late-Resolution Winner Trading REST fallback exceeded latency limit"
                                    );
                                }
                                if yes_requested {
                                    if let Ok(top) = &yes_rest {
                                        runtime_metrics.record_late_resolution_winner_trading_rest_result(
                                            rest_latency_ms,
                                            top.is_some(),
                                        );
                                    }
                                }
                                if no_requested {
                                    if let Ok(top) = &no_rest {
                                        runtime_metrics.record_late_resolution_winner_trading_rest_result(
                                            rest_latency_ms,
                                            top.is_some(),
                                        );
                                    }
                                }
                                yes_top = match yes_rest {
                                    Ok(top) => top,
                                    Err(error) => {
                                        runtime_metrics.record_late_resolution_winner_trading_book_read_error();
                                        debug!(target: "polypulse::strategy::late_resolution_winner_trading", %error, token = %yes_token, "Late-Resolution Winner Trading YES book REST fallback failed");
                                        None
                                    }
                                };
                                no_top = match no_rest {
                                    Ok(top) => top,
                                    Err(error) => {
                                        runtime_metrics.record_late_resolution_winner_trading_book_read_error();
                                        debug!(target: "polypulse::strategy::late_resolution_winner_trading", %error, token = %no_token, "Late-Resolution Winner Trading NO book REST fallback failed");
                                        None
                                    }
                                };
                                if rest_too_slow {
                                    yes_top = None;
                                    no_top = None;
                                }
                            }
                            let rest_checked_at_ms = Utc::now().timestamp_millis();
                            if yes_top.as_ref().is_some_and(|top| {
                                !received_at_is_fresh(
                                    top.received_at_ms,
                                    rest_checked_at_ms,
                                    max_book_age_ms,
                                )
                            }) {
                                runtime_metrics.record_late_resolution_winner_trading_stale_book();
                                if yes_from_rest {
                                    runtime_metrics.record_late_resolution_winner_trading_stale_rest_book();
                                } else {
                                    runtime_metrics.record_late_resolution_winner_trading_stale_ws_book();
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
                                runtime_metrics.record_late_resolution_winner_trading_stale_book();
                                if no_from_rest {
                                    runtime_metrics.record_late_resolution_winner_trading_stale_rest_book();
                                } else {
                                    runtime_metrics.record_late_resolution_winner_trading_stale_ws_book();
                                }
                                no_top = None;
                            }
                            let yes_ask = yes_top.and_then(|top| top.ask_price);
                            let no_ask = no_top.and_then(|top| top.ask_price);
                            if yes_ask.is_none() && no_ask.is_none() {
                                winner_confirmations.reject(&market.slug);
                                winner_shadow_confirmations.reject(&market.slug);
                                for tracker in &mut winner_sensitivity_confirmations { tracker.reject(&market.slug); }
                                runtime_metrics.record_late_resolution_winner_trading_no_book();
                                runtime_metrics
                                    .record_late_resolution_winner_trading_asset_no_book(&market.crypto_symbol);
                                debug!(
                                    market = %market.crypto_symbol,
                                    "Late-Resolution Winner Trading: no executable asks, confirmation state paused"
                                );
                                continue;
                            }
                            let Some(signal) =
                                detect_winner_asks(yes_ask, no_ask, min_price, max_price)
                            else {
                                winner_confirmations.reject(&market.slug);
                                winner_shadow_confirmations.reject(&market.slug);
                                for tracker in &mut winner_sensitivity_confirmations { tracker.reject(&market.slug); }
                                runtime_metrics.record_late_resolution_winner_trading_no_signal();
                                runtime_metrics
                                    .record_late_resolution_winner_trading_asset_no_signal(&market.crypto_symbol);
                                debug!(
                                    market = %market.crypto_symbol,
                                    yes_ask = ?yes_ask,
                                    no_ask = ?no_ask,
                                    "Late-Resolution Winner Trading: winner not identifiable, skipping"
                                );
                                continue;
                            };
                            let token = if signal.buy_yes { yes_token } else { no_token };
                            runtime_metrics.record_late_resolution_winner_trading_signal();
                            runtime_metrics
                                .record_late_resolution_winner_trading_asset_signal(&market.crypto_symbol);
                            let selected_top = if signal.buy_yes { yes_top } else { no_top };
                            let selected_from_rest = if signal.buy_yes {
                                yes_from_rest
                            } else {
                                no_from_rest
                            };
                            let Some(selected_top) = selected_top else {
                                winner_confirmations.reject(&market.slug);
                                winner_shadow_confirmations.reject(&market.slug);
                                for tracker in &mut winner_sensitivity_confirmations { tracker.reject(&market.slug); }
                                continue;
                            };
                            let book_source = if selected_from_rest { "rest" } else { "ws" };
                            let book_age_ms = rest_checked_at_ms
                                .saturating_sub(selected_top.received_at_ms)
                                .max(0);
                            let (Some(bid_price), Some(ask_price), Some(ask_size)) = (
                                selected_top.bid_price,
                                selected_top.ask_price,
                                selected_top.ask_size,
                            ) else {
                                winner_confirmations.reject(&market.slug);
                                winner_shadow_confirmations.reject(&market.slug);
                                for tracker in &mut winner_sensitivity_confirmations { tracker.reject(&market.slug); }
                                runtime_metrics.record_late_resolution_winner_trading_incomplete_book();
                                debug!(
                                    market = %market.crypto_symbol,
                                    buy_yes = signal.buy_yes,
                                    bid = ?selected_top.bid_price,
                                    ask = ?selected_top.ask_price,
                                    "Late-Resolution Winner Trading selected book is one-sided; confirmation state paused"
                                );
                                continue;
                            };
                            let Some(fee_policy) = winner_fee_policies.get(&market.slug).copied()
                            else {
                                winner_confirmations.reject(&market.slug);
                                winner_shadow_confirmations.reject(&market.slug);
                                for tracker in &mut winner_sensitivity_confirmations { tracker.reject(&market.slug); }
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
                                late_resolution_winner_model_policy(
                                    &config,
                                    &market.crypto_symbol,
                                    fee_per_share,
                                ),
                            );
                            let evaluation_id = uuid::Uuid::new_v4().to_string();
                            let mut evaluation = WinnerEvaluation::from_model(
                                run_id.to_string(),
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
                                book_source.to_string(),
                                book_age_ms,
                                &model,
                            );
                            evaluation.research_cohorts = winner_research_cohorts(
                                &evaluation,
                                rust_decimal::Decimal::try_from(config.late_resolution_winner_trading_max_spread)
                                    .expect("validated late-resolution-winner-trading max spread"),
                                rust_decimal::Decimal::try_from(
                                    config.late_resolution_winner_trading_min_depth_usdc,
                                )
                                .expect("validated late-resolution-winner-trading minimum depth"),
                            );
                            let research_cohorts = evaluation.research_cohorts.clone();
                            let hybrid_paper_admitted = !model.admitted()
                                && config.late_resolution_winner_trading_hybrid_paper_enabled
                                && config.dry_run
                                // Hybrid PAPER is calibration only; it must
                                // never turn a negative-EV model output into
                                // an order lifecycle entry.
                                && model.net_ev >= rust_decimal::Decimal::ZERO
                                && research_cohorts
                                    .iter()
                                    .any(|cohort| cohort == "hybrid_chainlink_veto");
                            // A rejected candidate can be confirmed by both the
                            // sensitivity and main Shadow tracks in the same scan.
                            // Defer the sensitivity execution so both tracks can
                            // share one delayed book read and one persisted result.
                            let mut sensitivity_groups_to_execute = Vec::new();
                            evaluation.candidate_stage = if model.admitted() {
                                "production"
                            } else if hybrid_paper_admitted {
                                "hybrid_paper"
                            } else {
                                "rejected_shadow"
                            }
                            .to_string();
                            if let Err(error) = winner_evaluation_journal
                                .as_ref()
                                .expect("Late-Resolution Winner Trading evaluation journal initialized")
                                .record(evaluation.clone())
                            {
                                error!(target: "polypulse::strategy::late_resolution_winner_trading",
                                    %error,
                                    market = %market.crypto_symbol,
                                    "Late-Resolution Winner Trading evaluation persistence failed"
                                );
                                continue;
                            }
                            if winner_sensitivity_enabled {
                                let journal = winner_evaluation_journal.as_ref().expect("initialized journal");
                                let policy = late_resolution_winner_model_policy(&config, &market.crypto_symbol, fee_per_share);
                                let mut groups = Vec::new();
                                let mut decisions = Vec::new();
                                for (index, (floor, risk)) in [1.0, 0.5, 0.25, 0.1].into_iter()
                                    .flat_map(|f| [1.0, 0.5].into_iter().map(move |r| (f, r))).enumerate() {
                                    let group = format!("sensitivity_f{floor}_r{risk}");
                                    let key = format!("{}:{group}", market.slug);
                                    let (admitted, probability, net_ev) = crate::late_resolution_winner_model::sensitivity_decision(
                                        &model, policy, i64::from(seconds_until_end), floor, risk);
                                    let tracker = &mut winner_sensitivity_confirmations[index];
                                    let confirmed = if admitted {
                                        model.observed_at_ms.is_some_and(|t| tracker.observe(&market.slug, signal.buy_yes, t,
                                            config.late_resolution_winner_trading_confirmation_observations,
                                            config.late_resolution_winner_trading_feed_max_age_ms.saturating_mul(3)).confirmed)
                                    } else { tracker.reject(&market.slug); false };
                                    let attempted = winner_sensitivity_executed.contains(&key);
                                    decisions.push(serde_json::json!({"group":group,"floor_scale":floor,"risk_scale":risk,
                                        "probability":probability,"net_ev":net_ev,"admitted":admitted,
                                        "confirmed":confirmed,"already_attempted":attempted}));
                                    if confirmed && !attempted { groups.push(group); }
                                }
                                let persisted = journal.record_sensitivity(serde_json::json!({"run_id":run_id,
                                    "evaluation_id":evaluation_id,"slug":market.slug,"recorded_at":Utc::now(),
                                    "reference_verified":false,"groups":decisions}));
                                if let Err(error) = persisted {
                                    warn!(%error, "Winner sensitivity decision persistence failed");
                                } else if !groups.is_empty() {
                                    let size = fee_inclusive_depth_capped_size(signal.price, fee_per_share, hybrid_max_order, ask_size);
                                    if size > dec!(0) {
                                        info!(target: "polypulse::console::late_resolution_winner_trading",
                                            market = %market.crypto_symbol, %evaluation_id, groups = ?groups,
                                            price = %signal.price, %size, "Winner sensitivity Shadow opportunity confirmed");
                                        for group in &groups { winner_sensitivity_executed.insert(format!("{}:{group}", market.slug)); }
                                        if model.admitted() || hybrid_paper_admitted {
                                            // No rejected-shadow path will run for
                                            // admitted candidates, so execute the
                                            // sensitivity observation immediately.
                                            window_session.spawn_trade(record_winner_shadow(executor.clone(), journal.clone(),
                                                runtime_metrics.clone(), token, signal.price, size, fee_per_share,
                                                config.paper_late_resolution_winner_trading_delay_ms, run_id.to_string(),
                                                evaluation_id.clone(), market.slug.clone(), market.crypto_symbol.clone(),
                                                "sensitivity_confirmed".into(), groups));
                                        } else {
                                            sensitivity_groups_to_execute = groups;
                                        }
                                    }
                                }
                            }
                            runtime_metrics.record_late_resolution_winner_trading_evaluation();
                            runtime_metrics
                                .record_late_resolution_winner_trading_asset_evaluation(&market.crypto_symbol);
                            if model.failed_checks.contains(
                                &crate::late_resolution_winner_model::ModelRejection::NetEvTooLow,
                            ) {
                                runtime_metrics.record_late_resolution_winner_trading_shadow_ev_failure();
                            }
                            debug!(target: "polypulse::strategy::late_resolution_winner_trading",
                                %evaluation_id,
                                market = %market.crypto_symbol,
                                slug = %market.slug,
                                rejection = ?model.rejection,
                                fee_per_share = %fee_per_share,
                                p_model = %model.probability,
                                net_ev = %model.net_ev,
                                "Late-Resolution Winner Trading candidate evaluation persisted"
                            );
                            if let Some(rejection) = model.rejection {
                                // Preserve the strict-model funnel even when a
                                // PAPER-only hybrid candidate proceeds to simulation.
                                runtime_metrics.record_late_resolution_winner_model_rejection(rejection);
                            }
                            if !model.admitted() && !hybrid_paper_admitted {
                                winner_confirmations.reject(&market.slug);
                                debug!(target: "polypulse::strategy::late_resolution_winner_trading",
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
                                    "Late-Resolution Winner Trading rejected by independent Chainlink model"
                                );
                                if config.dry_run {
                                    let mut spawn_sensitivity_only = |groups: Vec<String>| {
                                        if groups.is_empty() {
                                            return;
                                        }
                                        let size = fee_inclusive_depth_capped_size(
                                            signal.price,
                                            fee_per_share,
                                            hybrid_max_order,
                                            ask_size,
                                        );
                                        if size > dec!(0) {
                                            window_session.spawn_trade(record_winner_shadow(
                                                executor.clone(),
                                                winner_evaluation_journal
                                                    .as_ref()
                                                    .expect("initialized journal")
                                                    .clone(),
                                                runtime_metrics.clone(),
                                                token,
                                                signal.price,
                                                size,
                                                fee_per_share,
                                                config.paper_late_resolution_winner_trading_delay_ms,
                                                run_id.to_string(),
                                                evaluation_id.clone(),
                                                market.slug.clone(),
                                                market.crypto_symbol.clone(),
                                                "sensitivity_confirmed".to_string(),
                                                groups,
                                            ));
                                        }
                                    };
                                    let Some(observed_at_ms) = model.observed_at_ms else {
                                        winner_shadow_confirmations.reject(&market.slug);
                                        spawn_sensitivity_only(std::mem::take(
                                            &mut sensitivity_groups_to_execute,
                                        ));
                                        continue;
                                    };
                                    let confirmation = winner_shadow_confirmations.observe(
                                        &market.slug,
                                        signal.buy_yes,
                                        observed_at_ms,
                                        config.late_resolution_winner_trading_confirmation_observations,
                                        config.late_resolution_winner_trading_feed_max_age_ms.saturating_mul(3),
                                    );
                                    let confirmation_observation = WinnerConfirmationObservation {
                                        run_id: run_id.to_string(),
                                        evaluation_id: evaluation_id.clone(),
                                        slug: market.slug.clone(),
                                        recorded_at: Utc::now(),
                                        model_observed_at_ms: observed_at_ms,
                                        confirmations: confirmation.confirmations,
                                        required: confirmation.required,
                                        confirmed: confirmation.confirmed,
                                        candidate_stage: "rejected_shadow".to_string(),
                                    };
                                    if let Err(error) = winner_evaluation_journal
                                        .as_ref()
                                        .expect("Late-Resolution Winner Trading evaluation journal initialized")
                                        .record_confirmation_observation(confirmation_observation)
                                    {
                                        error!(target: "polypulse::strategy::late_resolution_winner_trading",
                                            %error,
                                            %evaluation_id,
                                            market = %market.crypto_symbol,
                                            "Late-Resolution Winner Trading rejected-shadow confirmation persistence failed"
                                        );
                                        spawn_sensitivity_only(std::mem::take(
                                            &mut sensitivity_groups_to_execute,
                                        ));
                                        continue;
                                    }
                                    if !confirmation.confirmed {
                                        runtime_metrics
                                            .record_late_resolution_winner_trading_shadow_awaiting_confirmation();
                                        spawn_sensitivity_only(std::mem::take(
                                            &mut sensitivity_groups_to_execute,
                                        ));
                                        continue;
                                    }
                                    if winner_shadow_executed.contains(&market.slug) {
                                        spawn_sensitivity_only(std::mem::take(
                                            &mut sensitivity_groups_to_execute,
                                        ));
                                        continue;
                                    }
                                    let shadow_size = fee_inclusive_depth_capped_size(
                                        signal.price,
                                        fee_per_share,
                                        hybrid_max_order,
                                        ask_size,
                                    );
                                    if shadow_size > dec!(0) {
                                        winner_shadow_executed.insert(market.slug.clone());
                                        runtime_metrics.record_late_resolution_winner_trading_shadow_confirmed();
                                        runtime_metrics
                                            .record_late_resolution_winner_trading_shadow_execution_attempt();
                                        let mut combined_cohorts = research_cohorts.clone();
                                        combined_cohorts.append(
                                            &mut sensitivity_groups_to_execute,
                                        );
                                        combined_cohorts.sort();
                                        combined_cohorts.dedup();
                                        window_session.spawn_trade(record_winner_shadow(
                                            executor.clone(),
                                            winner_evaluation_journal
                                                .as_ref()
                                                .expect(
                                                    "Late-Resolution Winner Trading evaluation journal initialized",
                                                )
                                                .clone(),
                                            runtime_metrics.clone(),
                                            token,
                                            signal.price,
                                            shadow_size,
                                            fee_per_share,
                                            config.paper_late_resolution_winner_trading_delay_ms,
                                            run_id.to_string(),
                                            evaluation_id.clone(),
                                            market.slug.clone(),
                                            market.crypto_symbol.clone(),
                                            "rejected_confirmed".to_string(),
                                            combined_cohorts,
                                        ));
                                    }
                                }
                                continue;
                            }
                            let candidate_stage = if hybrid_paper_admitted {
                                info!(target: "polypulse::console::late_resolution_winner_trading",
                                    %evaluation_id,
                                    market = %market.crypto_symbol,
                                    ask_price = %signal.price,
                                    effective_price = %model.effective_price,
                                    spread = %model.spread,
                                    depth_usdc = %model.depth_notional,
                                    margin_bps = model.directional_margin_bps,
                                    failed_checks = ?model.failed_checks,
                                    "Late-Resolution Winner Trading PAPER hybrid candidate admitted"
                                );
                                "hybrid_paper"
                            } else {
                                "production"
                            };
                            let (stage_count, stage_max_per_window) = if hybrid_paper_admitted {
                                (
                                    &late_resolution_winner_trading_hybrid_count,
                                    config
                                        .late_resolution_winner_trading_hybrid_paper_max_per_window,
                                )
                            } else {
                                (
                                    &late_resolution_winner_trading_production_count,
                                    config.late_resolution_winner_trading_max_per_window,
                                )
                            };
                            if stage_max_per_window > 0
                                && stage_count.load(Ordering::Relaxed) >= stage_max_per_window
                            {
                                debug!(target: "polypulse::strategy::late_resolution_winner_trading",
                                    market = %market.crypto_symbol,
                                    stage = %candidate_stage,
                                    stage_max_per_window,
                                    "Late-Resolution Winner Trading stage-specific window limit reached"
                                );
                                continue;
                            }
                            winner_shadow_confirmations.reject(&market.slug);
                            let observed_at_ms = model
                                .observed_at_ms
                                .expect("admitted model has observation timestamp");
                            let confirmation = winner_confirmations.observe(
                                &market.slug,
                                signal.buy_yes,
                                observed_at_ms,
                                config.late_resolution_winner_trading_confirmation_observations,
                                config.late_resolution_winner_trading_feed_max_age_ms.saturating_mul(3),
                            );
                            let confirmation_observation = WinnerConfirmationObservation {
                                run_id: run_id.to_string(),
                                evaluation_id: evaluation_id.clone(),
                                slug: market.slug.clone(),
                                recorded_at: Utc::now(),
                                model_observed_at_ms: observed_at_ms,
                                confirmations: confirmation.confirmations,
                                required: confirmation.required,
                                confirmed: confirmation.confirmed,
                                candidate_stage: candidate_stage.to_string(),
                            };
                            if let Err(error) = winner_evaluation_journal
                                .as_ref()
                                .expect("Late-Resolution Winner Trading evaluation journal initialized")
                                .record_confirmation_observation(confirmation_observation)
                            {
                                error!(target: "polypulse::strategy::late_resolution_winner_trading",
                                    %error,
                                    %evaluation_id,
                                    market = %market.crypto_symbol,
                                    "Late-Resolution Winner Trading confirmation progress persistence failed"
                                );
                                continue;
                            }
                            if !confirmation.confirmed {
                                runtime_metrics.record_late_resolution_winner_trading_awaiting_confirmation();
                                debug!(target: "polypulse::strategy::late_resolution_winner_trading",
                                    market = %market.crypto_symbol,
                                    p_model = %model.probability,
                                    net_ev = %model.net_ev,
                                    confirmations = confirmation.confirmations,
                                    required = confirmation.required,
                                    "Late-Resolution Winner Trading awaiting consecutive Chainlink confirmations"
                                );
                                if config.dry_run {
                                    debug!(target: "polypulse::strategy::late_resolution_winner_trading",
                                        %evaluation_id,
                                        market = %market.crypto_symbol,
                                        "Late-Resolution Winner Trading shadow deferred until confirmation to avoid optimistic fill bias"
                                    );
                                }
                                continue;
                            }
                            // The configured maximum is a cash limit, so fees
                            // must be included when converting it to shares.
                            let size = fee_inclusive_depth_capped_size(
                                signal.price,
                                fee_per_share,
                                if hybrid_paper_admitted {
                                    hybrid_max_order
                                } else {
                                    max_order
                                },
                                ask_size,
                            );
                            if size <= dec!(0) {
                                continue;
                            }
                            if !strategy_controls.late_resolution_winner_trading_enabled() {
                                winner_confirmations.clear();
                                winner_shadow_confirmations.clear();
                                for tracker in &mut winner_sensitivity_confirmations { tracker.clear(); }
                                debug!(target: "polypulse::strategy::late_resolution_winner_trading", market = %market.crypto_symbol, "Late-Resolution Winner Trading paused before submission");
                                continue;
                            }
                            let position_id = uuid::Uuid::new_v4().to_string();
                            let planned_cost =
                                executor.winner_planned_cost(signal.price, size, fee_per_share);
                            let winner_journal = late_resolution_winner_trading_journal
                                .as_ref()
                                .expect("late-resolution-winner-trading journal initialized");
                            if let Err(reason) = winner_journal
                                .check_risk(&market.crypto_symbol, late_resolution_winner_trading_risk_policy)
                            {
                                runtime_metrics.record_risk_rejection();
                                warn!(target: "polypulse::strategy::late_resolution_winner_trading",
                                    market = %market.crypto_symbol,
                                    %reason,
                                    "Late-Resolution Winner Trading rejected by strategy/asset loss circuit breaker"
                                );
                                continue;
                            }
                            let position_tracker = winner_position_tracker.clone();
                            let Some(risk_reservation) = position_tracker.reserve(
                                format!("winner:{position_id}"),
                                planned_cost,
                            ) else {
                                runtime_metrics.record_risk_rejection();
                                warn!(target: "polypulse::strategy::late_resolution_winner_trading",
                                    market = %market.crypto_symbol,
                                    %planned_cost,
                                    exposure = %position_tracker.calculate_exposure(),
                                    limit = %position_tracker.max_exposure(),
                                    "Late-Resolution Winner Trading rejected by exposure limit"
                                );
                                continue;
                            };
                            if let Err(reason) = live_safety.try_start_winner(
                                window_session.timestamp(),
                                planned_cost,
                            ) {
                                runtime_metrics.record_risk_rejection();
                                warn!(target: "polypulse::strategy::late_resolution_winner_trading", %reason, "Live/canary safety gate rejected late-resolution-winner-trading");
                                continue;
                            }
                            let attempt = PendingLateResolutionWinnerTrading {
                                run_id: run_id.to_string(),
                                position_id,
                                evaluation_id,
                                candidate_stage: candidate_stage.to_string(),
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
                                    late_resolution_winner_trading_count.fetch_add(1, Ordering::Relaxed);
                                    stage_count.fetch_add(1, Ordering::Relaxed);
                                    *winner_attempts_by_market
                                        .entry(market.slug.clone())
                                        .or_default() += 1;
                                }
                                Ok(false) => {
                                    live_safety.trip(format!(
                                        "duplicate late-resolution-winner-trading attempt id {}",
                                        attempt.position_id
                                    ));
                                    service_health.set_safety_ok(false);
                                    continue;
                                }
                                Err(error) => {
                                    live_safety.trip(format!(
                                        "late-resolution-winner-trading attempt {} could not be persisted: {error}",
                                        attempt.position_id
                                    ));
                                    service_health.set_safety_ok(false);
                                    continue;
                                }
                            }
                            runtime_metrics.record_late_resolution_winner_trading_attempt();
                            runtime_metrics
                                .record_late_resolution_winner_trading_asset_attempt(&market.crypto_symbol);
                            let side = if signal.buy_yes { "YES" } else { "NO" };
                            info!(target: "polypulse::console::late_resolution_winner_trading",
                                market = %market.crypto_symbol,
                                stage = %candidate_stage,
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
                                model_expected_profit = %(model.net_ev * size),
                                secs_left = seconds_until_end,
                                "🛒 Late-Resolution Winner Trading order submitted"
                            );
                            let winner_execution_started = Instant::now();
                            match executor
                                .buy_winner(token, signal.price, size, fee_per_share)
                                .await
                            {
                                Ok(execution) => {
                                    runtime_metrics.record_late_resolution_winner_trading_execution_latency(
                                        execution.request_latency_ms,
                                        execution.total_latency_ms,
                                    );
                                    if execution.filled > dec!(0) {
                                        let filled = execution.filled;
                                        let fill_price = execution.average_price;
                                        position_tracker.record_buy_fill(
                                            token,
                                            fill_price,
                                            filled,
                                        );
                                        runtime_metrics.record_late_resolution_winner_trading_fill();
                                        runtime_metrics
                                            .record_late_resolution_winner_trading_asset_fill(&market.crypto_symbol);
                                        runtime_metrics.record_late_resolution_winner_trading_pnl(filled, fill_price);
                                        let est_pnl = filled * (dec!(1) - fill_price);
                                        dashboard.with_mut(|d| {
                                            d.record_late_resolution_winner_trading_fill(decimal_to_f64(est_pnl));
                                        });
                                        let pending = PendingLateResolutionWinnerTrading {
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
                                                runtime_metrics.set_late_resolution_winner_trading_pending(
                                                    winner_journal.pending().len(),
                                                );
                                            }
                                            Ok(false) => {
                                                live_safety.trip(format!(
                                                    "duplicate late-resolution-winner-trading fill {}",
                                                    attempt.position_id
                                                ));
                                                service_health.set_safety_ok(false);
                                            }
                                            Err(e) => {
                                                error!(target: "polypulse::strategy::late_resolution_winner_trading",
                                                    error = %e,
                                                    market = %market.crypto_symbol,
                                                    %side,
                                                    "Late-Resolution Winner Trading fill persistence failed"
                                                );
                                                live_safety.trip(format!(
                                                    "late-resolution-winner-trading fill {} could not be persisted",
                                                    market.slug
                                                ));
                                                service_health.set_safety_ok(false);
                                            }
                                        }
                                        info!(target: "polypulse::console::late_resolution_winner_trading",
                                            market = %market.crypto_symbol,
                                            %side,
                                            price = %fill_price,
                                            %filled,
                                            cost = %execution.cost,
                                            fee = %execution.estimated_fee,
                                            expected_profit_if_wins = %(filled - execution.cost),
                                            execution_request_latency_ms = execution.request_latency_ms,
                                            execution_total_latency_ms = execution.total_latency_ms,
                                            "✅ Late-Resolution Winner Trading filled; holding to settlement"
                                        );
                                    } else {
                                        if let Err(error) = winner_journal.record_no_fill(&attempt)
                                        {
                                            live_safety.trip(format!(
                                                "late-resolution-winner-trading no-fill {} could not be persisted: {error}",
                                                attempt.position_id
                                            ));
                                            service_health.set_safety_ok(false);
                                        }
                                        warn!(target: "polypulse::console::late_resolution_winner_trading",
                                            market = %market.crypto_symbol,
                                            %side,
                                            price = %signal.price,
                                            execution_request_latency_ms = execution.request_latency_ms,
                                            execution_total_latency_ms = execution.total_latency_ms,
                                            "Late-Resolution Winner Trading order not filled (no depth at limit)"
                                        );
                                    }
                                }
                                Err(e) => {
                                    let execution_attempt_latency_ms = u64::try_from(
                                        winner_execution_started.elapsed().as_millis(),
                                    )
                                    .unwrap_or(u64::MAX);
                                    if let Err(journal_error) =
                                        winner_journal.record_failed(&attempt, &e.to_string())
                                    {
                                        live_safety.trip(format!(
                                            "late-resolution-winner-trading failure {} could not be persisted: {journal_error}",
                                            attempt.position_id
                                        ));
                                        service_health.set_safety_ok(false);
                                    }
                                    warn!(target: "polypulse::console::late_resolution_winner_trading",
                                        market = %market.crypto_symbol,
                                        %side,
                                        error = %e,
                                        execution_attempt_latency_ms,
                                        "Late-Resolution Winner Trading failed"
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
                if profile != RuntimeProfile::LateResolutionWinnerTradingDryRun {
                    info!(target: "polypulse::strategy::binary_complement_arbitrage", metrics = %snapshot, "Binary Complement Arbitrage metrics snapshot");
                }
                if late_resolution_winner_trading_available {
                    info!(target: "polypulse::strategy::late_resolution_winner_trading", metrics = %snapshot, "Late-Resolution Winner Trading metrics snapshot");
                }
            }
            Err(error) => warn!(%error, "Failed to serialize runtime metrics"),
        }

        info!("Current window monitoring ended, refreshing markets for next round");
    }
}

#[allow(clippy::too_many_arguments)]
async fn record_winner_shadow(
    executor: Arc<TradingExecutor>,
    journal: Arc<WinnerEvaluationJournal>,
    metrics: Arc<RuntimeMetrics>,
    token: polymarket_client_sdk::types::U256,
    limit_price: rust_decimal::Decimal,
    size: rust_decimal::Decimal,
    fee_per_share: rust_decimal::Decimal,
    delay_ms: u64,
    run_id: String,
    evaluation_id: String,
    slug: String,
    asset: String,
    candidate_stage: String,
    research_cohorts: Vec<String>,
) {
    let result = executor
        .shadow_buy_winner(token, limit_price, size, fee_per_share)
        .await;
    let request_latency_ms = result.as_ref().ok().map(|r| r.request_latency_ms);
    let total_latency_ms = result.as_ref().ok().map(|r| r.total_latency_ms);
    let (outcome, filled, cost, average_price, estimated_fee, error) = match result {
        Ok(execution) if execution.filled > dec!(0) => (
            "filled",
            execution.filled,
            execution.cost,
            execution.average_price,
            execution.estimated_fee,
            None,
        ),
        Ok(_) => ("no_fill", dec!(0), dec!(0), dec!(0), dec!(0), None),
        Err(error) => (
            "error",
            dec!(0),
            dec!(0),
            dec!(0),
            dec!(0),
            Some(error.to_string()),
        ),
    };
    let execution = WinnerShadowExecution {
        request_latency_ms,
        total_latency_ms,
        run_id,
        evaluation_id: evaluation_id.clone(),
        slug,
        simulated_at: chrono::Utc::now(),
        delay_ms,
        candidate_stage: candidate_stage.clone(),
        research_cohorts,
        outcome: outcome.to_string(),
        limit_price,
        requested: size,
        filled,
        cost,
        average_price,
        estimated_fee,
        error,
    };
    match journal.record_shadow_execution(execution) {
        Ok(true) => {}
        Ok(false) => {
            debug!(target: "polypulse::strategy::late_resolution_winner_trading",
                %evaluation_id,
                market = %asset,
                stage = %candidate_stage,
                "Late-Resolution Winner Trading duplicate shadow result suppressed"
            );
            return;
        }
        Err(error) => {
            warn!(target: "polypulse::strategy::late_resolution_winner_trading",
                %error,
                %evaluation_id,
                market = %asset,
                stage = %candidate_stage,
                "Late-Resolution Winner Trading shadow persistence failed"
            );
            return;
        }
    }
    if candidate_stage != "sensitivity_confirmed" {
        match outcome {
            "filled" => metrics.record_late_resolution_winner_trading_shadow_execution_fill(),
            "no_fill" => metrics.record_late_resolution_winner_trading_shadow_execution_no_fill(),
            _ => metrics.record_late_resolution_winner_trading_shadow_execution_failure(),
        }
    }
    if candidate_stage == "sensitivity_confirmed" {
        info!(target: "polypulse::console::late_resolution_winner_trading", %evaluation_id,
            market = %asset, %outcome, %filled, %cost, ?request_latency_ms, ?total_latency_ms,
            "Winner sensitivity Shadow result; settlement PnL available in report");
    }
    debug!(target: "polypulse::strategy::late_resolution_winner_trading",
        %evaluation_id,
        market = %asset,
        stage = %candidate_stage,
        %outcome,
        %limit_price,
        requested = %size,
        %filled,
        %average_price,
        %cost,
        "Late-Resolution Winner Trading shadow execution recorded"
    );
}

fn late_resolution_winner_model_policy(
    config: &Config,
    asset: &str,
    fee_per_share: rust_decimal::Decimal,
) -> WinnerModelPolicy {
    let asset = asset.trim().to_ascii_lowercase();
    let (min_volatility, model_risk_buffer) = match asset.as_str() {
        "btc" => (
            config.late_resolution_winner_trading_btc_min_volatility_bps,
            config.late_resolution_winner_trading_btc_model_risk_buffer,
        ),
        "eth" => (
            config.late_resolution_winner_trading_eth_min_volatility_bps,
            config.late_resolution_winner_trading_eth_model_risk_buffer,
        ),
        _ => (
            config.late_resolution_winner_trading_other_min_volatility_bps,
            config.late_resolution_winner_trading_other_model_risk_buffer,
        ),
    };
    WinnerModelPolicy {
        max_feed_age_ms: config.late_resolution_winner_trading_feed_max_age_ms,
        max_opening_skew_ms: config.late_resolution_winner_trading_opening_max_skew_ms,
        volatility_lookback_secs: config.late_resolution_winner_trading_volatility_lookback_secs,
        min_observations: config.late_resolution_winner_trading_min_observations,
        min_volatility_bps_per_sqrt_second: min_volatility,
        min_margin_bps: config.late_resolution_winner_trading_min_margin_bps,
        reversal_sigma_multiplier: config.late_resolution_winner_trading_reversal_sigma,
        max_spread: rust_decimal::Decimal::try_from(
            config.late_resolution_winner_trading_max_spread,
        )
        .unwrap_or(dec!(0.03)),
        min_depth_usdc: rust_decimal::Decimal::try_from(
            config.late_resolution_winner_trading_min_depth_usdc,
        )
        .unwrap_or(dec!(10)),
        fee_buffer: fee_per_share,
        model_risk_buffer: rust_decimal::Decimal::try_from(model_risk_buffer).unwrap_or(dec!(0.01)),
        min_net_ev: rust_decimal::Decimal::try_from(
            config.late_resolution_winner_trading_min_net_ev,
        )
        .unwrap_or(dec!(0.002)),
    }
}

fn rate_pct(numerator: u64, denominator: u64) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 / denominator as f64 * 100.0
    }
}

fn log_binary_complement_paper_summary(
    executor: &TradingExecutor,
    runtime_metrics: &RuntimeMetrics,
    summary_interval_secs: u64,
) {
    let paper = executor.paper_arbitrage_summary();
    let metrics = runtime_metrics.snapshot();
    info!(target: "polypulse::console::binary_complement_arbitrage",
        opportunities_admitted = metrics.opportunities_admitted,
        trades_started = metrics.trades_started,
        preflight_attempts = paper.preflight_attempts,
        preflight_passes = paper.preflight_passes,
        preflight_pass_rate_pct = rate_pct(paper.preflight_passes, paper.preflight_attempts),
        preflight_rest_avg_ms = average_u64(paper.preflight_rest_ms_total, paper.preflight_attempts),
        preflight_rest_max_ms = paper.preflight_rest_ms_max,
        second_leg_attempts = paper.second_leg_attempts,
        second_leg_failures = paper.second_leg_failures,
        second_leg_failure_rate_pct = rate_pct(paper.second_leg_failures, paper.second_leg_attempts),
        second_leg_rest_avg_ms = average_u64(paper.second_leg_rest_ms_total, paper.second_leg_attempts),
        second_leg_rest_max_ms = paper.second_leg_rest_ms_max,
        emergency_unwind_attempts = paper.emergency_unwind_attempts,
        emergency_unwind_successes = paper.emergency_unwind_successes,
        emergency_unwind_success_rate_pct = rate_pct(paper.emergency_unwind_successes, paper.emergency_unwind_attempts),
        emergency_unwind_rest_avg_ms = average_u64(paper.emergency_unwind_rest_ms_total, paper.emergency_unwind_attempts),
        emergency_unwind_rest_max_ms = paper.emergency_unwind_rest_ms_max,
        paired_executions = paper.paired_executions,
        unrecovered_executions = paper.unrecovered_executions,
        realized_pnl = %paper.realized_pnl,
        cumulative_paper_pnl = %paper.cumulative_paper_pnl,
        unmatched_cost = %paper.unmatched_cost,
        summary_interval_secs,
        "📊 Binary Complement PAPER periodic summary"
    );
}

fn log_late_resolution_winner_paper_summary(
    runtime_metrics: &RuntimeMetrics,
    journal: &LateResolutionWinnerTradingJournal,
    summary_interval_secs: u64,
) {
    let metrics = runtime_metrics.snapshot();
    let paper = journal.paper_summary();
    let fallback_rest_samples = metrics
        .late_resolution_winner_trading_rest_successes
        .saturating_add(metrics.late_resolution_winner_trading_rest_empty_books);
    let settled = metrics
        .late_resolution_winner_trading_wins
        .saturating_add(metrics.late_resolution_winner_trading_losses);
    info!(target: "polypulse::console::late_resolution_winner_trading",
        scans = metrics.late_resolution_winner_trading_scans,
        signals = metrics.late_resolution_winner_trading_signals,
        signal_rate_pct = rate_pct(metrics.late_resolution_winner_trading_signals, metrics.late_resolution_winner_trading_scans),
        evaluations = metrics.late_resolution_winner_trading_evaluations,
        awaiting_confirmation = metrics.late_resolution_winner_trading_awaiting_confirmation,
        attempts = metrics.late_resolution_winner_trading_attempts,
        fills = metrics.late_resolution_winner_trading_fills,
        fill_rate_pct = rate_pct(metrics.late_resolution_winner_trading_fills, metrics.late_resolution_winner_trading_attempts),
        pending = metrics.late_resolution_winner_trading_pending,
        resolved = metrics.late_resolution_winner_trading_resolved,
        redeemed = metrics.late_resolution_winner_trading_redeemed,
        wins = metrics.late_resolution_winner_trading_wins,
        losses = metrics.late_resolution_winner_trading_losses,
        win_rate_pct = rate_pct(metrics.late_resolution_winner_trading_wins, settled),
        model_data_rejections = metrics.late_resolution_winner_trading_model_data_rejections,
        model_market_rejections = metrics.late_resolution_winner_trading_model_market_rejections,
        model_boundary_rejections = metrics.late_resolution_winner_trading_model_boundary_rejections,
        model_ev_rejections = metrics.late_resolution_winner_trading_model_ev_rejections,
        fallback_rest_samples,
        fallback_rest_avg_ms = average_u64(metrics.late_resolution_winner_trading_rest_latency_ms_total, fallback_rest_samples),
        fallback_rest_max_ms = metrics.late_resolution_winner_trading_rest_latency_ms_max,
        execution_samples = metrics.late_resolution_winner_trading_execution_samples,
        execution_request_avg_ms = average_u64(metrics.late_resolution_winner_trading_execution_request_latency_ms_total, metrics.late_resolution_winner_trading_execution_samples),
        execution_request_max_ms = metrics.late_resolution_winner_trading_execution_request_latency_ms_max,
        execution_total_avg_ms = average_u64(metrics.late_resolution_winner_trading_execution_total_latency_ms_total, metrics.late_resolution_winner_trading_execution_samples),
        execution_total_max_ms = metrics.late_resolution_winner_trading_execution_total_latency_ms_max,
        estimated_pnl = metrics.late_resolution_winner_trading_est_pnl_microusd as f64 / 1_000_000.0,
        realized_pnl = metrics.late_resolution_winner_trading_realized_pnl_microusd as f64 / 1_000_000.0,
        pending_cost = %paper.pending_cost,
        total_spent = %paper.total_spent,
        journal_realized_pnl = %paper.realized_pnl,
        open_positions = paper.open_positions,
        summary_interval_secs,
        "📊 Late-Resolution Winner Trading PAPER periodic summary"
    );
}

fn average_u64(total: u64, count: u64) -> f64 {
    if count == 0 {
        0.0
    } else {
        total as f64 / count as f64
    }
}
