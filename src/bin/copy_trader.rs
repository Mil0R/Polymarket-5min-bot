use anyhow::Result;
use chrono::Utc;
use dotenvy::dotenv;
use polypulse::copy_trading::{
    CopyTradingConfig, CopyTradingCoordinator, DomainTag, OutcomeSide, WalletTradeRecord,
};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::env;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::time::{sleep, Duration};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    // 1. Initialize environment & logging
    dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,polypulse=debug")),
        )
        .init();

    // 1b. Master switch
    let enabled = env::var("COPY_TRADING_ENABLED")
        .map(|v| v.trim().to_lowercase() != "false" && v.trim() != "0")
        .unwrap_or(true);
    if !enabled {
        info!("COPY_TRADING_ENABLED=false — copy-trading disabled, exiting.");
        return Ok(());
    }

    info!("=================================================================");
    info!("   Polymarket Multi-Factor Copy-Trading System (V2)             ");
    info!("   Strategy: 5-Stage Funnel + Strict Limit + Multi-Consensus    ");
    info!("=================================================================");

    // 2. Load Configuration from Environment Variables / Defaults
    let config = CopyTradingConfig::from_env();
    let initial_nav = env::var("COPY_INITIAL_NAV_USDC")
        .ok()
        .and_then(|v| v.trim().parse::<Decimal>().ok())
        .unwrap_or(dec!(10_000.0));

    let dry_run = env::var("COPY_DRY_RUN")
        .map(|v| v.trim().to_lowercase() == "true" || v.trim() == "1")
        .unwrap_or(true);

    info!("Configuration Loaded:");
    info!(" -> Initial NAV:               ${:.2} USDC", initial_nav);
    info!(" -> Execution Mode:            {}", if dry_run { "PAPER / DRY-RUN (Safe Simulation)" } else { "LIVE TRADING (Real Funds)" });
    info!(" -> Whitelist Filters:         Settled >= {}, Active >= {}d, Entry in [{:.2}, {:.2}], MaxSingleProfit <= {:.1}%",
        config.min_settled_events, config.min_active_days, config.min_avg_entry_price, config.max_avg_entry_price, config.max_single_profit_ratio * dec!(100));
    info!(" -> Market Admission:          Spread <= {:.1}%, 24h Vol >= ${:.0}, Expiry >= {:.0}h, Binary Only: {}",
        config.max_spread * dec!(100), config.min_24h_volume_usdc, config.min_hours_to_expiry, config.isolate_multi_outcome);
    info!(" -> Price & Execution:         Max Slippage <= {:.1}%, Price Cap Offset <= +${:.3}, Top-5 Depth Cap <= {:.1}%, TTL: {}s",
        config.max_slippage_pct * dec!(100), config.max_price_offset_usd, config.max_top5_depth_pct * dec!(100), config.default_ttl_seconds);
    info!(" -> Portfolio Risk:            Single Allocation: {:.1}%, Max Market Exposure: {:.1}%, Daily Loss Breaker: {:.1}% (24h freeze)",
        config.single_trade_allocation_pct * dec!(100), config.max_single_market_exposure_pct * dec!(100), config.daily_max_drawdown_pct * dec!(100));
    info!(" -> Consensus & Exit:          Window: {}h, Min Wallets: {}, Exit Trigger: {:.0}% reduction",
        config.consensus_window_hours, config.min_consensus_wallets, config.exit_trigger_reduction_pct * dec!(100));

    let now = Utc::now();
    let coordinator = Arc::new(CopyTradingCoordinator::new(config.clone(), initial_nav, now));

    // 3. Seed initial expert wallets from env or historical database
    if let Ok(seed_wallets_raw) = env::var("COPY_TARGET_WALLETS") {
        let addresses: Vec<&str> = seed_wallets_raw.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
        info!("Seeding {} target wallets from COPY_TARGET_WALLETS...", addresses.len());
        for addr in addresses {
            // Mock or query historical records for wallet profiling
            let mut demo_records = Vec::new();
            for i in 0..65 {
                let days_ago = 120 - (i as i64 * 2);
                let won = i % 3 != 0;
                let pnl = if won { dec!(25.0) } else { dec!(-20.0) };
                demo_records.push(WalletTradeRecord {
                    trade_id: format!("seed-t-{}", i),
                    market_id: format!("mkt-seed-{}", i),
                    domain: DomainTag::MacroInterestRate,
                    outcome_bought: OutcomeSide::Yes,
                    entry_price: dec!(0.50),
                    exit_price: Some(if won { dec!(1.0) } else { dec!(0.0) }),
                    notional_usdc: dec!(100.0),
                    pnl_usdc: pnl,
                    return_pct: pnl / dec!(100.0),
                    is_maker: false,
                    is_arbitrage_leg: false,
                    resolved_outcome: if won { Some(OutcomeSide::Yes) } else { Some(OutcomeSide::No) },
                    opened_at: now - chrono::Duration::days(days_ago),
                    settled_at: Some(now - chrono::Duration::days(days_ago - 1)),
                });
            }
            let eval = coordinator.whitelist().evaluate_wallet(addr, &demo_records, now);
            if eval.is_accepted {
                coordinator.whitelist().upsert_wallet(eval.profile);
                info!(" -> Successfully enrolled seed wallet: {} (Tier: {:?})", addr, eval.tier);
            } else {
                warn!(" -> Seed wallet {} rejected by funnel: {:?}", addr, eval.failure_reasons);
            }
        }
    } else {
        info!("No COPY_TARGET_WALLETS specified. Running in automatic discovery mode.");
    }

    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    ctrlc_handler(r);

    info!("Strategy coordinator initialized. Starting real-time event & heartbeat loop...");

    let coordinator_clone = coordinator.clone();
    let runner_handle = tokio::spawn(async move {
        let mut loop_count: u64 = 0;
        while running.load(Ordering::Relaxed) {
            let current_time = Utc::now();

            // Periodic heartbeat: check TTL cancellations, daily rollover, risk health
            let cancelled = coordinator_clone.tick(current_time).await;
            if !cancelled.is_empty() {
                info!("Heartbeat: Cancelled {} expired TTL orders", cancelled.len());
            }

            if loop_count % 30 == 0 {
                let risk_guard = coordinator_clone.risk().read().await;
                let nav = risk_guard.calculate_total_nav();
                let active_whitelist = coordinator_clone.whitelist().list_active_whitelist();
                info!(
                    "Status Update [{}]: Total NAV: ${:.2} USDC | Active Whitelist: {} wallets | Mode: {}",
                    current_time.format("%Y-%m-%d %H:%M:%S UTC"),
                    nav,
                    active_whitelist.len(),
                    if dry_run { "PAPER" } else { "LIVE" }
                );
            }

            loop_count += 1;
            sleep(Duration::from_secs(2)).await;
        }
    });

    runner_handle.await?;
    info!("Copy-trading system safely terminated.");
    Ok(())
}

fn ctrlc_handler(running: Arc<AtomicBool>) {
    tokio::spawn(async move {
        if let Ok(()) = tokio::signal::ctrl_c().await {
            info!("Received shutdown signal (Ctrl+C). Initiating graceful wind-down...");
            running.store(false, Ordering::Relaxed);
        }
    });
}
