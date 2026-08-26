//! Deterministic dry-run simulator for the copy-trading strategy.
//!
//! Unlike the live `copy_trader` binary (which waits for real wallet trades),
//! this harness drives REAL market data through the FULL strategy pipeline with
//! controlled signals, so the paper-trading loop can be observed on demand:
//!
//!   1. Fetches a REAL eligible market (Gamma API) + its REAL order book (CLOB).
//!   2. Enrolls two whitelisted TierA wallets (Macro domain) — the whitelist
//!      evaluation itself is unit-tested elsewhere; here they act as signal
//!      sources only.
//!   3. Sends two BUY signals (consensus) -> proposal -> PAPER fill -> position.
//!   4. Refreshes the position mark price from the live book mid -> NAV moves.
//!   5. Sends a SELL signal -> exit mirror -> PAPER fill -> position reduced.
//!
//! The market default (September 2026 Fed decision) passes every default Module
//! 2 filter: binary, ~24d to expiry, ~$208k 24h volume, ~1.5% spread.
//!
//! Usage: `cargo run --bin copy_trader_sim [CONDITION_ID]`

use anyhow::{Context, Result};
use chrono::{Duration, Utc};
use dotenvy::dotenv;
use polypulse::copy_trading::{
    BookFetcher, CopyTradingConfig, CopyTradingCoordinator, DomainTag, MarketMetadata,
    MarketMetadataFetcher, OutcomeSide, SignalAction, SignalEvent, WalletTradeRecord,
};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::env;
use std::sync::Arc;
use tokio::time::{sleep, Duration as TokioDuration};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

const DEFAULT_CONDITION_ID: &str =
    "0xa3b36b2d6104d34af4e6c6215fc818e43352e78a748fbfb0b85e3a35f71dec9a";

