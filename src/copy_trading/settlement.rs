//! Settlement monitor (review Opinion 2): detects market resolution and runs
//! the settlement closed-loop so the strategy never runs out of cash.
//!
//! Flow per open position:
//!   1. fetch settlement status from Gamma (`closed` flag + `outcomePrices`).
//!   2. on resolution: mark all positions of the market to final value
//!      (1.0 winner / 0.0 loser) via the coordinator.
//!   3. winning positions: on-chain redeem (live mode, retries next cycle on
//!      failure) or simulated redeem (paper mode) — proceeds credit cash.
//!   4. zero-value positions (losers + realized winners) are pruned.
//!
//! The Gamma source is behind a trait so the whole flow is unit-testable with
//! a fake source.

use crate::copy_trading::coordinator::CopyTradingCoordinator;
use crate::copy_trading::live_executor::CopyTradeExecutor;
use crate::copy_trading::types::OutcomeSide;
use anyhow::{Context, Result};
use dashmap::DashMap;
use futures::future::BoxFuture;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::sync::Arc;
use std::time::Instant;
use tracing::{debug, info, warn};

const SETTLE_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(30);
const GAMMA_API_URL: &str = "https://gamma-api.polymarket.com";

/// Minimal gamma settlement info. `outcome_prices` may arrive as a JSON-encoded
/// string (e.g. `"[\"0\",\"1\"]"`) or as a native array.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct SettlementInfo {
    #[serde(default)]
    pub closed: Option<bool>,
    #[serde(default)]
    pub outcome_prices: Option<serde_json::Value>,
}

/// Testable boundary for fetching settlement status.
pub trait SettlementSource: Send + Sync {
    fn fetch(&self, condition_id: &str) -> BoxFuture<'_, Result<Option<SettlementInfo>>>;
}

/// Real Gamma-API-backed source.
#[derive(Debug, Clone)]
pub struct GammaSettlementSource {
    http: reqwest::Client,
}

impl GammaSettlementSource {
    pub fn new() -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .user_agent("polypulse-copy-trader")
                .build()
                .context("failed to build http client")?,
        })
    }
}

impl SettlementSource for GammaSettlementSource {
    fn fetch(&self, condition_id: &str) -> BoxFuture<'_, Result<Option<SettlementInfo>>> {
        let url = format!(
            "{}/markets?condition_ids={}&limit=1",
            GAMMA_API_URL, condition_id
        );
        let http = self.http.clone();
        Box::pin(async move {
            let resp: serde_json::Value = http
                .get(&url)
                .send()
                .await
                .context("gamma settle fetch failed")?
                .error_for_status()
                .context("gamma settle non-2xx")?
                .json()
                .await
                .context("gamma settle parse failed")?;
            let first = resp.as_array().and_then(|a| a.first()).cloned();
            match first {
                Some(v) => Ok(Some(
                    serde_json::from_value(v).context("gamma settle shape mismatch")?,
                )),
                None => Ok(None),
            }
        })
    }
}

fn json_str_array_to_decimals(v: &serde_json::Value) -> Vec<Decimal> {
    let items: Vec<serde_json::Value> = match v {
        serde_json::Value::String(s) => serde_json::from_str(s).unwrap_or_default(),
        serde_json::Value::Array(a) => a.clone(),
        _ => return Vec::new(),
    };
    items
        .iter()
        .filter_map(|x| match x {
            serde_json::Value::String(s) => s.parse::<Decimal>().ok(),
            serde_json::Value::Number(n) => n.as_f64().and_then(Decimal::from_f64_retain),
            _ => None,
        })
        .collect()
}

/// Maps resolved outcome prices to the winning binary side (index 0 -> YES,
/// index 1 -> NO, consistent with the feed's binary convention). Returns `None`
/// when no side has settled at ~1.0.
pub fn determine_resolved_outcome(prices: &[Decimal]) -> Option<OutcomeSide> {
    let winner_idx = prices.iter().position(|p| *p >= dec!(0.99))?;
    match winner_idx {
        0 => Some(OutcomeSide::Yes),
        1 => Some(OutcomeSide::No),
        _ => None,
    }
}

/// Settlement monitor with a short-TTL cache so resolution is detected within
/// ~30s without hammering Gamma.
#[derive(Clone)]
pub struct SettlementMonitor {
    source: Arc<dyn SettlementSource>,
    cache: Arc<DashMap<String, (Instant, Option<SettlementInfo>)>>,
}

impl SettlementMonitor {
    pub fn new(source: Arc<dyn SettlementSource>) -> Self {
        Self {
            source,
            cache: Arc::new(DashMap::new()),
        }
    }

