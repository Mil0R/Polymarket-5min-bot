use anyhow::Result;
use chrono::Utc;
use dotenvy::dotenv;
use polypulse::copy_trading::{
    BookFetcher, CopyTradeExecutor, CopyTradingConfig, CopyTradingCoordinator, ExecutionOrderProposal,
    MarketMetadata, MarketMetadataFetcher, OrderBookSnapshot, OrderSide, SignalAction, SignalEvent,
    TradeFeed, WalletHistoryFetcher,
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
use tracing_subscriber::EnvFilter;

/// Cache TTLs for upstream API data (avoids hammering gamma/clob on every poll).
const MARKET_CACHE_TTL: StdDuration = StdDuration::from_secs(3600);
const BOOK_CACHE_TTL: StdDuration = StdDuration::from_secs(15);
const POLL_INTERVAL: Duration = Duration::from_secs(5);

type MarketCache = Arc<Mutex<HashMap<String, (Instant, Option<MarketMetadata>)>>>;
type BookCache = Arc<Mutex<HashMap<String, (Instant, OrderBookSnapshot)>>>;

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
    info!("   Data: data-api / gamma / clob public endpoints (live feed)   ");
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

    // Live execution engine (only constructed when actually going live).
    let live_executor: Option<Arc<CopyTradeExecutor>> = if dry_run {
        None
    } else {
        info!("Initializing live CLOB execution engine...");
        let executor = CopyTradeExecutor::new_from_env().await?;
        executor.verify_authentication().await?;
        match executor.usdc_balance().await {
            Ok(bal) => info!("Live CLOB execution engine authenticated. USDC balance: ${:.2}", bal),
            Err(e) => warn!(error = %e, "Could not read USDC balance (read-only check failed)"),
        }
        Some(Arc::new(executor))
    };

    info!("Configuration Loaded:");
    info!(" -> Initial NAV:               ${:.2} USDC", initial_nav);
    info!(" -> Execution Mode:            {}", if dry_run { "PAPER / DRY-RUN (Safe Simulation)" } else { "LIVE (real CLOB orders)" });

    let now = Utc::now();
    let coordinator = Arc::new(CopyTradingCoordinator::new(config.clone(), initial_nav, now));

    // 3. Live data infrastructure
    let feed = Arc::new(TradeFeed::new()?);
    let history = Arc::new(WalletHistoryFetcher::new()?);
    let meta_fetcher = Arc::new(MarketMetadataFetcher::new()?);
    let book_fetcher = Arc::new(BookFetcher::new()?);
    let market_cache: MarketCache = Arc::new(Mutex::new(HashMap::new()));
    let book_cache: BookCache = Arc::new(Mutex::new(HashMap::new()));

    // 4. Seed target wallets from env, or discover from the profit leaderboard
    let target_wallets: Vec<String> = match env::var("COPY_TARGET_WALLETS") {
        Ok(raw) => raw
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        Err(_) => {
            info!("No COPY_TARGET_WALLETS specified. Discovering candidates from profit leaderboard...");
            match history.discover_candidates(30).await {
                Ok(cands) => cands,
                Err(e) => {
                    warn!("Leaderboard discovery failed: {e:#}");
                    Vec::new()
                }
            }
        }
    };

    if target_wallets.is_empty() {
        warn!("No target wallets available; copy-trading will idle until COPY_TARGET_WALLETS is configured.");
    } else {
        info!("Profiling {} target wallets from real Polymarket history...", target_wallets.len());
        for addr in &target_wallets {
            match history.fetch_records(addr).await {
                Ok(records) => {
                    let res = coordinator.whitelist().evaluate_wallet(addr, &records, now);
                    if res.is_accepted {
                        let tier = res.tier;
                        let settled = res.profile.total_settled_events;
                        coordinator.whitelist().upsert_wallet(res.profile);
                        info!(" -> Enrolled wallet {} (Tier: {:?}, settled: {})", addr, tier, settled);
                    } else {
                        warn!(" -> Wallet {} rejected by funnel: {:?}", addr, res.failure_reasons);
                    }
                }
                Err(e) => warn!(" -> Failed to fetch history for {}: {e:#}", addr),
            }
        }
    }

    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    ctrlc_handler(r);

    info!("Strategy coordinator initialized. Polling live wallet trades every {}s...", POLL_INTERVAL.as_secs());

    let coordinator_clone = coordinator.clone();
    let feed_clone = feed.clone();
    let meta_clone = meta_fetcher.clone();
    let book_clone = book_fetcher.clone();
    let mcache = market_cache.clone();
    let bcache = book_cache.clone();
    let executor_for_loop = live_executor.clone();
    let runner_handle = tokio::spawn(async move {
        let mut loop_count: u64 = 0;
        while running.load(Ordering::Relaxed) {
            let current_time = Utc::now();

            // 1. Heartbeat: TTL cancellations, daily rollover, risk health
            let cancelled = coordinator_clone.tick(current_time).await;
            if !cancelled.is_empty() {
                info!("Heartbeat: Cancelled {} expired TTL orders", cancelled.len());
                // Live mode: also cancel resting exchange orders for GTC/GTD mode.
                if let Some(executor) = &executor_for_loop {
                    for order_id in &cancelled {
                        if let Err(e) = executor.cancel(order_id).await {
                            warn!(order_id = %order_id, error = %e, "Exchange cancel failed for expired order");
                        }
                    }
                }
            }

            // 2. Poll each whitelisted wallet for fresh trades
            let active = coordinator_clone.whitelist().list_active_whitelist();
            for profile in active {
                match feed_clone.poll_wallet(&profile.address, current_time).await {
                    Ok(signals) => {
                        let ctx = SignalContext {
                            coordinator: coordinator_clone.clone(),
                            meta_fetcher: meta_clone.clone(),
                            book_fetcher: book_clone.clone(),
                            market_cache: mcache.clone(),
                            book_cache: bcache.clone(),
                            dry_run,
                            live_executor: executor_for_loop.clone(),
                        };
                        for sig in signals {
                            handle_signal(&ctx, sig).await;
                        }
                    }
                    Err(e) => warn!(wallet = %profile.address, error = %e, "Trade poll failed"),
                }
            }

            // 3. Refresh mark prices for open positions from live book mid
            refresh_mark_prices(&coordinator_clone, &book_clone, &bcache).await;

            // 4. Periodic status log
            if loop_count.is_multiple_of(12) {
                let risk_guard = coordinator_clone.risk().read().await;
                let nav = risk_guard.calculate_total_nav();
                let open = risk_guard.list_exposures().len();
                let active_whitelist = coordinator_clone.whitelist().list_active_whitelist();
                info!(
                    "Status Update [{}]: NAV: ${:.2} | Open positions: {} | Whitelist: {} wallets | Mode: {}",
                    current_time.format("%Y-%m-%d %H:%M:%S UTC"),
                    nav,
                    open,
                    active_whitelist.len(),
                    if dry_run { "PAPER" } else { "LIVE" }
                );
            }

            loop_count += 1;
            sleep(POLL_INTERVAL).await;
        }
    });

    runner_handle.await?;
    info!("Copy-trading system safely terminated.");
    Ok(())
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
}