#[tokio::main]
async fn main() -> Result<()> {
    dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,polypulse=debug")),
        )
        .init();

    let condition_id = env::args()
        .nth(1)
        .unwrap_or_else(|| DEFAULT_CONDITION_ID.to_string());

    info!("=================================================================");
    info!("   Copy-Trading Dry-Run Simulator (real market + real book)      ");
    info!("   Condition: {}", condition_id);
    info!("=================================================================");

    let config = CopyTradingConfig::from_env();
    let now = Utc::now();
    let coordinator = Arc::new(CopyTradingCoordinator::new(
        config.clone(),
        dec!(10_000.0),
        now,
    ));

    // 1. Enroll two whitelisted TierA wallets specialized in the Macro domain.
    //    (Signal-source doubles; whitelist evaluation itself is unit-tested.)
    let wallet_a = "0xSimTraderA00000000000000000000000000000001";
    let wallet_b = "0xSimTraderB00000000000000000000000000000002";
    enroll_macro_wallet(&coordinator, wallet_a, now)?;
    enroll_macro_wallet(&coordinator, wallet_b, now)?;
    info!(
        "Whitelisted sim wallets: {} (TierA), {} (TierA)",
        wallet_a, wallet_b
    );

    // 2. Real market metadata via Gamma.
    let meta_fetcher = MarketMetadataFetcher::new()?;
    let market = meta_fetcher
        .fetch_by_condition(&condition_id)
        .await?
        .with_context(|| format!("Gamma returned no market for {}", condition_id))?;
    info!(
        "Market: {} | end {} | vol24h ${} | active {}",
        market.question, market.settlement_time, market.volume_24h_usdc, market.is_active
    );

    // 3. Real order book via CLOB (need the YES/NO token ids from Gamma).
    let (yes_token, no_token) = fetch_market_tokens(&condition_id).await?;
    info!("Tokens: YES={} NO={}", yes_token, no_token);
    let book_fetcher = BookFetcher::new()?;
    let book = book_fetcher.fetch(&yes_token, &market.market_id).await?;
    let mid = book.mid_price().unwrap_or(dec!(0.50));
    info!(
        "Book: best_bid {} best_ask {} mid {} ({} bids, {} asks)",
        book.best_bid()
            .map(|d| d.to_string())
            .unwrap_or_else(|| "-".into()),
        book.best_ask()
            .map(|d| d.to_string())
            .unwrap_or_else(|| "-".into()),
        mid,
        book.bids.len(),
        book.asks.len()
    );

    // 4. Two BUY signals -> consensus -> proposal -> PAPER fill.
    let sig_a = buy_signal(wallet_a, &market, &yes_token, mid, now);
    let prop1 = coordinator
        .process_signal(sig_a.clone(), Some(&market), &book, now)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    info!(
        "Signal A processed: {}",
        if prop1.is_some() {
            "proposal generated"
        } else {
            "waiting for consensus partner"
        }
    );

    let sig_b = buy_signal(
        wallet_b,
        &market,
        &yes_token,
        mid,
        now + Duration::seconds(30),
    );
    let prop2 = coordinator
        .process_signal(
            sig_b.clone(),
            Some(&market),
            &book,
            now + Duration::seconds(30),
        )
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let proposal = prop2.context(
        "consensus buy produced no proposal — check market filter / risk / execution logs",
    )?;
    info!(
        "CONSENSUS BUY: {} shares @ ${} = ${:.2} (limit, TTL {}s)",
        proposal.shares, proposal.limit_price, proposal.notional_usdc, proposal.ttl_seconds
    );

    let order_id = format!("sim-buy-{}", proposal.proposal_id);
    coordinator.on_order_placed(order_id.clone(), proposal.clone(), now);
    coordinator
        .on_order_fill(
            &order_id,
            &proposal.market_id,
            &proposal.token_id,
            proposal.shares,
            proposal.limit_price,
            now,
        )
        .await;
    let risk = coordinator.risk().read().await;
    let exposure = risk
        .get_market_exposure(&market.market_id, &OutcomeSide::Yes)
        .expect("buy fill must create a YES position");
    let nav = risk.calculate_total_nav();
    info!(
        "PAPER POSITION: {} shares YES @ avg ${:.4} | NAV ${:.2} (cash {} + position ${:.2})",
        exposure.shares,
        exposure.avg_entry_price,
        nav,
        nav - exposure.notional_usdc,
        exposure.notional_usdc
    );
    drop(risk);

    // 5. Mark-price refresh from the live book: watch NAV react to real moves.
    info!("Refreshing mark prices from live book (5s)...");
    for i in 0..3 {
        sleep(TokioDuration::from_secs(5)).await;
        let book = book_fetcher.fetch(&yes_token, &market.market_id).await?;
        if let Some(new_mid) = book.mid_price() {
            coordinator
                .risk()
                .read()
                .await
                .update_mark_price(&yes_token, new_mid);
        }
        let risk = coordinator.risk().read().await;
        let nav = risk.calculate_total_nav();
        let exp = risk.get_market_exposure(&market.market_id, &OutcomeSide::Yes);
        info!(
            "mark[{}] NAV ${:.2} | position mark ${:.4}",
            i,
            nav,
            exp.map(|e| e.current_mark_price).unwrap_or_default()
        );
    }

    // 6. SELL signal from wallet A -> exit mirror -> PAPER fill.
    let sell_sig = SignalEvent {
        signal_id: "sim-sell-1".to_string(),
        source_wallet: wallet_a.to_string(),
        market_id: market.market_id.clone(),
        token_id: yes_token.clone(),
        outcome: OutcomeSide::Yes,
        domain: DomainTag::MacroInterestRate,
        action: SignalAction::CloseSell,
        target_vwap: mid,
        target_shares: dec!(500.0),
        target_notional_usdc: (mid * dec!(500.0)).round_dp(2),
        target_reduction_pct: Some(Decimal::ONE),
        timestamp: now + Duration::minutes(2),
    };
    let exit_prop = coordinator
        .process_signal(sell_sig, None, &book, now + Duration::minutes(2))
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?
        .context("exit mirror produced no proposal")?;
    info!(
        "EXIT MIRROR: sell {} shares @ ${} = ${:.2}",
        exit_prop.shares, exit_prop.limit_price, exit_prop.notional_usdc
    );
    let exit_order = format!("sim-sell-{}", exit_prop.proposal_id);
    coordinator.on_order_placed(exit_order.clone(), exit_prop.clone(), now);
    coordinator
        .on_order_fill(
            &exit_order,
            &exit_prop.market_id,
            &exit_prop.token_id,
            exit_prop.shares,
            exit_prop.limit_price,
            now,
        )
        .await;

    let risk = coordinator.risk().read().await;
    let remaining = risk.get_market_exposure(&market.market_id, &OutcomeSide::Yes);
    let nav = risk.calculate_total_nav();
    info!(
        "AFTER EXIT: remaining YES shares {} | NAV ${:.2}",
        remaining.map(|e| e.shares).unwrap_or_default(),
        nav
    );
    drop(risk);

    info!("Dry-run simulation complete.");
    Ok(())
}

