use anyhow::{Context, Result};
use chrono::Utc;
use dotenvy::dotenv;
use fs2::FileExt;
use polypulse::copy_trading::{
    init_copy_trader_logging, BookFetcher, CopyTradeExecutor, CopyTradeJournal, CopyTradingConfig,
    CopyTradingCoordinator, CopyTradingRuntime, ExecutionOrderProposal, GammaSettlementSource,
    LadderExitConfig, LadderExitTracker, MarketMetadata, MarketMetadataFetcher, OnChainTradeFeed,
    OrderBookSnapshot, OrderSide, ProcessedSignalStore, ScoutReport, SettlementMonitor,
    SignalAction, SignalEvent, TradeFeed, WalletHistoryFetcher, WalletProfile, WalletQualityPolicy,
    WalletQualityTracker,
};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::HashMap;
use std::env;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration as StdDuration, Instant};
use tokio::time::{sleep, Duration};
use tracing::{debug, error, info, warn};

/// Cache TTLs for upstream API data (avoids hammering gamma/clob on every poll).
const MARKET_CACHE_TTL: StdDuration = StdDuration::from_secs(3600);
const BOOK_CACHE_TTL: StdDuration = StdDuration::from_secs(15);
const BOOK_NEGATIVE_CACHE_TTL: StdDuration = StdDuration::from_secs(60);

type MarketCache = Arc<Mutex<HashMap<String, (Instant, Option<MarketMetadata>)>>>;

#[derive(Clone)]
enum CachedBook {
    Available(OrderBookSnapshot),
    Unavailable(String),
}

type BookCache = Arc<Mutex<HashMap<String, (Instant, CachedBook)>>>;

#[tokio::main]
#[allow(dead_code)] // also included as a module by the primary polypulse binary
async fn main() -> Result<()> {
    // 1. Initialize environment & logging (stdout + rotating file, see
    //    LOG_FILE / LOG_ROTATE_MAX_BYTES / LOG_ROTATE_FILES).
    dotenv().ok();
    init_copy_trader_logging("logs/copy-trader.log")?;

    let configured_dry_run = env::var("COPY_DRY_RUN")
        .map(|value| value.trim() == "1" || value.trim().eq_ignore_ascii_case("true"))
        .unwrap_or(true);
    if !configured_dry_run {
        anyhow::bail!(
            "standalone copy_trader is restricted to stage-2 PAPER mode; set COPY_DRY_RUN=true. Real execution is only allowed through the interactive polypulse TUI"
        );
    }

    let enabled = env::var("COPY_TRADING_ENABLED")
        .map(|v| v.trim().to_lowercase() != "false" && v.trim() != "0")
        .unwrap_or(true);
    if !enabled {
        info!("COPY_TRADING_ENABLED=false — copy-trading disabled, exiting.");
        return Ok(());
    }
    let shutdown = Arc::new(AtomicBool::new(false));
    standalone_ctrlc_handler(shutdown.clone());
    let runtime = CopyTradingRuntime::new(shutdown, true);
    run_copy_trader(runtime, None, false).await
}

/// Runs Copy Trading inside the main polypulse process. Embedded operation is
/// intentionally PAPER-only until all three live strategies share one account
/// reservation ledger. TUI entry pause does not stop exits or settlement.
pub async fn run_embedded(runtime: CopyTradingRuntime, budget_usdc: Decimal) -> Result<()> {
    run_copy_trader(runtime, Some(budget_usdc), true).await
}