/// Routes a single live signal through the full coordinator pipeline and
/// executes the resulting proposal: paper fill in dry-run, real CLOB order in
/// live mode.
async fn handle_signal(ctx: &SignalContext, signal: SignalEvent) {
    let now = Utc::now();

    // Market metadata is only required for buy-side signals (exit routing skips
    // the market filter). Fetch it lazily with a per-condition cache.
    let market: Option<MarketMetadata> = if signal.action == SignalAction::OpenBuy {
        match fetch_cached_market(&ctx.meta_fetcher, &ctx.market_cache, &signal.market_id).await {
            Ok(m) => m,
            Err(e) => {
                warn!(market = %signal.market_id, error = %e, "Market metadata fetch failed; skipping signal");
                return;
            }
        }
    } else {
        None
    };

    // Order book is required for both sides (pricing + depth protection).
    let book = match fetch_cached_book(&ctx.book_fetcher, &ctx.book_cache, &signal.token_id, &signal.market_id).await {
        Ok(b) => b,
        Err(e) => {
            warn!(token = %signal.token_id, error = %e, "Book fetch failed; skipping signal");
            return;
        }
    };

    let market_ref = market.as_ref();
    match ctx.coordinator.process_signal(signal.clone(), market_ref, &book, now).await {
        Ok(Some(prop)) => {
            let order_id = format!("order-{}", prop.proposal_id);
            if ctx.dry_run {
                // Paper mode: register and fill at the proposal limit price.
                ctx.coordinator.on_order_placed(order_id.clone(), prop.clone(), now);
                ctx.coordinator
                    .on_order_fill(
                        &order_id,
                        &prop.market_id,
                        &prop.token_id,
                        prop.shares,
                        prop.limit_price,
                        now,
                    )
                    .await;
                info!(
                    "[PAPER] {} {} {} shares @ {} = ${:.2} | reason: {}",
                    side_label(&prop),
                    prop.market_id,
                    prop.shares,
                    prop.limit_price,
                    prop.notional_usdc,
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
                        ctx.coordinator.on_order_placed(outcome.order_id.clone(), prop.clone(), now);
                        if outcome.filled_shares > Decimal::ZERO {
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
                        info!(
                            "[LIVE] {} {} filled {} shares @ ${:.4} ({} {}) | reason: {}",
                            side_label(&prop),
                            prop.market_id,
                            outcome.filled_shares,
                            outcome.avg_price,
                            outcome.status,
                            if outcome.fully_filled { "full" } else { "partial" },
                            prop.reason
                        );
                    }
                    Err(e) => {
                        warn!(
                            order_id = %order_id,
                            error = %e,
                            "LIVE order execution failed; proposal not filled"
                        );
                    }
                }
            }
        }
        Ok(None) => {}
        Err(e) => {
            debug!(signal = %signal.signal_id, error = %e, "Signal rejected by strategy pipeline");
        }
    }
}