fn buy_signal(
    wallet: &str,
    market: &MarketMetadata,
    token_id: &str,
    vwap: Decimal,
    ts: chrono::DateTime<Utc>,
) -> SignalEvent {
    SignalEvent {
        signal_id: format!("sim-buy-{}-{}", wallet, ts.timestamp()),
        source_wallet: wallet.to_string(),
        market_id: market.market_id.clone(),
        token_id: token_id.to_string(),
        outcome: OutcomeSide::Yes,
        domain: DomainTag::MacroInterestRate,
        action: SignalAction::OpenBuy,
        target_vwap: vwap,
        target_shares: dec!(500.0),
        target_notional_usdc: (vwap * dec!(500.0)).round_dp(2),
        target_reduction_pct: None,
        timestamp: ts,
    }
}

/// Enrolls a wallet as TierA in the Macro domain by evaluating generated records
/// (a deterministic test double; the funnel logic itself is unit-tested).
fn enroll_macro_wallet(
    coordinator: &CopyTradingCoordinator,
    addr: &str,
    now: chrono::DateTime<Utc>,
) -> Result<()> {
    let mut records = Vec::new();
    for i in 0..60 {
        let days_ago = 120 - (i as i64 * 2);
        let won = i % 3 != 0; // ~66% win rate
        let pnl = if won { dec!(20.0) } else { dec!(-20.0) };
        records.push(WalletTradeRecord {
            trade_id: format!("sim-{}-{}", addr, i),
            market_id: format!("sim-mkt-{}", i),
            domain: DomainTag::MacroInterestRate,
            outcome_bought: OutcomeSide::Yes,
            entry_price: dec!(0.50),
            exit_price: Some(if won { dec!(1.0) } else { dec!(0.0) }),
            notional_usdc: dec!(100.0),
            pnl_usdc: pnl,
            return_pct: pnl / dec!(100.0),
            is_maker: false,
            is_arbitrage_leg: false,
            behavior_data_known: true,
            resolved_outcome: if won {
                Some(OutcomeSide::Yes)
            } else {
                Some(OutcomeSide::No)
            },
            opened_at: now - Duration::days(days_ago),
            settled_at: Some(now - Duration::days(days_ago - 1)),
        });
    }
    let res = coordinator.whitelist().evaluate_wallet(addr, &records, now);
    if !res.is_accepted {
        warn!(
            "Sim wallet {} rejected by funnel: {:?}",
            addr, res.failure_reasons
        );
    }
    coordinator.whitelist().upsert_wallet(res.profile);
    Ok(())
}

/// Fetches the YES/NO outcome token ids for a market from raw Gamma JSON.
async fn fetch_market_tokens(condition_id: &str) -> Result<(String, String)> {
    let url = format!(
        "https://gamma-api.polymarket.com/markets?condition_ids={}&limit=1",
        condition_id
    );
    let resp: serde_json::Value = reqwest::Client::new()
        .get(&url)
        .send()
        .await
        .context("gamma request failed")?
        .error_for_status()
        .context("gamma non-2xx")?
        .json()
        .await
        .context("gamma parse failed")?;

    let tokens = resp
        .as_array()
        .and_then(|a| a.first())
        .and_then(|m| m.get("clobTokenIds"))
        .and_then(|t| {
            // Gamma serializes clobTokenIds as a JSON-encoded STRING in some
            // responses; accept both string and native array forms.
            match t {
                serde_json::Value::String(s) => serde_json::from_str::<Vec<String>>(s).ok(),
                serde_json::Value::Array(a) => a
                    .iter()
                    .map(|v| v.as_str().map(|s| s.to_string()))
                    .collect(),
                _ => None,
            }
        })
        .context("gamma response missing clobTokenIds")?;

    let yes = tokens.first().cloned().context("clobTokenIds[0] missing")?;
    let no = tokens.get(1).cloned().context("clobTokenIds[1] missing")?;
    Ok((yes, no))
}
