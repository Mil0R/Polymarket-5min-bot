//! Laddered scale-out exit (review Opinion 5).
//!
//! When a single exit attempt at the then-best bid does not fully fill (the
//! target whale's sell pushed the book down, or depth was insufficient), the
//! position can be stuck until resolution. This tracker re-attempts the exit
//! at descending prices over a bounded number of cycles so the position is
//! cleared without dumping into the book.
//!
//! Env config:
//!   COPY_EXIT_LADDER_ENABLED      = true (default)
//!   COPY_EXIT_LADDER_MAX_ATTEMPTS = 3
//!   COPY_EXIT_LADDER_STEP         = 0.005 (price descent per attempt)

use crate::wallet_copy_trading::coordinator::WalletCopyTradingCoordinator;
use crate::wallet_copy_trading::feed::BookFetcher;
use crate::wallet_copy_trading::live_executor::CopyTradeExecutor;
use crate::wallet_copy_trading::types::{ExecutionOrderProposal, OrderSide, OutcomeSide};
use chrono::Utc;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::HashMap;
use std::env;
use std::sync::{Arc, Mutex};
use tracing::{debug, info, warn};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LadderExitConfig {
    pub enabled: bool,
    pub max_attempts: u32,
    pub step: Decimal,
}

impl Default for LadderExitConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_attempts: 3,
            step: dec!(0.005),
        }
    }
}