async fn run_copy_trader(
    runtime: CopyTradingRuntime,
    budget_override: Option<Decimal>,
    embedded: bool,
) -> Result<()> {
    let lock_path = env::var("COPY_RUNTIME_LOCK_FILE")
        .unwrap_or_else(|_| "data/copy-trading-runtime.lock".into());
    if let Some(parent) = std::path::Path::new(&lock_path).parent() {
        std::fs::create_dir_all(parent)?;
    }
    let runtime_lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("open Copy Trading runtime lock {lock_path}"))?;
    runtime_lock.try_lock_exclusive().with_context(|| {
        format!("Copy Trading is already running (lock {lock_path}); do not run standalone and embedded instances together")
    })?;

    info!("=================================================================");
    info!("   Polymarket Multi-Factor Copy-Trading System (V2)             ");
    info!("   Strategy: 5-Stage Funnel + Strict Limit + Multi-Consensus    ");
    info!("   Data: data-api / gamma / clob public endpoints (live feed)   ");
    info!("=================================================================");

    // 2. Load Configuration from Environment Variables / Defaults
    let config = CopyTradingConfig::from_env();
    config.validate().map_err(anyhow::Error::msg)?;
    let configured_nav = budget_override.unwrap_or_else(|| {
        env::var("COPY_TRADING_BUDGET_USDC")
            .or_else(|_| env::var("COPY_INITIAL_NAV_USDC"))
            .ok()
            .and_then(|v| v.trim().parse::<Decimal>().ok())
            .unwrap_or(dec!(10_000.0))
    });

    let dry_run = embedded
        || env::var("COPY_DRY_RUN")
            .map(|v| v.trim().to_lowercase() == "true" || v.trim() == "1")
            .unwrap_or(true);
    let poll_interval = Duration::from_secs(
        env::var("COPY_POLL_INTERVAL_SECONDS")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .unwrap_or(15)
            .clamp(5, 300),
    );
    let wallet_reevaluation_loops =
        (Duration::from_secs(24 * 60 * 60).as_secs() / poll_interval.as_secs()).max(1);
    let quality_audit_minutes = env::var("COPY_WALLET_QUALITY_AUDIT_MINUTES")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(360)
        .clamp(10, 24 * 60);
    let quality_audit_loops = (Duration::from_secs(quality_audit_minutes * 60).as_secs()
        / poll_interval.as_secs())
    .max(1);
    let auto_rotation = dry_run
        && env::var("COPY_AUTO_ROTATION_ENABLED")
            .map(|value| value.trim() == "1" || value.trim().eq_ignore_ascii_case("true"))
            .unwrap_or(false);
    let rotation_report = env::var("COPY_ROTATION_CANDIDATE_REPORT")
        .unwrap_or_else(|_| "results/copy-candidates/rotation-current/report.json".to_string());
    let quality_policy = WalletQualityPolicy::from_env();
    let quality_state_file = env::var("COPY_WALLET_QUALITY_STATE_FILE")
        .unwrap_or_else(|_| "data/copy-wallet-quality.json".to_string());
    let quality_tracker = WalletQualityTracker::new_with_state(&quality_state_file);

    // Live execution engine (only constructed when actually going live).
    let live_executor: Option<Arc<CopyTradeExecutor>> = if dry_run {
        None
    } else {
        info!("Initializing live CLOB execution engine...");
        let executor = CopyTradeExecutor::new_from_env().await?;
        executor.verify_authentication().await?;
        Some(Arc::new(executor))
    };

    // Initial NAV for the risk engine. In live mode prefer the REAL USDC balance
    // so exposure caps (10%/market, 30%/portfolio) are computed against actual
    // capital; fall back to the configured value when the balance is unreadable.
    let initial_nav = if dry_run {
        configured_nav
    } else {
        match live_executor.as_ref().unwrap().usdc_balance().await {
            Ok(bal) if bal > Decimal::ZERO => bal,
            Ok(_) => {
                warn!(
                    "Live USDC balance is zero/empty; using configured initial NAV ${:.2}",
                    configured_nav
                );
                configured_nav
            }
            Err(e) => {
                warn!(error = %e, "Could not read live USDC balance; using configured initial NAV ${:.2}", configured_nav);
                configured_nav
            }
        }
    };

    info!("Configuration Loaded:");
    info!(" -> Initial NAV:               ${:.2} USDC", initial_nav);
    info!(
        " -> Execution Mode:            {}",
        if dry_run {
            "PAPER / DRY-RUN (Safe Simulation)"
        } else {
            "LIVE (real CLOB orders)"
        }
    );
    info!(
        max_signal_age_seconds = config.max_signal_age_seconds,
        shadow_max_signal_age_seconds = config.shadow_max_signal_age_seconds,
        min_signal_notional = %config.min_signal_notional_usdc,
        shadow_min_signal_notional = %config.shadow_min_signal_notional_usdc,
        min_consensus_wallets = config.min_consensus_wallets,
        paper_execution_delay_ms = config.paper_execution_delay_ms,
        " -> Signal and PAPER research policy"
    );

    // Structured results journal (NDJSON) for post-run regression analysis.
    let mode = if dry_run { "paper" } else { "live" };
    let run_id = uuid::Uuid::new_v4().to_string();
    let journal = CopyTradeJournal::new(run_id.clone());
    let processed_signals = ProcessedSignalStore::open(
        env::var("COPY_SIGNAL_STATE_FILE")
            .unwrap_or_else(|_| "data/copy-signals-seen.json".to_string()),
        50_000,
    );
    let results_dir = env::var("COPY_TRADING_RESULTS_DIR")
        .unwrap_or_else(|_| format!("results/copy-trading/{}", mode));
    let events_file = format!("{}/events.ndjson", results_dir);
    match journal.enable(&events_file) {
        Ok(()) => {
            info!(run_id = %run_id, file = %events_file, "Copy-trading results journal enabled")
        }
        Err(e) => warn!(error = %e, "Results journal could not be enabled; continuing without it"),
    }
    journal.record_run_meta(
        mode,
        serde_json::json!({
            "initialNav": initial_nav.to_string(),
            "dryRun": dry_run,
            "targetWallets": env::var("COPY_TARGET_WALLETS").unwrap_or_default(),
            "maxSignalAgeSeconds": config.max_signal_age_seconds,
            "shadowMaxSignalAgeSeconds": config.shadow_max_signal_age_seconds,
            "minSignalNotional": config.min_signal_notional_usdc.to_string(),
            "shadowMinSignalNotional": config.shadow_min_signal_notional_usdc.to_string(),
            "minConsensusWallets": config.min_consensus_wallets,
            "paperExecutionDelayMs": config.paper_execution_delay_ms,
            "autoRotation": auto_rotation,
            "qualityAuditMinutes": quality_audit_minutes,
            "rotationCandidateReport": rotation_report,
            "qualityStateFile": quality_state_file,
        }),
    );

    let now = Utc::now();
    // Whitelist persistence (Opinion 4): profiles + strike counters survive
    // restarts, enabling warm-start without re-fetching all Data-API history.
    let whitelist_state_file = env::var("COPY_WHITELIST_STATE_FILE")
        .unwrap_or_else(|_| "data/copy-whitelist.json".to_string());
    let risk_state_file = env::var("COPY_RISK_STATE_FILE")
        .unwrap_or_else(|_| format!("data/copy-risk-{}.json", mode));
    let coordinator = Arc::new(CopyTradingCoordinator::new_with_state_paths(
        config.clone(),
        initial_nav,
        now,
        whitelist_state_file,
        risk_state_file,
    ));
    if embedded {
        coordinator
            .risk()
            .read()
            .await
            .reconcile_paper_budget(configured_nav);
    }
    if let Some(executor) = &live_executor {
        match executor.usdc_balance().await {
            Ok(actual) => coordinator
                .risk()
                .read()
                .await
                .reconcile_cash_balance(actual),
            Err(e) => {
                warn!(error = %e, "Startup cash reconciliation unavailable; restored risk cash retained")
            }
        }
        let restored = coordinator.risk().read().await.list_exposures();
        for exposure in restored {
            match executor.token_balance_str(&exposure.token_id).await {
                Ok(actual) => coordinator
                    .risk()
                    .read()
                    .await
                    .reconcile_position_shares(&exposure.token_id, actual),
                Err(e) => {
                    warn!(token = %exposure.token_id, error = %e, "Startup token reconciliation failed")
                }
            }
        }
    }

    // 3. Live data infrastructure
    let feed = Arc::new(TradeFeed::with_max_signal_age_seconds(if dry_run {
        config.shadow_max_signal_age_seconds
    } else {
        config.max_signal_age_seconds
    })?);
    let history = Arc::new(WalletHistoryFetcher::new()?);
    let meta_fetcher = Arc::new(MarketMetadataFetcher::new()?);
    let book_fetcher = Arc::new(BookFetcher::new()?);
    let market_cache: MarketCache = Arc::new(Mutex::new(HashMap::new()));
    let book_cache: BookCache = Arc::new(Mutex::new(HashMap::new()));
    let settlement_monitor = SettlementMonitor::new(Arc::new(GammaSettlementSource::new()?));
    let ladder_tracker = LadderExitTracker::new(LadderExitConfig::from_env());

    // Sybil defense (Opinion 7): load operator-declared correlated wallet pairs
    // so same-source wallets cannot manufacture consensus.
    coordinator.consensus().load_correlated_from_env();

    // Optional on-chain OrderFilled feed (Opinion 1): bypasses the Data-API
    // indexing delay for whitelisted wallet fills.
    let onchain_feed: Option<Arc<OnChainTradeFeed>> = if env::var("COPY_ONCHAIN_FEED_ENABLED")
        .map(|v| v.trim().to_lowercase() == "true" || v.trim() == "1")
        .unwrap_or(false)
    {
        let rpc = env::var("COPY_ONCHAIN_RPC_URL")
            .unwrap_or_else(|_| "https://polygon-bor-rpc.publicnode.com".to_string());
        match OnChainTradeFeed::new(&rpc).await {
            Ok(feed) => {
                info!("On-chain OrderFilled feed enabled (RPC: {})", rpc);
                Some(Arc::new(feed))
            }
            Err(e) => {
                warn!(error = %e, "On-chain feed init failed; falling back to Data-API polling");
                None
            }
        }
    } else {
        None
    };

    // 4. Seed target wallets from env, or discover from the profit leaderboard
    let configured_wallets: Vec<String> = env::var("COPY_TARGET_WALLETS")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let target_wallets: Vec<String> = if configured_wallets.is_empty() {
        if !dry_run {
            anyhow::bail!("COPY_TARGET_WALLETS must be explicitly configured in live mode");
        }
        info!(
            "No COPY_TARGET_WALLETS specified. Discovering candidates from profit leaderboard..."
        );
        match history.discover_candidates(30).await {
            Ok(cands) => cands,
            Err(e) => {
                warn!("Leaderboard discovery failed: {e:#}");
                Vec::new()
            }
        }
    } else {
        configured_wallets
    };

    if target_wallets.is_empty() {
        warn!("No target wallets available; copy-trading will idle until COPY_TARGET_WALLETS is configured.");
    } else {
        info!(
            "Profiling {} target wallets from real Polymarket history...",
            target_wallets.len()
        );
        let refresh_days = env::var("COPY_WHITELIST_REFRESH_DAYS")
            .ok()
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(7);
        for addr in &target_wallets {
            // Warm-start: reuse a fresh persisted profile instead of re-fetching
            // history (avoids Data-API rate limits on restart).
            if let Some(fresh) = coordinator
                .whitelist()
                .get_fresh_profile(addr, refresh_days, now)
            {
                journal.record(
                    "wallet_warmstart",
                    serde_json::json!({
                        "address": addr, "tier": format!("{:?}", fresh.tier),
                        "lastEvaluated": fresh.last_evaluated_at.to_rfc3339(),
                    }),
                );
                info!(
                    " -> Warm-started wallet {} (Tier: {:?}, last evaluated {})",
                    addr, fresh.tier, fresh.last_evaluated_at
                );
                continue;
            }
            match history.fetch_records(addr).await {
                Ok(records) => {
                    let res = coordinator.whitelist().evaluate_wallet(addr, &records, now);
                    if res.is_accepted {
                        let tier = res.tier;
                        let settled = res.profile.total_settled_events;
                        coordinator.whitelist().upsert_wallet(res.profile);
                        journal.record("wallet_enrolled", serde_json::json!({
                            "address": addr, "tier": format!("{:?}", tier), "settledEvents": settled,
                        }));
                        info!(
                            " -> Enrolled wallet {} (Tier: {:?}, settled: {})",
                            addr, tier, settled
                        );
                    } else {
                        journal.record(
                            "wallet_rejected",
                            serde_json::json!({
                                "address": addr, "reasons": res.failure_reasons,
                            }),
                        );
                        warn!(
                            " -> Wallet {} rejected by funnel: {:?}",
                            addr, res.failure_reasons
                        );
                    }
                }
                Err(e) => warn!(" -> Failed to fetch history for {}: {e:#}", addr),
            }
        }
    }

    let target_keys = target_wallets
        .iter()
        .map(|address| address.to_ascii_lowercase())
        .collect::<std::collections::HashSet<_>>();
    let active_profiles = coordinator
        .whitelist()
        .list_active_whitelist()
        .into_iter()
        .filter(|profile| target_keys.contains(&profile.address.to_ascii_lowercase()))
        .collect::<Vec<_>>();
    let tier_s_wallets = active_profiles
        .iter()
        .filter(|profile| profile.tier == polypulse::copy_trading::WalletTier::TierS)
        .count();
    let mut domain_sources = HashMap::<String, usize>::new();
    for profile in &active_profiles {
        *domain_sources
            .entry(profile.primary_domain.to_string())
            .or_default() += 1;
    }
    let consensus_capable_domains = domain_sources
        .iter()
        .filter(|(_, count)| **count >= config.min_consensus_wallets)
        .map(|(domain, count)| format!("{domain}:{count}"))
        .collect::<Vec<_>>();
    let direct_or_consensus_capable = tier_s_wallets > 0 || !consensus_capable_domains.is_empty();
    journal.record(
        "strategy_readiness",
        serde_json::json!({
            "configuredWallets": target_wallets.len(),
            "activeWallets": active_profiles.len(),
            "tierSWallets": tier_s_wallets,
            "minConsensusWallets": config.min_consensus_wallets,
            "domainSources": domain_sources,
            "consensusCapableDomains": consensus_capable_domains,
            "directOrConsensusCapable": direct_or_consensus_capable,
        }),
    );
    if !direct_or_consensus_capable {
        warn!(
            active_wallets = active_profiles.len(),
            tier_s_wallets,
            min_consensus_wallets = config.min_consensus_wallets,
            "Copy-trading cannot currently produce an entry: no TierS source and no domain has enough independent wallets"
        );
    } else if active_profiles.len() < target_wallets.len() {
        warn!(
            configured_wallets = target_wallets.len(),
            active_wallets = active_profiles.len(),
            "Copy-trading wallet funnel has low coverage; review wallet_rejected events"
        );
    }

    info!(
        "Strategy coordinator initialized. Polling live wallet trades every {}s...",
        poll_interval.as_secs()
    );

    let coordinator_clone = coordinator.clone();
    let feed_clone = feed.clone();
    let history_for_loop = history.clone();
    let meta_clone = meta_fetcher.clone();
    let book_clone = book_fetcher.clone();
    let mcache = market_cache.clone();
    let bcache = book_cache.clone();
    let settle_monitor = settlement_monitor.clone();
    let ladder_for_loop = ladder_tracker.clone();
    let journal_for_loop = journal.clone();
    let executor_for_loop = live_executor.clone();
    let onchain_for_loop = onchain_feed.clone();
    let signal_config = config.clone();
    let quality_for_loop = quality_tracker.clone();
    let quality_policy_for_loop = quality_policy.clone();
    let rotation_report_for_loop = rotation_report.clone();
    runtime.set_initialized(true);
    runtime.set_healthy(true);
    runtime.set_whitelist_wallets(active_profiles.len());
    let runtime_for_loop = runtime.clone();
    let runner_handle = tokio::spawn(async move {
        let mut loop_count: u64 = 0;
        while !runtime_for_loop.shutdown_requested() {
            let current_time = Utc::now();
            runtime_for_loop.record_poll(current_time.timestamp_millis());

            // 1. Heartbeat: TTL cancellations, daily rollover, risk health
            let cancelled = coordinator_clone.tick(current_time).await;
            if !cancelled.is_empty() {
                info!(
                    "Heartbeat: Cancelled {} expired TTL orders",
                    cancelled.len()
                );
                // Live mode: also cancel resting exchange orders for GTC/GTD mode.
                if let Some(executor) = &executor_for_loop {
                    for order_id in &cancelled {
                        if let Err(e) = executor.cancel(order_id).await {
                            warn!(order_id = %order_id, error = %e, "Exchange cancel failed for expired order");
                        }
                    }
                }
            }

            // 2. Poll each whitelisted wallet for fresh trades (Data-API backfill
            //    + optional on-chain OrderFilled feed for lower latency).
            let active = coordinator_clone.whitelist().list_active_whitelist();
            let maker_addresses: Vec<_> = active
                .iter()
                .filter_map(|p| p.address.parse().ok())
                .collect();
            let mut signals = Vec::new();
            for profile in &active {
                match feed_clone.poll_wallet(&profile.address, current_time).await {
                    Ok(s) => signals.extend(s),
                    Err(e) => {
                        journal_for_loop.record(
                            "feed_poll_error",
                            serde_json::json!({
                                "wallet": profile.address,
                                "source": "data_api",
                                "error": e.to_string(),
                            }),
                        );
                        warn!(wallet = %profile.address, error = %e, "Trade poll failed");
                    }
                }
            }
            if let Some(feed) = &onchain_for_loop {
                match feed.poll_wallets(&maker_addresses, current_time).await {
                    Ok(mut s) => signals.append(&mut s),
                    Err(e) => warn!(error = %e, "On-chain feed poll failed"),
                }
            }
            if runtime_for_loop.shutdown_requested() {
                break;
            }
            let ctx = SignalContext {
                coordinator: coordinator_clone.clone(),
                meta_fetcher: meta_clone.clone(),
                book_fetcher: book_clone.clone(),
                market_cache: mcache.clone(),
                book_cache: bcache.clone(),
                dry_run,
                live_executor: executor_for_loop.clone(),
                ladder_tracker: ladder_for_loop.clone(),
                journal: journal_for_loop.clone(),
                processed_signals: processed_signals.clone(),
                config: signal_config.clone(),
                runtime: runtime_for_loop.clone(),
                quality: quality_for_loop.clone(),
            };
            for sig in signals {
                if runtime_for_loop.shutdown_requested() {
                    break;
                }
                handle_signal(&ctx, sig).await;
            }

            // 3. Refresh mark prices for open positions from live book mid
            refresh_mark_prices(&coordinator_clone, &book_clone, &bcache).await;

            // 3b. Settlement monitor: detect resolved markets, mark to final
            //     values, redeem winners on-chain (live) / realize (paper).
            let settled_markets = settle_monitor
                .check_and_settle(&coordinator_clone, executor_for_loop.as_deref(), dry_run)
                .await;
            for market_id in &settled_markets {
                journal_for_loop.record(
                    "settlement",
                    serde_json::json!({
                        "market": market_id,
                    }),
                );
            }

            // 3c. Laddered scale-out: finish partially-filled exits at descending
            //     prices (Opinion 5) so positions do not get stuck.
            let ladder_legs = ladder_for_loop
                .run(
                    &coordinator_clone,
                    &book_clone,
                    executor_for_loop.as_deref(),
                    dry_run,
                )
                .await;
            if ladder_legs > 0 {
                journal_for_loop.record(
                    "ladder_exit",
                    serde_json::json!({
                        "legsPlaced": ladder_legs,
                    }),
                );
            }

            // 4. Periodic status log + NAV snapshot for equity-curve analysis
            if loop_count.is_multiple_of(12) {
                let status_time = Utc::now();
                let risk_guard = coordinator_clone.risk().read().await;
                let nav = risk_guard.calculate_total_nav();
                let open = risk_guard.list_exposures().len();
                let active_whitelist = coordinator_clone.whitelist().list_active_whitelist();
                let cash = risk_guard.cash_balance();
                runtime_for_loop.set_whitelist_wallets(active_whitelist.len());
                runtime_for_loop.set_portfolio(nav, cash, open);
                journal_for_loop.record_nav_snapshot(nav, cash, open, active_whitelist.len());
                if let Err(error) = quality_for_loop.save() {
                    warn!(%error, "Copy-trading wallet quality state persistence failed");
                }
                info!(
                    "Status Update [{}]: NAV: ${:.2} | Open positions: {} | Whitelist: {} wallets | Mode: {}",
                    status_time.format("%Y-%m-%d %H:%M:%S UTC"),
                    nav,
                    open,
                    active_whitelist.len(),
                    if dry_run { "PAPER" } else { "LIVE" }
                );
            }

            // 5. Daily out-of-sample refresh: a once-good wallet must be able to
            // decay into probation without requiring a process restart.
            if loop_count > 0 && loop_count.is_multiple_of(wallet_reevaluation_loops) {
                let profiles = coordinator_clone.whitelist().list_active_whitelist();
                let mut historically_degraded = Vec::new();
                for profile in profiles {
                    match history_for_loop.fetch_records(&profile.address).await {
                        Ok(records) => {
                            let evaluation = coordinator_clone.whitelist().evaluate_wallet(
                                &profile.address,
                                &records,
                                Utc::now(),
                            );
                            let accepted = evaluation.is_accepted;
                            let tier = evaluation.tier;
                            coordinator_clone
                                .whitelist()
                                .upsert_wallet(evaluation.profile);
                            journal_for_loop.record(
                                "wallet_reevaluated",
                                serde_json::json!({
                                    "address": profile.address,
                                    "accepted": accepted,
                                    "tier": format!("{:?}", tier),
                                }),
                            );
                            if auto_rotation && !accepted {
                                historically_degraded.push(profile);
                            }
                        }
                        Err(e) => {
                            warn!(wallet = %profile.address, error = %e, "Periodic wallet reevaluation failed")
                        }
                    }
                }
                if auto_rotation && !historically_degraded.is_empty() {
                    rotate_wallets_from_report(
                        &historically_degraded,
                        &rotation_report_for_loop,
                        &coordinator_clone,
                        &history_for_loop,
                        &journal_for_loop,
                    )
                    .await;
                }
            }

            // 6. PAPER-only runtime source quality audit. A wallet is judged
            // only after enough observations, then moved to probation before
            // an independently-scanned candidate is enrolled.
            if loop_count > 0 && loop_count.is_multiple_of(quality_audit_loops) {
                let active_by_address = coordinator_clone
                    .whitelist()
                    .list_active_whitelist()
                    .into_iter()
                    .map(|profile| (profile.address.to_ascii_lowercase(), profile))
                    .collect::<HashMap<_, _>>();
                let mut degraded = Vec::new();
                for (address, sample) in quality_for_loop.snapshots() {
                    let reasons = quality_policy_for_loop.rejection_reasons(&sample);
                    journal_for_loop.record(
                        "wallet_quality_audit",
                        serde_json::json!({
                            "address": address,
                            "sample": sample,
                            "reasons": reasons,
                            "eligibleForRotation": !reasons.is_empty(),
                            "autoRotation": auto_rotation,
                        }),
                    );
                    if auto_rotation && !reasons.is_empty() {
                        if let Some(profile) = active_by_address.get(&address) {
                            let reason = format!("runtime quality: {}", reasons.join("; "));
                            if coordinator_clone
                                .whitelist()
                                .quarantine_wallet(&address, reason.clone())
                            {
                                journal_for_loop.record(
                                    "wallet_quarantined",
                                    serde_json::json!({
                                        "address": address,
                                        "domain": profile.primary_domain.to_string(),
                                        "reason": reason,
                                    }),
                                );
                                degraded.push(profile.clone());
                            }
                        }
                    }
                }
                for profile in coordinator_clone.whitelist().list_probation_wallets() {
                    if !degraded
                        .iter()
                        .any(|existing| existing.address.eq_ignore_ascii_case(&profile.address))
                    {
                        degraded.push(profile);
                    }
                }
                if auto_rotation && !degraded.is_empty() {
                    rotate_wallets_from_report(
                        &degraded,
                        &rotation_report_for_loop,
                        &coordinator_clone,
                        &history_for_loop,
                        &journal_for_loop,
                    )
                    .await;
                }
                if let Err(error) = quality_for_loop.save() {
                    warn!(%error, "Copy-trading wallet quality state persistence failed");
                }
            }

            loop_count += 1;
            if runtime_for_loop.shutdown_requested() {
                break;
            }
            sleep(poll_interval).await;
        }
    });

    runner_handle.await?;
    if let Err(error) = quality_tracker.save() {
        warn!(%error, "Final copy-trading wallet quality state persistence failed");
    }
    runtime.set_healthy(false);
    info!("Copy-trading system safely terminated.");
    Ok(())
}

