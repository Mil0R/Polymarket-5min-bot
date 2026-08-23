use anyhow::Result;
use chrono::Utc;
use dotenvy::dotenv;
use polypulse::copy_trading::{
    BookFetcher, CopyTradeExecutor, CopyTradeJournal, CopyTradingConfig, CopyTradingCoordinator,
    ExecutionOrderProposal, GammaSettlementSource, init_copy_trader_logging, LadderExitConfig,
    LadderExitTracker, MarketMetadata, MarketMetadataFetcher, OnChainTradeFeed, OrderBookSnapshot,
    OrderSide, SettlementMonitor, SignalAction, SignalEvent, TradeFeed, WalletHistoryFetcher,
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
const POLL_INTERVAL: Duration = Duration::from_secs(5);

type MarketCache = Arc<Mutex<HashMap<String, (Instant, Option<MarketMetadata>)>>>;
type BookCache = Arc<Mutex<HashMap<String, (Instant, OrderBookSnapshot)>>>;

#[tokio::main]
async fn main() -> Result<()> {
    // 1. Initialize environment & logging (stdout + rotating file, see
    //    LOG_FILE / LOG_ROTATE_MAX_BYTES / LOG_ROTATE_FILES).
    dotenv().ok();
    init_copy_trader_logging("logs/copy-trader.log")?;

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
    let configured_nav = env::var("COPY_INITIAL_NAV_USDC")
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
                warn!("Live USDC balance is zero/empty; using configured initial NAV ${:.2}", configured_nav);
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
    info!(" -> Execution Mode:            {}", if dry_run { "PAPER / DRY-RUN (Safe Simulation)" } else { "LIVE (real CLOB orders)" });

    // Structured results journal (NDJSON) for post-run regression analysis.
    let mode = if dry_run { "paper" } else { "live" };
    let run_id = uuid::Uuid::new_v4().to_string();
    let journal = CopyTradeJournal::new(run_id.clone());
    let results_dir = env::var("COPY_TRADING_RESULTS_DIR")
        .unwrap_or_else(|_| format!("results/copy-trading/{}", mode));
    let events_file = format!("{}/events.ndjson", results_dir);
    match journal.enable(&events_file) {
        Ok(()) => info!(run_id = %run_id, file = %events_file, "Copy-trading results journal enabled"),
        Err(e) => warn!(error = %e, "Results journal could not be enabled; continuing without it"),
    }
    journal.record_run_meta(mode, serde_json::json!({
        "initialNav": initial_nav.to_string(),
        "dryRun": dry_run,
        "targetWallets": env::var("COPY_TARGET_WALLETS").unwrap_or_default(),
    }));

    let now = Utc::now();
    // Whitelist persistence (Opinion 4): profiles + strike counters survive
    // restarts, enabling warm-start without re-fetching all Data-API history.
    let whitelist_state_file = env::var("COPY_WHITELIST_STATE_FILE")
        .unwrap_or_else(|_| "data/copy-whitelist.json".to_string());
    let coordinator = Arc::new(CopyTradingCoordinator::new_with_whitelist_state(
        config.clone(),
        initial_nav,
        now,
        whitelist_state_file,
    ));

    // 3. Live data infrastructure
    let feed = Arc::new(TradeFeed::new()?);
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
    let onchain_feed: Option<Arc<OnChainTradeFeed>> =
        if env::var("COPY_ONCHAIN_FEED_ENABLED")
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
                journal.record("wallet_warmstart", serde_json::json!({
                    "address": addr, "tier": format!("{:?}", fresh.tier),
                    "lastEvaluated": fresh.last_evaluated_at.to_rfc3339(),
                }));
                info!(
                    " -> Warm-started wallet {} (Tier: {:?}, last evaluated {})",
                    addr,
                    fresh.tier,
                    fresh.last_evaluated_at
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
                        info!(" -> Enrolled wallet {} (Tier: {:?}, settled: {})", addr, tier, settled);
                    } else {
                        journal.record("wallet_rejected", serde_json::json!({
                            "address": addr, "reasons": res.failure_reasons,
                        }));
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
    let settle_monitor = settlement_monitor.clone();
    let ladder_for_loop = ladder_tracker.clone();
    let journal_for_loop = journal.clone();
    let executor_for_loop = live_executor.clone();
    let onchain_for_loop = onchain_feed.clone();
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
                    Err(e) => warn!(wallet = %profile.address, error = %e, "Trade poll failed"),
                }
            }
            if let Some(feed) = &onchain_for_loop {
                match feed
                    .poll_wallets(&maker_addresses, current_time)
                    .await
                {
                    Ok(mut s) => signals.append(&mut s),
                    Err(e) => warn!(error = %e, "On-chain feed poll failed"),
                }
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
            };
            for sig in signals {
                handle_signal(&ctx, sig).await;
            }

            // 3. Refresh mark prices for open positions from live book mid
            refresh_mark_prices(&coordinator_clone, &book_clone, &bcache).await;

            // 3b. Settlement monitor: detect resolved markets, mark to final
            //     values, redeem winners on-chain (live) / realize (paper).
            let settled_markets = settle_monitor
                .check_and_settle(
                    &coordinator_clone,
                    executor_for_loop.as_deref(),
                    dry_run,
                )
                .await;
            for market_id in &settled_markets {
                journal_for_loop.record("settlement", serde_json::json!({
                    "market": market_id,
                }));
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
                journal_for_loop.record("ladder_exit", serde_json::json!({
                    "legsPlaced": ladder_legs,
                }));
            }

            // 4. Periodic status log + NAV snapshot for equity-curve analysis
            if loop_count.is_multiple_of(12) {
                let risk_guard = coordinator_clone.risk().read().await;
                let nav = risk_guard.calculate_total_nav();
                let open = risk_guard.list_exposures().len();
                let active_whitelist = coordinator_clone.whitelist().list_active_whitelist();
                let cash = risk_guard.cash_balance();
                journal_for_loop.record_nav_snapshot(nav, cash, open, active_whitelist.len());
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
    ladder_tracker: LadderExitTracker,
    journal: CopyTradeJournal,
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

    ctx.journal.record("signal", serde_json::json!({
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
    }));

    let market_ref = market.as_ref();
    match ctx.coordinator.process_signal(signal.clone(), market_ref, &book, now).await {
        Ok(Some(prop)) => {
            ctx.journal.record("proposal", serde_json::json!({
                "proposalId": prop.proposal_id,
                "side": format!("{:?}", prop.side),
                "market": prop.market_id,
                "token": prop.token_id,
                "outcome": prop.outcome.to_string(),
                "limitPrice": prop.limit_price.to_string(),
                "shares": prop.shares.to_string(),
                "notional": prop.notional_usdc.to_string(),
                "ttlSeconds": prop.ttl_seconds,
            }));
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
                ctx.journal.record("order", serde_json::json!({
                    "orderId": order_id,
                    "side": format!("{:?}", prop.side),
                    "mode": "paper",
                    "filled": prop.shares.to_string(),
                    "avgPrice": prop.limit_price.to_string(),
                    "market": prop.market_id,
                }));
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
                        ctx.journal.record("order", serde_json::json!({
                            "orderId": outcome.order_id,
                            "side": format!("{:?}", prop.side),
                            "mode": "live",
                            "filled": outcome.filled_shares.to_string(),
                            "avgPrice": outcome.avg_price.to_string(),
                            "market": prop.market_id,
                            "status": outcome.status,
                            "fullyFilled": outcome.fully_filled,
                        }));
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
            ctx.journal.record("signal_rejected", serde_json::json!({
                "signalId": signal.signal_id,
                "reason": e,
            }));
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
        let book = match fetch_cached_book(book_fetcher, book_cache, &exp.token_id, &exp.market_id).await {
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

fn ctrlc_handler(running: Arc<AtomicBool>) {
    tokio::spawn(async move {
        if let Ok(()) = tokio::signal::ctrl_c().await {
            info!("Received shutdown signal (Ctrl+C). Initiating graceful wind-down...");
            running.store(false, Ordering::Relaxed);
        }
    });
}