    /// Runs one settlement pass over all open positions.
    ///
    /// Returns a summary of settled markets for logging/telemetry.
    pub async fn check_and_settle(
        &self,
        coordinator: &CopyTradingCoordinator,
        live_executor: Option<&CopyTradeExecutor>,
        dry_run: bool,
    ) -> Vec<String> {
        let exposures = coordinator.risk().read().await.list_exposures();
        let mut settled_markets: Vec<String> = Vec::new();

        for exp in exposures {
            if exp.shares <= Decimal::ZERO || settled_markets.contains(&exp.market_id) {
                continue;
            }
            let info = match self.fetch_cached(&exp.market_id).await {
                Ok(Some(i)) => i,
                Ok(None) => continue,
                Err(e) => {
                    debug!(market = %exp.market_id, error = %e, "Settlement check failed");
                    continue;
                }
            };
            if !info.closed.unwrap_or(false) {
                continue;
            }

            let prices = info
                .outcome_prices
                .as_ref()
                .map(json_str_array_to_decimals)
                .unwrap_or_default();
            let winner = match determine_resolved_outcome(&prices) {
                Some(w) => w,
                None => {
                    warn!(market = %exp.market_id, prices = ?prices, "No winning outcome price found; skipping settlement");
                    continue;
                }
            };

            info!(
                market = %exp.market_id,
                winner = ?winner,
                prices = ?prices,
                "Market resolved; settling positions"
            );
            coordinator
                .on_market_settlement(&exp.market_id, &winner, &[])
                .await;

            // Realize winning positions: on-chain redeem in live mode (retry
            // next cycle on failure), simulated redeem in paper mode.
            let winners: Vec<_> = coordinator
                .risk()
                .read()
                .await
                .list_exposures()
                .into_iter()
                .filter(|e| {
                    e.market_id == exp.market_id && e.outcome == winner && e.shares > Decimal::ZERO
                })
                .collect();

            for w in winners {
                if dry_run {
                    coordinator
                        .risk()
                        .read()
                        .await
                        .realize_settled_position(&w.token_id, w.shares);
                    info!(
                        token = %w.token_id,
                        proceeds = %w.shares,
                        "[PAPER] settled position realized to cash"
                    );
                } else if let Some(executor) = live_executor {
                    match executor
                        .redeem_settled_position(
                            &w.market_id,
                            &w.outcome,
                            &w.token_id,
                            w.shares,
                            false,
                        )
                        .await
                    {
                        Ok(tx) => {
                            coordinator
                                .risk()
                                .read()
                                .await
                                .realize_settled_position(&w.token_id, w.shares);
                            info!(
                                token = %w.token_id,
                                proceeds = %w.shares,
                                tx = %tx,
                                "[LIVE] settled position redeemed and realized to cash"
                            );
                        }
                        Err(e) => {
                            warn!(
                                token = %w.token_id,
                                error = %e,
                                "Redeem failed; will retry next cycle"
                            );
                        }
                    }
                }
            }

            // Drop zero-value positions (settled losers + realized winners).
            coordinator.risk().read().await.prune_settled_positions();
            settled_markets.push(exp.market_id.clone());
        }

        settled_markets
    }

    async fn fetch_cached(&self, condition_id: &str) -> Result<Option<SettlementInfo>> {
        if let Some(entry) = self.cache.get(condition_id) {
            if entry.0.elapsed() < SETTLE_CACHE_TTL {
                return Ok(entry.1.clone());
            }
        }
        let info = self.source.fetch(condition_id).await?;
        self.cache
            .insert(condition_id.to_string(), (Instant::now(), info.clone()));
        Ok(info)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::copy_trading::types::{
        BookLevel, CopyTradingConfig, DomainTag, MarketMetadata, OrderBookSnapshot, OutcomeSide,
        SignalAction, SignalEvent, WalletTradeRecord,
    };
    use chrono::{Duration, Utc};
    use rust_decimal_macros::dec;
    use std::sync::Mutex;

    struct FakeSource {
        response: Mutex<Option<SettlementInfo>>,
    }

    impl SettlementSource for FakeSource {
        fn fetch(&self, _condition_id: &str) -> BoxFuture<'_, Result<Option<SettlementInfo>>> {
            let resp = self.response.lock().unwrap().clone();
            Box::pin(async move { Ok(resp) })
        }
    }

    fn seed_wallet(coordinator: &CopyTradingCoordinator, addr: &str, now: chrono::DateTime<Utc>) {
        let mut records = Vec::new();
        for i in 0..60 {
            let days_ago = 120 - (i as i64 * 2);
            records.push(WalletTradeRecord {
                trade_id: format!("{}-{}", addr, i),
                market_id: format!("mkt-{}", i),
                domain: DomainTag::MacroInterestRate,
                outcome_bought: OutcomeSide::Yes,
                entry_price: dec!(0.50),
                exit_price: Some(dec!(1.0)),
                notional_usdc: dec!(100.0),
                pnl_usdc: dec!(20.0),
                return_pct: dec!(0.2),
                is_maker: false,
                is_arbitrage_leg: false,
                resolved_outcome: Some(OutcomeSide::Yes),
                opened_at: now - Duration::days(days_ago),
                settled_at: Some(now - Duration::days(days_ago - 1)),
            });
        }
        let res = coordinator.whitelist().evaluate_wallet(addr, &records, now);
        assert!(res.is_accepted, "seed wallet must be accepted");
        coordinator.whitelist().upsert_wallet(res.profile);
    }