fn side_label(prop: &ExecutionOrderProposal) -> &'static str {
    match prop.side {
        polypulse::copy_trading::OrderSide::Buy => "BUY",
        polypulse::copy_trading::OrderSide::Sell => "SELL",
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
        if let Some((at, book)) = guard.get(token_id) {
            if at.elapsed() < BOOK_CACHE_TTL {
                return Ok(book.clone());
            }
        }
    }
    let book = fetcher.fetch(token_id, market_id).await?;
    let mut guard = cache.lock().unwrap();
    guard.insert(token_id.to_string(), (Instant::now(), book.clone()));
    Ok(book)
}

/// Refreshes unrealized P&L marks from live book mid prices so the daily
/// drawdown circuit breaker actually observes real market moves.
async fn refresh_mark_prices(
    coordinator: &CopyTradingCoordinator,
    book_fetcher: &BookFetcher,
    book_cache: &BookCache,
) {
    let exposures = coordinator.risk().read().await.list_exposures();
    for exp in exposures {
        if exp.shares <= Decimal::ZERO {
            continue;
        }
        match fetch_cached_book(book_fetcher, book_cache, &exp.token_id, &exp.market_id).await {
            Ok(book) => {
                if let Some(mid) = book.mid_price() {
                    coordinator
                        .risk()
                        .read()
                        .await
                        .update_mark_price(&exp.token_id, mid);
                }
            }
            Err(e) => debug!(token = %exp.token_id, error = %e, "Mark price refresh failed"),
        }
    }
}

fn ctrlc_handler(running: Arc<AtomicBool>) {
    tokio::spawn(async move {
        if let Ok(()) = tokio::signal::ctrl_c().await {
            info!("Received shutdown signal (Ctrl+C). Initiating graceful wind-down...");
            running.store(false, Ordering::Relaxed);
        }
    });
}