async fn rotate_wallets_from_report(
    degraded: &[WalletProfile],
    report_path: &str,
    coordinator: &Arc<CopyTradingCoordinator>,
    history: &Arc<WalletHistoryFetcher>,
    journal: &CopyTradeJournal,
) {
    let report = match std::fs::read(report_path)
        .ok()
        .and_then(|raw| serde_json::from_slice::<ScoutReport>(&raw).ok())
    {
        Some(report) => report,
        None => {
            journal.record(
                "wallet_rotation_unavailable",
                serde_json::json!({
                    "report": report_path,
                    "reason": "candidate report missing or invalid",
                }),
            );
            warn!(report = %report_path, "Wallet rotation candidate report missing or invalid");
            return;
        }
    };
    let max_report_age_hours = env::var("COPY_ROTATION_MAX_REPORT_AGE_HOURS")
        .ok()
        .and_then(|value| value.trim().parse::<i64>().ok())
        .unwrap_or(24)
        .max(1);
    let report_age_hours = Utc::now()
        .signed_duration_since(report.generated_at)
        .num_hours()
        .max(0);
    if !report.partial_failures.is_empty() || report_age_hours > max_report_age_hours {
        journal.record(
            "wallet_rotation_unavailable",
            serde_json::json!({
                "report": report_path,
                "reason": if !report.partial_failures.is_empty() { "candidate scan has partial failures" } else { "candidate report is stale" },
                "reportAgeHours": report_age_hours,
                "maxReportAgeHours": max_report_age_hours,
                "partialFailures": report.partial_failures,
            }),
        );
        warn!(
            report = %report_path,
            report_age_hours,
            partial_failures = report.partial_failures.len(),
            "Wallet rotation candidate report failed freshness/completeness checks"
        );
        return;
    }
    let mut candidates = report
        .candidates
        .into_iter()
        .filter(|candidate| candidate.accepted && candidate.runtime_compatible)
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        right
            .score
            .partial_cmp(&left.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut unavailable = coordinator
        .whitelist()
        .list_active_whitelist()
        .into_iter()
        .map(|profile| profile.address.to_ascii_lowercase())
        .collect::<std::collections::HashSet<_>>();
    unavailable.extend(
        degraded
            .iter()
            .map(|profile| profile.address.to_ascii_lowercase()),
    );

    for old in degraded {
        let mut replacement = None;
        for candidate in &candidates {
            let key = candidate.address.to_ascii_lowercase();
            if unavailable.contains(&key) {
                continue;
            }
            unavailable.insert(key.clone());
            let records = match history.fetch_records(&candidate.address).await {
                Ok(records) => records,
                Err(error) => {
                    journal.record(
                        "wallet_rotation_candidate_rejected",
                        serde_json::json!({
                            "address": candidate.address,
                            "replaces": old.address,
                            "reason": format!("history fetch failed: {error}"),
                        }),
                    );
                    continue;
                }
            };
            let evaluation =
                coordinator
                    .whitelist()
                    .evaluate_wallet(&candidate.address, &records, Utc::now());
            let covers_domain = evaluation.profile.primary_domain == old.primary_domain
                || evaluation
                    .profile
                    .domain_shares
                    .iter()
                    .any(|(domain, share)| *domain == old.primary_domain && *share >= dec!(0.30));
            if !evaluation.is_accepted || !covers_domain {
                journal.record(
                    "wallet_rotation_candidate_rejected",
                    serde_json::json!({
                        "address": candidate.address,
                        "replaces": old.address,
                        "requiredDomain": old.primary_domain.to_string(),
                        "candidateDomain": evaluation.profile.primary_domain.to_string(),
                        "runtimeAccepted": evaluation.is_accepted,
                        "failureReasons": evaluation.failure_reasons,
                    }),
                );
                continue;
            }
            replacement = Some(evaluation.profile);
            break;
        }
        if let Some(profile) = replacement {
            let replacement_address = profile.address.clone();
            let replacement_tier = profile.tier;
            coordinator.whitelist().upsert_wallet(profile);
            coordinator.whitelist().remove_wallet(&old.address);
            journal.record(
                "wallet_rotated",
                serde_json::json!({
                    "removed": old.address,
                    "replacement": replacement_address,
                    "domain": old.primary_domain.to_string(),
                    "tier": format!("{replacement_tier:?}"),
                    "candidateReport": report_path,
                }),
            );
            info!(
                removed = %old.address,
                replacement = %replacement_address,
                domain = %old.primary_domain,
                "Copy-trading wallet source rotated"
            );
        } else {
            journal.record(
                "wallet_rotation_no_candidate",
                serde_json::json!({
                    "removed": old.address,
                    "domain": old.primary_domain.to_string(),
                    "candidateReport": report_path,
                }),
            );
            warn!(
                removed = %old.address,
                domain = %old.primary_domain,
                "No independently-scanned replacement wallet was available"
            );
        }
    }
}

/// Shared execution context for processing a single live signal.
struct SignalContext {
    coordinator: Arc<CopyTradingCoordinator>,
    meta_fetcher: Arc<MarketMetadataFetcher>,
    book_fetcher: Arc<BookFetcher>,
    market_cache: MarketCache,
    book_cache: BookCache,
    dry_run: bool,
    live_executor: Option<Arc<CopyTradeExecutor>>,
    ladder_tracker: LadderExitTracker,
    journal: CopyTradeJournal,
    processed_signals: ProcessedSignalStore,
    config: CopyTradingConfig,
    runtime: CopyTradingRuntime,
    quality: WalletQualityTracker,
}

/// Routes a single live signal through the full coordinator pipeline and
/// executes the resulting proposal: paper fill in dry-run, real CLOB order in
/// live mode.
async fn handle_signal(ctx: &SignalContext, mut signal: SignalEvent) {
    let now = Utc::now();
    ctx.runtime
        .record_signal(signal.timestamp.timestamp_millis());
    if signal.action == SignalAction::OpenBuy && !ctx.runtime.entry_enabled() {
        let dedup_key = durable_signal_key(&signal);
        if ctx.processed_signals.check_and_record(&dedup_key) {
            ctx.journal.record(
                "signal_entry_paused",
                serde_json::json!({
                    "signalId": signal.signal_id,
                    "wallet": signal.source_wallet,
                    "market": signal.market_id,
                }),
            );
        }
        ctx.runtime.record_rejection();
        return;
    }
    if signal.action == SignalAction::OpenBuy {
        let age_seconds = now.signed_duration_since(signal.timestamp).num_seconds();
        ctx.quality.record_buy(
            &signal.source_wallet,
            (0..=ctx.config.max_signal_age_seconds).contains(&age_seconds),
        );
    }

    // Reject observations that can never influence the strategy before making
    // any Gamma/CLOB request. Stale observations are still recorded in the
    // durable idempotency store so restarts cannot replay the same backlog.
    if let Some(reason) = signal_prefilter_reason(&signal, now, &ctx.config) {
        ctx.runtime.record_rejection();
        let dedup_key = durable_signal_key(&signal);
        if ctx.processed_signals.check_and_record(&dedup_key) {
            let age_seconds = now.signed_duration_since(signal.timestamp).num_seconds();
            ctx.journal.record(
                "signal_prefilter_rejected",
                serde_json::json!({
                    "signalId": signal.signal_id,
                    "wallet": signal.source_wallet,
                    "action": format!("{:?}", signal.action),
                    "market": signal.market_id,
                    "token": signal.token_id,
                    "reason": reason,
                    "ageSeconds": age_seconds,
                    "vwap": signal.target_vwap.to_string(),
                    "shares": signal.target_shares.to_string(),
                    "notional": signal.target_notional_usdc.to_string(),
                }),
            );
            debug!(signal = %signal.signal_id, reason, "Signal ignored by preflight filter");
            if ctx.dry_run
                && (reason == "below-minimum-notional" || reason == "execution-stale-shadow-only")
                && signal.action == SignalAction::OpenBuy
                && signal.target_notional_usdc >= ctx.config.shadow_min_signal_notional_usdc
            {
                record_signal_shadow(ctx, &signal, age_seconds, reason).await;
            }
        }
        return;
    }
    if signal.action == SignalAction::OpenBuy {
        ctx.quality.record_actionable(&signal.source_wallet);
    }

    // Market metadata is only required for buy-side signals (exit routing skips
    // the market filter). Fetch it lazily with a per-condition cache.
    let market: Option<MarketMetadata> = if signal.action == SignalAction::OpenBuy {
        match fetch_cached_market(&ctx.meta_fetcher, &ctx.market_cache, &signal.market_id).await {
            Ok(m) => m,
            Err(e) => {
                ctx.journal.record(
                    "signal_upstream_error",
                    serde_json::json!({
                        "signalId": signal.signal_id,
                        "wallet": signal.source_wallet,
                        "market": signal.market_id,
                        "stage": "market_metadata",
                        "error": e.to_string(),
                    }),
                );
                warn!(market = %signal.market_id, error = %e, "Market metadata fetch failed; skipping signal");
                return;
            }
        }
    } else {
        None
    };

    // Order book is required for both sides (pricing + depth protection).
    let book = match fetch_cached_book(
        &ctx.book_fetcher,
        &ctx.book_cache,
        &signal.token_id,
        &signal.market_id,
    )
    .await
    {
        Ok(b) => b,
        Err(e) => {
            ctx.runtime.record_rejection();
            ctx.journal.record(
                "signal_upstream_error",
                serde_json::json!({
                    "signalId": signal.signal_id,
                    "wallet": signal.source_wallet,
                    "market": signal.market_id,
                    "token": signal.token_id,
                    "stage": "order_book",
                    "error": e.to_string(),
                }),
            );
            warn!(token = %signal.token_id, error = %e, "Book fetch failed; skipping signal");
            return;
        }
    };

    // Verified on-chain transfer signals do not carry execution price. Enrich
    // them from a sane two-sided live book before applying normal gates.
    if signal.target_vwap <= Decimal::ZERO {
        let Some(mid) = book.mid_price() else {
            debug!(signal = %signal.signal_id, "On-chain signal lacks a usable two-sided book");
            return;
        };
        signal.target_vwap = mid;
        signal.target_notional_usdc = (signal.target_shares * mid).round_dp(2);
    }

    let dedup_key = durable_signal_key(&signal);
    if !ctx.processed_signals.check_and_record(&dedup_key) {
        debug!(signal = %signal.signal_id, "Duplicate signal ignored by durable idempotency store");
        return;
    }

    ctx.journal.record(
        "signal",
        serde_json::json!({
            "signalId": signal.signal_id,
            "wallet": signal.source_wallet,
            "action": format!("{:?}", signal.action),
            "market": signal.market_id,
            "token": signal.token_id,
            "outcome": signal.outcome.to_string(),
            "domain": signal.domain.to_string(),
            "shares": signal.target_shares.to_string(),
            "vwap": signal.target_vwap.to_string(),
            "notional": signal.target_notional_usdc.to_string(),
        }),
    );

    let market_ref = market.as_ref();
    let pipeline_result = ctx
        .coordinator
        .process_signal(signal.clone(), market_ref, &book, now)
        .await;
    if signal.action == SignalAction::OpenBuy {
        let market_passed = !matches!(
            &pipeline_result,
            Err(error) if error.starts_with("Market validation failed:")
        );
        ctx.quality
            .record_market(&signal.source_wallet, market_passed);
    }
    match pipeline_result {
        Ok(Some(prop)) => {
            ctx.runtime.record_proposal();
            if signal.action == SignalAction::OpenBuy {
                ctx.quality.record_proposal(&signal.source_wallet);
            }
            ctx.journal.record(
                "proposal",
                serde_json::json!({
                    "proposalId": prop.proposal_id,
                    "side": format!("{:?}", prop.side),
                    "market": prop.market_id,
                    "token": prop.token_id,
                    "outcome": prop.outcome.to_string(),
                    "limitPrice": prop.limit_price.to_string(),
                    "shares": prop.shares.to_string(),
                    "notional": prop.notional_usdc.to_string(),
                    "ttlSeconds": prop.ttl_seconds,
                }),
            );
            let order_id = format!("order-{}", prop.proposal_id);
            if ctx.dry_run {
                // Paper mode follows the same lifecycle as live: register the
                // proposal, wait for configured latency, re-read the uncached
                // live book, then walk only levels executable at the limit.
                ctx.coordinator
                    .on_order_placed(order_id.clone(), prop.clone(), now);
                if ctx.config.paper_execution_delay_ms > 0 {
                    sleep(Duration::from_millis(ctx.config.paper_execution_delay_ms)).await;
                }
                let execution_book = ctx
                    .book_fetcher
                    .fetch(&prop.token_id, &prop.market_id)
                    .await;
                let fill = match execution_book {
                    Ok(book) => simulate_paper_fill(&prop, &book),
                    Err(error) => {
                        warn!(
                            order_id = %order_id,
                            %error,
                            "PAPER execution book refresh failed; recording no fill"
                        );
                        PaperBookFill::default()
                    }
                };
                if fill.filled_shares > Decimal::ZERO {
                    ctx.runtime.record_fill();
                    ctx.coordinator
                        .on_order_fill(
                            &order_id,
                            &prop.market_id,
                            &prop.token_id,
                            fill.filled_shares,
                            fill.average_price,
                            Utc::now(),
                        )
                        .await;
                }
                ctx.coordinator
                    .on_order_finalized(Some(&order_id), &prop.proposal_id, Utc::now())
                    .await;
                ctx.journal.record(
                    "order",
                    serde_json::json!({
                        "orderId": order_id,
                        "side": format!("{:?}", prop.side),
                        "mode": "paper",
                        "status": if fill.filled_shares <= Decimal::ZERO { "no_fill" } else if fill.filled_shares < prop.shares { "partial" } else { "filled" },
                        "requested": prop.shares.to_string(),
                        "filled": fill.filled_shares.to_string(),
                        "avgPrice": fill.average_price.to_string(),
                        "cost": fill.notional.to_string(),
                        "delayMs": ctx.config.paper_execution_delay_ms,
                        "market": prop.market_id,
                    }),
                );
                info!(
                    "[PAPER] {} {} filled {}/{} shares @ {} = ${:.2} after {}ms | reason: {}",
                    side_label(&prop),
                    prop.market_id,
                    fill.filled_shares,
                    prop.shares,
                    fill.average_price,
                    fill.notional,
                    ctx.config.paper_execution_delay_ms,
                    prop.reason
                );
            } else {
                // Live mode: place a real CLOB order via the authenticated executor.
                let executor = match &ctx.live_executor {
                    Some(e) => e.as_ref(),
                    None => {
                        error!("Live mode requested but no executor available; skipping order");
                        return;
                    }
                };
                let outcome = if prop.side == OrderSide::Buy {
                    executor.execute_buy(&prop).await
                } else {
                    executor.execute_sell(&prop).await
                };

                match outcome {
                    Ok(outcome) => {
                        ctx.coordinator.on_order_placed(
                            outcome.order_id.clone(),
                            prop.clone(),
                            now,
                        );
                        if outcome.filled_shares > Decimal::ZERO {
                            ctx.runtime.record_fill();
                            ctx.coordinator
                                .on_order_fill(
                                    &outcome.order_id,
                                    &prop.market_id,
                                    &prop.token_id,
                                    outcome.filled_shares,
                                    outcome.avg_price,
                                    now,
                                )
                                .await;
                        }
                        ctx.coordinator
                            .on_order_finalized(
                                Some(&outcome.order_id),
                                &prop.proposal_id,
                                Utc::now(),
                            )
                            .await;
                        ctx.journal.record(
                            "order",
                            serde_json::json!({
                                "orderId": outcome.order_id,
                                "side": format!("{:?}", prop.side),
                                "mode": "live",
                                "filled": outcome.filled_shares.to_string(),
                                "avgPrice": outcome.avg_price.to_string(),
                                "market": prop.market_id,
                                "status": outcome.status,
                                "fullyFilled": outcome.fully_filled,
                            }),
                        );
                        info!(
                            "[LIVE] {} {} filled {} shares @ ${:.4} ({} {}) | reason: {}",
                            side_label(&prop),
                            prop.market_id,
                            outcome.filled_shares,
                            outcome.avg_price,
                            outcome.status,
                            if outcome.fully_filled {
                                "full"
                            } else {
                                "partial"
                            },
                            prop.reason
                        );
                        // Partial/incomplete SELL: arm the ladder so the position
                        // is cleared at descending prices over later cycles.
                        if prop.side == OrderSide::Sell {
                            let remaining = ctx
                                .coordinator
                                .risk()
                                .read()
                                .await
                                .get_market_exposure(&prop.market_id, &prop.outcome)
                                .map(|e| e.shares)
                                .unwrap_or_default();
                            if remaining > Decimal::ZERO {
                                ctx.ladder_tracker.record_exit_intent(
                                    &prop.market_id,
                                    &prop.outcome,
                                    &prop.token_id,
                                    remaining,
                                    outcome.avg_price,
                                );
                            }
                        }
                    }
                    Err(e) => {
                        ctx.coordinator
                            .on_order_finalized(None, &prop.proposal_id, Utc::now())
                            .await;
                        warn!(
                            order_id = %order_id,
                            error = %e,
                            "LIVE order execution failed; proposal not filled"
                        );
                    }
                }
            }
        }
        Ok(None) => {
            ctx.journal.record(
                if signal.action == SignalAction::OpenBuy {
                    "consensus_pending"
                } else {
                    "exit_ignored"
                },
                serde_json::json!({
                    "signalId": signal.signal_id,
                    "wallet": signal.source_wallet,
                    "market": signal.market_id,
                    "token": signal.token_id,
                    "action": format!("{:?}", signal.action),
                }),
            );
        }
        Err(e) => {
            ctx.runtime.record_rejection();
            ctx.journal.record(
                "signal_rejected",
                serde_json::json!({
                    "signalId": signal.signal_id,
                    "wallet": signal.source_wallet,
                    "market": signal.market_id,
                    "token": signal.token_id,
                    "action": format!("{:?}", signal.action),
                    "stage": if e.starts_with("Market validation failed:") { "market_validation" } else if e.starts_with("Execution pricing rejected:") { "execution_pricing" } else if e.starts_with("Risk allocation rejected:") || e.starts_with("Risk reservation failed") { "risk" } else { "pipeline" },
                    "reason": e,
                }),
            );
            debug!(signal = %signal.signal_id, error = %e, "Signal rejected by strategy pipeline");
        }
    }
}

async fn record_signal_shadow(
    ctx: &SignalContext,
    signal: &SignalEvent,
    age_seconds: i64,
    rejection_stage: &str,
) {
    if ctx.config.paper_execution_delay_ms > 0 {
        sleep(Duration::from_millis(ctx.config.paper_execution_delay_ms)).await;
    }
    let book = match ctx
        .book_fetcher
        .fetch(&signal.token_id, &signal.market_id)
        .await
    {
        Ok(book) => book,
        Err(error) => {
            ctx.quality.record_shadow(&signal.source_wallet, false);
            ctx.journal.record(
                "signal_shadow",
                serde_json::json!({
                    "signalId": signal.signal_id,
                    "wallet": signal.source_wallet,
                    "market": signal.market_id,
                    "stage": rejection_stage,
                    "outcome": "book_error",
                    "ageSeconds": age_seconds,
                    "delayMs": ctx.config.paper_execution_delay_ms,
                    "error": error.to_string(),
                }),
            );
            return;
        }
    };
    let nav = ctx.coordinator.risk().read().await.calculate_total_nav();
    let production_budget = nav * ctx.config.single_trade_allocation_pct;
    let consensus = polypulse::copy_trading::ConsensusSignal {
        consensus_id: format!("shadow-consensus-{}", signal.signal_id),
        market_id: signal.market_id.clone(),
        token_id: signal.token_id.clone(),
        outcome: signal.outcome.clone(),
        domain: signal.domain.clone(),
        contributing_wallets: vec![signal.source_wallet.clone()],
        consensus_score: Decimal::ONE,
        target_vwap: signal.target_vwap,
        created_at: Utc::now(),
    };
    let proposal = match ctx.coordinator.execution().build_buy_proposal(
        &consensus,
        &book,
        production_budget,
        Utc::now(),
    ) {
        Ok(proposal) => proposal,
        Err(error) => {
            ctx.quality.record_shadow(&signal.source_wallet, false);
            ctx.journal.record(
                "signal_shadow",
                serde_json::json!({
                    "signalId": signal.signal_id,
                    "wallet": signal.source_wallet,
                    "market": signal.market_id,
                    "token": signal.token_id,
                    "stage": rejection_stage,
                    "outcome": "execution_rejected",
                    "ageSeconds": age_seconds,
                    "delayMs": ctx.config.paper_execution_delay_ms,
                    "sourceNotional": signal.target_notional_usdc.to_string(),
                    "productionBudget": production_budget.to_string(),
                    "error": error.to_string(),
                }),
            );
            return;
        }
    };
    let fill = simulate_paper_fill(&proposal, &book);
    ctx.quality
        .record_shadow(&signal.source_wallet, fill.filled_shares > Decimal::ZERO);
    ctx.journal.record(
        "signal_shadow",
        serde_json::json!({
            "signalId": signal.signal_id,
            "wallet": signal.source_wallet,
            "market": signal.market_id,
            "token": signal.token_id,
            "stage": rejection_stage,
            "outcome": if fill.filled_shares <= Decimal::ZERO { "no_fill" } else if fill.filled_shares < proposal.shares { "partial" } else { "filled" },
            "ageSeconds": age_seconds,
            "delayMs": ctx.config.paper_execution_delay_ms,
            "targetVwap": signal.target_vwap.to_string(),
            "bestAsk": book.best_ask().map(|v| v.to_string()),
            "limitPrice": proposal.limit_price.to_string(),
            "sourceShares": signal.target_shares.to_string(),
            "sourceNotional": signal.target_notional_usdc.to_string(),
            "productionBudget": production_budget.to_string(),
            "requested": proposal.shares.to_string(),
            "requestedNotional": proposal.notional_usdc.to_string(),
            "filled": fill.filled_shares.to_string(),
            "averagePrice": fill.average_price.to_string(),
            "cost": fill.notional.to_string(),
            "priceDrift": (fill.average_price - signal.target_vwap).to_string(),
        }),
    );
}

fn durable_signal_key(signal: &SignalEvent) -> String {
    let mut parts = signal.signal_id.split('-');
    let prefix = parts.next().unwrap_or_default();
    let tx = parts.next().unwrap_or_default();
    if matches!(prefix, "sig" | "tx") && tx.len() >= 32 {
        format!(
            "tx:{}:{}:{:?}",
            tx.to_ascii_lowercase(),
            signal.token_id,
            signal.action
        )
    } else {
        signal.signal_id.clone()
    }
}

fn side_label(prop: &ExecutionOrderProposal) -> &'static str {
    match prop.side {
        polypulse::copy_trading::OrderSide::Buy => "BUY",
        polypulse::copy_trading::OrderSide::Sell => "SELL",
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct PaperBookFill {
    filled_shares: Decimal,
    notional: Decimal,
    average_price: Decimal,
}

/// Walks an already refreshed CLOB snapshot using live limit-order semantics.
/// BUY consumes asks no higher than the limit; SELL consumes bids no lower
/// than the limit. It deliberately permits partial/no fills instead of
/// crediting the complete proposal at its limit price.
fn simulate_paper_fill(
    proposal: &ExecutionOrderProposal,
    book: &OrderBookSnapshot,
) -> PaperBookFill {
    let levels = if proposal.side == OrderSide::Buy {
        &book.asks
    } else {
        &book.bids
    };
    let mut remaining = proposal.shares.max(Decimal::ZERO);
    let mut filled = Decimal::ZERO;
    let mut notional = Decimal::ZERO;
    for level in levels {
        let price_allowed = if proposal.side == OrderSide::Buy {
            level.price <= proposal.limit_price
        } else {
            level.price >= proposal.limit_price
        };
        if !price_allowed {
            break;
        }
        let take = remaining.min(level.size.max(Decimal::ZERO));
        filled += take;
        notional += take * level.price;
        remaining -= take;
        if remaining <= Decimal::ZERO {
            break;
        }
    }
    PaperBookFill {
        filled_shares: filled,
        notional,
        average_price: if filled > Decimal::ZERO {
            notional / filled
        } else {
            Decimal::ZERO
        },
    }
}

async fn fetch_cached_market(
    fetcher: &MarketMetadataFetcher,
    cache: &MarketCache,
    condition_id: &str,
) -> Result<Option<MarketMetadata>> {
    {
        let guard = cache.lock().unwrap();
        if let Some((at, meta)) = guard.get(condition_id) {
            if at.elapsed() < MARKET_CACHE_TTL {
                return Ok(meta.clone());
            }
        }
    }
    let meta = fetcher.fetch_by_condition(condition_id).await?;
    let mut guard = cache.lock().unwrap();
    guard.insert(condition_id.to_string(), (Instant::now(), meta.clone()));
    Ok(meta)
}

async fn fetch_cached_book(
    fetcher: &BookFetcher,
    cache: &BookCache,
    token_id: &str,
    market_id: &str,
) -> Result<OrderBookSnapshot> {
    {
        let guard = cache.lock().unwrap();
        if let Some((at, cached)) = guard.get(token_id) {
            match cached {
                CachedBook::Available(book) if at.elapsed() < BOOK_CACHE_TTL => {
                    return Ok(book.clone());
                }
                CachedBook::Unavailable(error) if at.elapsed() < BOOK_NEGATIVE_CACHE_TTL => {
                    return Err(anyhow::anyhow!("cached unavailable order book: {error}"));
                }
                _ => {}
            }
        }
    }
    match fetcher.fetch(token_id, market_id).await {
        Ok(book) => {
            let mut guard = cache.lock().unwrap();
            guard.insert(
                token_id.to_string(),
                (Instant::now(), CachedBook::Available(book.clone())),
            );
            Ok(book)
        }
        Err(error) => {
            let message = format!("{error:#}");
            let mut guard = cache.lock().unwrap();
            guard.insert(
                token_id.to_string(),
                (Instant::now(), CachedBook::Unavailable(message.clone())),
            );
            Err(anyhow::anyhow!(message))
        }
    }
}

/// Returns a reason when a signal can be discarded without consulting any
/// upstream market service. Exit signals deliberately bypass the opening-size
/// and entry-price gates because even a small sale may be a meaningful partial
/// reduction of an existing copied position.
fn signal_prefilter_reason(
    signal: &SignalEvent,
    now: chrono::DateTime<Utc>,
    config: &CopyTradingConfig,
) -> Option<&'static str> {
    let age = now.signed_duration_since(signal.timestamp).num_seconds();
    if age > config.shadow_max_signal_age_seconds {
        return Some("stale");
    }
    if age < -30 {
        return Some("future-dated");
    }
    if signal.action == SignalAction::OpenBuy {
        if age > config.max_signal_age_seconds {
            return Some("execution-stale-shadow-only");
        }
        if signal.target_notional_usdc < config.min_signal_notional_usdc {
            return Some("below-minimum-notional");
        }
        // A zero VWAP is valid only for the optional on-chain feed and must be
        // enriched from the live book before applying price sanity bounds.
        if signal.target_vwap > Decimal::ZERO
            && (signal.target_vwap < config.min_signal_vwap
                || signal.target_vwap > config.max_signal_vwap)
        {
            return Some("vwap-out-of-range");
        }
    }
    None
}

/// Mark-price sanity limits (Opinion 3): a distorted mark (thin book, wide
/// spread) must never feed the drawdown breaker or exposure caps. When the live
/// book fails the filters, the PREVIOUS valid mark is retained.
fn mark_filter_config() -> (Decimal, Decimal) {
    let max_spread = env::var("COPY_MARK_MAX_SPREAD_PCT")
        .ok()
        .and_then(|v| v.parse::<Decimal>().ok())
        .unwrap_or(dec!(0.05));
    let min_depth = env::var("COPY_MARK_MIN_DEPTH_USDC")
        .ok()
        .and_then(|v| v.parse::<Decimal>().ok())
        .unwrap_or(dec!(5.0));
    (max_spread, min_depth)
}

/// Refreshes unrealized P&L marks from live book mid prices so the daily
/// drawdown circuit breaker actually observes real market moves. Skips updates
/// when the book is too wide/thin to be trustworthy (retains previous mark).
async fn refresh_mark_prices(
    coordinator: &CopyTradingCoordinator,
    book_fetcher: &BookFetcher,
    book_cache: &BookCache,
) {
    let (max_spread, min_depth) = mark_filter_config();
    let exposures = coordinator.risk().read().await.list_exposures();
    for exp in exposures {
        if exp.shares <= Decimal::ZERO {
            continue;
        }
        let book = match fetch_cached_book(book_fetcher, book_cache, &exp.token_id, &exp.market_id)
            .await
        {
            Ok(b) => b,
            Err(e) => {
                debug!(token = %exp.token_id, error = %e, "Mark price refresh failed");
                continue;
            }
        };

        // Filter 1: relative spread sanity (e.g. bid 0.01 / ask 0.99 -> 98%).
        if let Some(rel) = book.relative_spread() {
            if rel > max_spread {
                debug!(
                    token = %exp.token_id,
                    spread_pct = %(rel * dec!(100)),
                    max_pct = %(max_spread * dec!(100)),
                    "Mark refresh skipped: orderbook spread too wide"
                );
                continue;
            }
        }
        // Filter 2: top-of-book notional depth must be meaningful.
        let top_depth = book
            .bids
            .first()
            .map(|l| l.price * l.size)
            .unwrap_or(Decimal::ZERO)
            + book
                .asks
                .first()
                .map(|l| l.price * l.size)
                .unwrap_or(Decimal::ZERO);
        if top_depth < min_depth {
            debug!(
                token = %exp.token_id,
                top_depth = %top_depth,
                min_depth = %min_depth,
                "Mark refresh skipped: orderbook too thin"
            );
            continue;
        }

        if let Some(mid) = book.mid_price() {
            coordinator
                .risk()
                .read()
                .await
                .update_mark_price(&exp.token_id, mid);
        }
    }
}

#[allow(dead_code)] // standalone binary only
fn standalone_ctrlc_handler(shutdown: Arc<AtomicBool>) {
    tokio::spawn(async move {
        if let Ok(()) = tokio::signal::ctrl_c().await {
            info!("Received shutdown signal (Ctrl+C). Initiating graceful wind-down...");
            shutdown.store(true, Ordering::Relaxed);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration as ChronoDuration;
    use polypulse::copy_trading::{BookLevel, OutcomeSide};

    fn signal(now: chrono::DateTime<Utc>, action: SignalAction) -> SignalEvent {
        SignalEvent {
            signal_id: "sig-test".to_string(),
            source_wallet: "0xabc".to_string(),
            market_id: "market".to_string(),
            token_id: "token".to_string(),
            outcome: OutcomeSide::Yes,
            domain: polypulse::copy_trading::DomainTag::General,
            action,
            target_vwap: dec!(0.50),
            target_shares: dec!(400),
            target_notional_usdc: dec!(200),
            target_reduction_pct: None,
            timestamp: now,
        }
    }

    #[test]
    fn prefilter_routes_execution_stale_signal_to_shadow_research() {
        let now = Utc::now();
        let mut sig = signal(now, SignalAction::OpenBuy);
        sig.timestamp = now - ChronoDuration::seconds(121);
        assert_eq!(
            signal_prefilter_reason(&sig, now, &CopyTradingConfig::default()),
            Some("execution-stale-shadow-only")
        );
        sig.timestamp = now - ChronoDuration::seconds(301);
        assert_eq!(
            signal_prefilter_reason(&sig, now, &CopyTradingConfig::default()),
            Some("stale")
        );
    }

    #[test]
    fn prefilter_rejects_dust_buy_but_preserves_small_exit() {
        let now = Utc::now();
        let mut buy = signal(now, SignalAction::OpenBuy);
        buy.target_notional_usdc = dec!(99.99);
        let mut sell = buy.clone();
        sell.action = SignalAction::CloseSell;
        assert_eq!(
            signal_prefilter_reason(&buy, now, &CopyTradingConfig::default()),
            Some("below-minimum-notional")
        );
        assert_eq!(
            signal_prefilter_reason(&sell, now, &CopyTradingConfig::default()),
            None
        );
    }

    #[test]
    fn prefilter_allows_zero_vwap_for_onchain_enrichment() {
        let now = Utc::now();
        let mut sig = signal(now, SignalAction::OpenBuy);
        sig.target_vwap = Decimal::ZERO;
        assert_eq!(
            signal_prefilter_reason(&sig, now, &CopyTradingConfig::default()),
            None
        );
    }

    fn proposal(side: OrderSide, limit_price: Decimal, shares: Decimal) -> ExecutionOrderProposal {
        ExecutionOrderProposal {
            proposal_id: "proposal".into(),
            market_id: "market".into(),
            token_id: "token".into(),
            outcome: OutcomeSide::Yes,
            side,
            limit_price,
            shares,
            notional_usdc: shares * limit_price,
            ttl_seconds: 45,
            reason: "test".into(),
            created_at: Utc::now(),
        }
    }

    fn book() -> OrderBookSnapshot {
        OrderBookSnapshot {
            market_id: "market".into(),
            token_id: "token".into(),
            bids: vec![
                BookLevel {
                    price: dec!(0.49),
                    size: dec!(3),
                },
                BookLevel {
                    price: dec!(0.48),
                    size: dec!(4),
                },
            ],
            asks: vec![
                BookLevel {
                    price: dec!(0.51),
                    size: dec!(2),
                },
                BookLevel {
                    price: dec!(0.52),
                    size: dec!(3),
                },
                BookLevel {
                    price: dec!(0.53),
                    size: dec!(10),
                },
            ],
            timestamp: Utc::now(),
        }
    }

    #[test]
    fn paper_buy_walks_only_asks_inside_limit_and_can_partially_fill() {
        let fill = simulate_paper_fill(&proposal(OrderSide::Buy, dec!(0.52), dec!(8)), &book());
        assert_eq!(fill.filled_shares, dec!(5));
        assert_eq!(fill.notional, dec!(2.58));
        assert_eq!(fill.average_price, dec!(0.516));
    }

    #[test]
    fn paper_sell_walks_only_bids_inside_limit() {
        let fill = simulate_paper_fill(&proposal(OrderSide::Sell, dec!(0.485), dec!(5)), &book());
        assert_eq!(fill.filled_shares, dec!(3));
        assert_eq!(fill.notional, dec!(1.47));
        assert_eq!(fill.average_price, dec!(0.49));
    }
}