impl LadderExitConfig {
    pub fn from_env() -> Self {
        let enabled = env::var("COPY_EXIT_LADDER_ENABLED")
            .map(|v| v.trim().to_lowercase() != "false" && v.trim() != "0")
            .unwrap_or(true);
        let max_attempts = env::var("COPY_EXIT_LADDER_MAX_ATTEMPTS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(3);
        let step = env::var("COPY_EXIT_LADDER_STEP")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(dec!(0.005));
        Self {
            enabled,
            max_attempts,
            step,
        }
    }
}

/// State of one pending laddered exit for a (market, outcome) position.
#[derive(Debug, Clone)]
pub struct LadderExit {
    pub market_id: String,
    pub token_id: String,
    pub outcome: OutcomeSide,
    pub remaining_shares: Decimal,
    pub attempts: u32,
    pub last_price: Decimal,
}

/// Pure sizing/price helpers (unit-testable).
pub fn ladder_target_price(best_bid: Decimal, attempts: u32, step: Decimal) -> Option<Decimal> {
    let target = best_bid - step * Decimal::from(attempts + 1);
    (target > Decimal::ZERO).then_some(target)
}

pub fn ladder_size(
    remaining: Decimal,
    exposure_shares: Decimal,
    top5_bid_notional: Decimal,
    best_bid: Decimal,
    max_depth_pct: Decimal,
) -> Decimal {
    let mut size = remaining.min(exposure_shares).max(Decimal::ZERO);
    if top5_bid_notional > Decimal::ZERO && best_bid > Decimal::ZERO {
        let depth_cap = (top5_bid_notional * max_depth_pct) / best_bid;
        size = size.min(depth_cap);
    }
    ((size * dec!(100)).floor()) / dec!(100)
}

#[derive(Clone)]
pub struct LadderExitTracker {
    config: LadderExitConfig,
    exits: Arc<Mutex<HashMap<(String, OutcomeSide), LadderExit>>>,
}

impl LadderExitTracker {
    pub fn new(config: LadderExitConfig) -> Self {
        Self {
            config,
            exits: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Registers/refreshes a pending exit for a position (called after an exit
    /// signal produced an incomplete fill).
    pub fn record_exit_intent(
        &self,
        market_id: &str,
        outcome: &OutcomeSide,
        token_id: &str,
        remaining_shares: Decimal,
        last_price: Decimal,
    ) {
        if !self.config.enabled || remaining_shares <= Decimal::ZERO {
            return;
        }
        let mut exits = self.exits.lock().expect("ladder exits lock poisoned");
        exits.insert(
            (market_id.to_string(), outcome.clone()),
            LadderExit {
                market_id: market_id.to_string(),
                token_id: token_id.to_string(),
                outcome: outcome.clone(),
                remaining_shares,
                attempts: 0,
                last_price,
            },
        );
        info!(
            market = %market_id,
            outcome = ?outcome,
            remaining = %remaining_shares,
            "Recorded laddered exit intent"
        );
    }

    pub fn pending_count(&self) -> usize {
        self.exits.lock().expect("ladder exits lock poisoned").len()
    }

    /// Runs one ladder pass: for each pending exit with an open position, try
    /// to sell at a descending price. Returns the number of orders placed.
    pub async fn run(
        &self,
        coordinator: &WalletCopyTradingCoordinator,
        book_fetcher: &BookFetcher,
        live_executor: Option<&CopyTradeExecutor>,
        dry_run: bool,
    ) -> usize {
        if !self.config.enabled {
            return 0;
        }
        let mut placed = 0usize;
        let mut keys_to_remove = Vec::new();

        let snapshot = {
            let exits = self.exits.lock().expect("ladder exits lock poisoned");
            exits.values().cloned().collect::<Vec<_>>()
        };

        for mut exit in snapshot {
            // Position gone -> done.
            let exposure = coordinator
                .risk()
                .read()
                .await
                .get_market_exposure(&exit.market_id, &exit.outcome);
            let exposure_shares = exposure.map(|e| e.shares).unwrap_or(Decimal::ZERO);
            if exposure_shares <= Decimal::ZERO {
                keys_to_remove.push((exit.market_id.clone(), exit.outcome.clone()));
                continue;
            }
            // Attempt budget exhausted -> give up and keep the (marked) position.
            if exit.attempts >= self.config.max_attempts {
                warn!(
                    market = %exit.market_id,
                    outcome = ?exit.outcome,
                    attempts = exit.attempts,
                    remaining = %exit.remaining_shares,
                    "Laddered exit gave up after max attempts; position retained"
                );
                keys_to_remove.push((exit.market_id.clone(), exit.outcome.clone()));
                continue;
            }

            let book = match book_fetcher.fetch(&exit.token_id, &exit.market_id).await {
                Ok(b) => b,
                Err(e) => {
                    debug!(token = %exit.token_id, error = %e, "Ladder book fetch failed");
                    continue;
                }
            };
            let best_bid = match book.best_bid() {
                Some(b) if b > Decimal::ZERO => b,
                _ => continue,
            };
            let target_price = match ladder_target_price(best_bid, exit.attempts, self.config.step)
            {
                Some(p) => p,
                None => continue,
            };
            let top5_bid = book.top_n_bid_depth_notional(5);
            let size = ladder_size(
                exit.remaining_shares,
                exposure_shares,
                top5_bid,
                best_bid,
                dec!(0.10),
            );
            if size <= Decimal::ZERO {
                continue;
            }

            let proposal = ExecutionOrderProposal {
                proposal_id: format!("ladder-{}-{}", exit.market_id, Utc::now().timestamp()),
                market_id: exit.market_id.clone(),
                token_id: exit.token_id.clone(),
                outcome: exit.outcome.clone(),
                side: OrderSide::Sell,
                limit_price: target_price,
                shares: size,
                notional_usdc: (size * target_price).round_dp(2),
                ttl_seconds: 45,
                reason: format!(
                    "Ladder exit attempt {}/{} (descent {})",
                    exit.attempts + 1,
                    self.config.max_attempts,
                    self.config.step
                ),
                created_at: Utc::now(),
            };

            if dry_run {
                let order_id = format!("paper-{}", proposal.proposal_id);
                coordinator.on_order_placed(order_id.clone(), proposal.clone(), Utc::now());
                coordinator
                    .on_order_fill(
                        &order_id,
                        &proposal.market_id,
                        &proposal.token_id,
                        size,
                        target_price,
                        Utc::now(),
                    )
                    .await;
                coordinator
                    .on_order_finalized(Some(&order_id), &proposal.proposal_id, Utc::now())
                    .await;
                exit.remaining_shares -= size;
                info!(
                    market = %exit.market_id,
                    size = %size,
                    price = %target_price,
                    remaining = %exit.remaining_shares,
                    "[PAPER] ladder exit leg filled"
                );
            } else if let Some(executor) = live_executor {
                match executor.execute_sell(&proposal).await {
                    Ok(outcome) => {
                        coordinator.on_order_placed(
                            outcome.order_id.clone(),
                            proposal.clone(),
                            Utc::now(),
                        );
                        if outcome.filled_shares > Decimal::ZERO {
                            coordinator
                                .on_order_fill(
                                    &outcome.order_id,
                                    &proposal.market_id,
                                    &proposal.token_id,
                                    outcome.filled_shares,
                                    outcome.avg_price,
                                    Utc::now(),
                                )
                                .await;
                            exit.remaining_shares -= outcome.filled_shares;
                            info!(
                                market = %exit.market_id,
                                filled = %outcome.filled_shares,
                                price = %outcome.avg_price,
                                remaining = %exit.remaining_shares,
                                "[LIVE] ladder exit leg filled"
                            );
                        }
                        coordinator
                            .on_order_finalized(
                                Some(&outcome.order_id),
                                &proposal.proposal_id,
                                Utc::now(),
                            )
                            .await;
                    }
                    Err(e) => warn!(
                        market = %exit.market_id,
                        error = %e,
                        "Ladder exit leg failed"
                    ),
                }
            }

            exit.attempts += 1;
            exit.last_price = target_price;
            placed += 1;
            if exit.remaining_shares > Decimal::ZERO {
                let mut exits = self.exits.lock().expect("ladder exits lock poisoned");
                exits.insert((exit.market_id.clone(), exit.outcome.clone()), exit.clone());
            } else {
                keys_to_remove.push((exit.market_id.clone(), exit.outcome.clone()));
            }
        }

        if !keys_to_remove.is_empty() {
            let mut exits = self.exits.lock().expect("ladder exits lock poisoned");
            for key in keys_to_remove {
                exits.remove(&key);
            }
        }
        placed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_price_descends_by_step_per_attempt() {
        assert_eq!(
            ladder_target_price(dec!(0.68), 0, dec!(0.005)),
            Some(dec!(0.675))
        );
        assert_eq!(
            ladder_target_price(dec!(0.68), 2, dec!(0.01)),
            Some(dec!(0.65))
        );
        // Never goes to zero or below.
        assert_eq!(ladder_target_price(dec!(0.003), 0, dec!(0.005)), None);
    }

    #[test]
    fn ladder_size_respects_remaining_exposure_and_depth() {
        // remaining/exposure cap
        assert_eq!(
            ladder_size(dec!(100), dec!(50), dec!(0), dec!(0.68), dec!(0.10)),
            dec!(50)
        );
        // depth cap: top5 495 * 10% / 0.68 = 72.79 -> floor to 72.79
        assert_eq!(
            ladder_size(dec!(500), dec!(500), dec!(495), dec!(0.68), dec!(0.10)),
            dec!(72.79)
        );
    }
}