    /// Builds a coordinator holding YES+NO paper positions in `market_id`
    /// (YES 200 @ 0.60, NO 300 @ 0.40) via a two-wallet consensus buy.
    async fn coordinator_with_positions() -> CopyTradingCoordinator {
        let config = CopyTradingConfig::default();
        let now = Utc::now();
        let coordinator = CopyTradingCoordinator::new(config, dec!(10_000.0), now);
        seed_wallet(&coordinator, "0xAlpha", now);
        seed_wallet(&coordinator, "0xBeta", now);

        let market = MarketMetadata {
            market_id: "mkt-settle".to_string(),
            condition_id: "mkt-settle".to_string(),
            question: "Settle test?".to_string(),
            domain: DomainTag::MacroInterestRate,
            outcome_count: 2,
            is_neg_risk: false,
            settlement_time: now + Duration::hours(100),
            volume_24h_usdc: dec!(100_000.0),
            is_active: true,
        };
        let book = OrderBookSnapshot {
            market_id: "mkt-settle".to_string(),
            token_id: "tok-yes".to_string(),
            bids: vec![BookLevel { price: dec!(0.595), size: dec!(1000.0) }],
            asks: vec![BookLevel { price: dec!(0.605), size: dec!(1000.0) }],
            timestamp: now,
        };
        let mk = |id: &str, wallet: &str| SignalEvent {
            signal_id: id.to_string(),
            source_wallet: wallet.to_string(),
            market_id: "mkt-settle".to_string(),
            token_id: "tok-yes".to_string(),
            outcome: OutcomeSide::Yes,
            domain: DomainTag::MacroInterestRate,
            action: SignalAction::OpenBuy,
            target_vwap: dec!(0.60),
            target_shares: dec!(200.0),
            target_notional_usdc: dec!(120.0),
            timestamp: now,
        };
        let _ = coordinator
            .process_signal(mk("s1", "0xAlpha"), Some(&market), &book, now)
            .await
            .unwrap();
        let prop = coordinator
            .process_signal(mk("s2", "0xBeta"), Some(&market), &book, now + Duration::seconds(5))
            .await
            .unwrap()
            .unwrap();
        coordinator.on_order_placed("order-1".to_string(), prop.clone(), now);
        coordinator
            .on_order_fill("order-1", "mkt-settle", "tok-yes", prop.shares, prop.limit_price, now)
            .await;

        // Add the NO position directly (opposite side of the same market).
        coordinator
            .risk()
            .read()
            .await
            .record_position_fill("mkt-settle", "tok-no", &OutcomeSide::No, dec!(300.0), dec!(0.40));

        coordinator
    }

    #[tokio::test]
    async fn settlement_flow_marks_realizes_and_prunes() {
        let coordinator = coordinator_with_positions().await;
        let source = Arc::new(FakeSource {
            response: Mutex::new(Some(SettlementInfo {
                closed: Some(true),
                outcome_prices: Some(serde_json::json!(["0", "1"])), // NO wins
            })),
        });
        let monitor = SettlementMonitor::new(source);

        let settled = monitor
            .check_and_settle(&coordinator, None, true)
            .await;
        assert_eq!(settled, vec!["mkt-settle".to_string()]);

        let risk = coordinator.risk().read().await;
        // NO winner realized: cash credited 300, position removed.
        let no_pos = risk.get_market_exposure("mkt-settle", &OutcomeSide::No);
        assert!(no_pos.is_none(), "winner position must be realized and removed");
        // YES loser pruned (marked 0.0).
        let yes_pos = risk.get_market_exposure("mkt-settle", &OutcomeSide::Yes);
        assert!(yes_pos.is_none(), "loser position must be pruned");
        assert!(risk.list_exposures().is_empty());
    }

    #[tokio::test]
    async fn unresolved_market_is_not_settled() {
        let coordinator = coordinator_with_positions().await;
        let source = Arc::new(FakeSource {
            response: Mutex::new(Some(SettlementInfo {
                closed: Some(false),
                outcome_prices: Some(serde_json::json!(["0.5", "0.5"])),
            })),
        });
        let monitor = SettlementMonitor::new(source);
        let settled = monitor.check_and_settle(&coordinator, None, true).await;
        assert!(settled.is_empty());
        assert_eq!(coordinator.risk().read().await.list_exposures().len(), 2);
    }

    #[test]
    fn winner_determination_maps_prices_to_side() {
        assert_eq!(determine_resolved_outcome(&[dec!(0.0), dec!(1.0)]), Some(OutcomeSide::No));
        assert_eq!(determine_resolved_outcome(&[dec!(1.0), dec!(0.0)]), Some(OutcomeSide::Yes));
        assert_eq!(determine_resolved_outcome(&[dec!(0.99), dec!(0.01)]), Some(OutcomeSide::Yes));
        assert_eq!(determine_resolved_outcome(&[dec!(0.5), dec!(0.5)]), None);
        assert_eq!(determine_resolved_outcome(&[]), None);
    }
}
