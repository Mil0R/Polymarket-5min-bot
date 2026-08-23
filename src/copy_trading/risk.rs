use crate::copy_trading::types::{CopyTradingConfig, OutcomeSide};
use chrono::{DateTime, Duration, Utc};
use dashmap::DashMap;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::fmt;
use std::sync::{Arc, RwLock};
use tracing::{error, info, warn};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RiskRejectionReason {
    CircuitBreakerActive {
        frozen_until: DateTime<Utc>,
        current_daily_drawdown_pct: Decimal,
    },
    MarketExposureLimitExceeded {
        current_exposure: Decimal,
        requested_amount: Decimal,
        max_allowed_exposure: Decimal,
    },
    PortfolioExposureLimitExceeded {
        current_total_exposure: Decimal,
        requested_amount: Decimal,
        max_total_exposure: Decimal,
    },
    InsufficientCashBalance {
        available: Decimal,
        required: Decimal,
    },
    OrderSizeBelowMinimum {
        size_usdc: Decimal,
        min_size: Decimal,
    },
}

impl fmt::Display for RiskRejectionReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RiskRejectionReason::CircuitBreakerActive {
                frozen_until,
                current_daily_drawdown_pct,
            } => write!(
                f,
                "Trading halted: Daily drawdown circuit breaker active ({:.2}% loss). Frozen until {}",
                current_daily_drawdown_pct * dec!(100),
                frozen_until
            ),
            RiskRejectionReason::MarketExposureLimitExceeded {
                current_exposure,
                requested_amount,
                max_allowed_exposure,
            } => write!(
                f,
                "Market exposure cap exceeded: current ${:.2} + req ${:.2} > max ${:.2}",
                current_exposure, requested_amount, max_allowed_exposure
            ),
            RiskRejectionReason::PortfolioExposureLimitExceeded {
                current_total_exposure,
                requested_amount,
                max_total_exposure,
            } => write!(
                f,
                "Portfolio exposure cap exceeded: current ${:.2} + req ${:.2} > max ${:.2}",
                current_total_exposure, requested_amount, max_total_exposure
            ),
            RiskRejectionReason::InsufficientCashBalance { available, required } => write!(
                f,
                "Insufficient cash balance: available ${:.2} < required ${:.2}",
                available, required
            ),
            RiskRejectionReason::OrderSizeBelowMinimum { size_usdc, min_size } => write!(
                f,
                "Calculated order size ${:.2} is below minimum allowed ${:.2}",
                size_usdc, min_size
            ),
        }
    }
}

impl std::error::Error for RiskRejectionReason {}

/// Active market position tracking for risk management.
///
/// Positions are keyed by TOKEN (one token == one directional outcome), so a
/// single market can hold both a YES and a NO position without corrupting each
/// other's share/price accounting.
#[derive(Debug, Clone)]
pub struct MarketExposure {
    pub market_id: String,
    pub token_id: String,
    pub outcome: OutcomeSide,
    pub notional_usdc: Decimal,
    pub shares: Decimal,
    pub avg_entry_price: Decimal,
    pub current_mark_price: Decimal,
}

/// Portfolio risk manager and circuit breaker controller.
#[derive(Debug, Clone)]
pub struct RiskController {
    config: CopyTradingConfig,
    cash_balance: Arc<RwLock<Decimal>>,
    daily_base_nav: Decimal,
    daily_base_timestamp: DateTime<Utc>,
    market_exposures: Arc<DashMap<String, MarketExposure>>, // keyed by token_id
    circuit_breaker_until: Option<DateTime<Utc>>,
}

impl RiskController {
    pub fn new(config: CopyTradingConfig, initial_nav: Decimal, now: DateTime<Utc>) -> Self {
        Self {
            config,
            cash_balance: Arc::new(RwLock::new(initial_nav)),
            daily_base_nav: initial_nav,
            daily_base_timestamp: now,
            market_exposures: Arc::new(DashMap::new()),
            circuit_breaker_until: None,
        }
    }

    /// Resets daily baseline at 00:00 UTC or upon new day.
    pub fn check_daily_rollover(&mut self, now: DateTime<Utc>) {
        if now.date_naive() != self.daily_base_timestamp.date_naive() {
            let current_nav = self.calculate_total_nav();
            self.daily_base_nav = current_nav;
            self.daily_base_timestamp = now;
            info!(
                date = %now.date_naive(),
                base_nav = %self.daily_base_nav,
                "Rolled over daily risk baseline NAV"
            );
        }

        // Lift circuit breaker if cooldown passed
        if let Some(until) = self.circuit_breaker_until {
            if now >= until {
                self.circuit_breaker_until = None;
                info!(now = %now, "Daily drawdown circuit breaker cooldown elapsed. Trading resumed.");
            }
        }
    }

    /// Updates current mark prices for open positions (per token).
    /// This is the ONLY source of truth for unrealized P&L; callers must feed it
    /// from the live orderbook mid/best prices so the drawdown breaker works.
    pub fn update_mark_price(&self, token_id: &str, new_price: Decimal) {
        if let Some(mut exposure) = self.market_exposures.get_mut(token_id) {
            exposure.current_mark_price = new_price;
            exposure.notional_usdc = exposure.shares * new_price;
        }
    }

    /// Calculates live Total NAV = Cash + Marked Position Value.
    pub fn calculate_total_nav(&self) -> Decimal {
        let position_value: Decimal = self
            .market_exposures
            .iter()
            .map(|e| e.shares * e.current_mark_price)
            .sum();

        let cash = *self.cash_balance.read().unwrap();
        if cash < Decimal::ZERO {
            warn!(
                cash = %cash,
                "Cash balance is negative; NAV accounting may be inconsistent"
            );
        }
        cash + position_value
    }

    /// Checks if the daily maximum drawdown threshold (5%) has been reached.
    pub fn check_and_apply_drawdown_breaker(&mut self, now: DateTime<Utc>) -> bool {
        self.check_daily_rollover(now);

        if self.circuit_breaker_until.is_some() {
            return true;
        }

        let live_nav = self.calculate_total_nav();
        if self.daily_base_nav > Decimal::ZERO {
            let drawdown = (self.daily_base_nav - live_nav) / self.daily_base_nav;
            if drawdown >= self.config.daily_max_drawdown_pct {
                let cooldown_hours = self.config.circuit_breaker_cooldown_hours as i64;
                let freeze_until = now + Duration::hours(cooldown_hours);
                self.circuit_breaker_until = Some(freeze_until);

                error!(
                    live_nav = %live_nav,
                    base_nav = %self.daily_base_nav,
                    drawdown_pct = %(drawdown * dec!(100)),
                    frozen_until = %freeze_until,
                    "CRITICAL: Daily max drawdown (5%) breached! Halting copy-trading for 24 hours."
                );
                return true;
            }
        }

        false
    }

    /// Calculates allowable allocation budget for a trade according to Module 4 rules.
    ///
    /// Enforces, in order:
    ///   1. circuit breaker status
    ///   2. per-market exposure cap (<= 10% of NAV)
    ///   3. portfolio-wide total exposure cap (<= 30% of NAV)
    ///   4. available cash balance
    ///   5. minimum order size
    pub fn allocate_trade_budget(
        &mut self,
        market_id: &str,
        now: DateTime<Utc>,
    ) -> Result<Decimal, RiskRejectionReason> {
        // 1. Check circuit breaker status
        if self.check_and_apply_drawdown_breaker(now) {
            let until = self.circuit_breaker_until.unwrap_or(now + Duration::hours(24));
            let dd = if self.daily_base_nav > Decimal::ZERO {
                (self.daily_base_nav - self.calculate_total_nav()) / self.daily_base_nav
            } else {
                Decimal::ZERO
            };
            return Err(RiskRejectionReason::CircuitBreakerActive {
                frozen_until: until,
                current_daily_drawdown_pct: dd,
            });
        }

        let nav = self.calculate_total_nav();

        // 2. Fixed fraction allocation (1% ~ 3%)
        let standard_allocation = nav * self.config.single_trade_allocation_pct;

        // 3. Single market exposure cap check (<= 10% of NAV), summed across all
        //    tokens of the market so YES+NO positions count against the same cap.
        let current_market_exposure: Decimal = self
            .market_exposures
            .iter()
            .filter(|e| e.market_id == market_id)
            .map(|e| e.notional_usdc)
            .sum();

        let max_market_exposure = nav * self.config.max_single_market_exposure_pct;

        if current_market_exposure >= max_market_exposure {
            return Err(RiskRejectionReason::MarketExposureLimitExceeded {
                current_exposure: current_market_exposure,
                requested_amount: standard_allocation,
                max_allowed_exposure: max_market_exposure,
            });
        }

        let max_remaining_capacity = max_market_exposure - current_market_exposure;
        let mut final_allocated = standard_allocation.min(max_remaining_capacity);

        // 4. Portfolio-wide total exposure cap (<= 30% of NAV)
        let total_exposure: Decimal = self.market_exposures.iter().map(|e| e.notional_usdc).sum();
        let max_total_exposure = nav * self.config.max_total_exposure_pct;
        if total_exposure + final_allocated > max_total_exposure {
            final_allocated = (max_total_exposure - total_exposure).max(Decimal::ZERO);
            if final_allocated < self.config.min_order_notional_usdc {
                return Err(RiskRejectionReason::PortfolioExposureLimitExceeded {
                    current_total_exposure: total_exposure,
                    requested_amount: standard_allocation,
                    max_total_exposure,
                });
            }
        }

        // 5. Cash availability check: never allocate more than we actually hold.
        let available_cash = *self.cash_balance.read().unwrap();
        if final_allocated > available_cash {
            final_allocated = available_cash.max(Decimal::ZERO);
            if final_allocated < self.config.min_order_notional_usdc {
                return Err(RiskRejectionReason::InsufficientCashBalance {
                    available: available_cash,
                    required: self.config.min_order_notional_usdc,
                });
            }
        }

        // 6. Minimum order size
        if final_allocated < self.config.min_order_notional_usdc {
            return Err(RiskRejectionReason::OrderSizeBelowMinimum {
                size_usdc: final_allocated,
                min_size: self.config.min_order_notional_usdc,
            });
        }

        Ok(final_allocated)
    }

    /// Records a filled BUY position, keyed by token so opposite directions in the
    /// same market never merge their share/price accounting.
    pub fn record_position_fill(
        &self,
        market_id: &str,
        token_id: &str,
        outcome: &OutcomeSide,
        added_shares: Decimal,
        fill_price: Decimal,
    ) {
        let cost = added_shares * fill_price;
        if let Ok(mut cash) = self.cash_balance.write() {
            *cash -= cost;
        }

        let mut entry = self
            .market_exposures
            .entry(token_id.to_string())
            .or_insert_with(|| MarketExposure {
                market_id: market_id.to_string(),
                token_id: token_id.to_string(),
                outcome: outcome.clone(),
                notional_usdc: Decimal::ZERO,
                shares: Decimal::ZERO,
                avg_entry_price: Decimal::ZERO,
                current_mark_price: fill_price,
            });

        // Defensive: if the token already exists under a different market or outcome,
        // do not silently merge — log and overwrite identity is not acceptable.
        if entry.market_id != market_id || entry.outcome != *outcome {
            warn!(
                token_id = %token_id,
                existing_market = %entry.market_id,
                incoming_market = %market_id,
                existing_outcome = ?entry.outcome,
                incoming_outcome = ?outcome,
                "Position identity mismatch on fill; ignoring mismatched fill"
            );
            return;
        }

        let total_shares = entry.shares + added_shares;
        if total_shares > Decimal::ZERO {
            let total_cost = (entry.shares * entry.avg_entry_price) + (added_shares * fill_price);
            entry.avg_entry_price = total_cost / total_shares;
            entry.shares = total_shares;
            entry.current_mark_price = fill_price;
            entry.notional_usdc = total_shares * fill_price;
        }
    }

    /// Records a filled SELL (position reduction/close), keyed by token.
    pub fn record_position_reduction(&self, token_id: &str, reduced_shares: Decimal, exit_price: Decimal) {
        let proceeds = reduced_shares * exit_price;
        if let Ok(mut cash) = self.cash_balance.write() {
            *cash += proceeds;
        }

        if let Some(mut entry) = self.market_exposures.get_mut(token_id) {
            if reduced_shares >= entry.shares {
                entry.shares = Decimal::ZERO;
                entry.notional_usdc = Decimal::ZERO;
            } else {
                entry.shares -= reduced_shares;
                entry.current_mark_price = exit_price;
                entry.notional_usdc = entry.shares * exit_price;
            }
        }
    }

    /// Returns current exposure for a specific (market, outcome) direction.
    /// Direction-aware lookup: an exit signal on the NO side cannot ever touch a
    /// YES position, even within the same market.
    pub fn get_market_exposure(&self, market_id: &str, outcome: &OutcomeSide) -> Option<MarketExposure> {
        self.market_exposures
            .iter()
            .find(|e| e.market_id == market_id && e.outcome == *outcome)
            .map(|e| e.value().clone())
    }

    /// Returns all open positions (for mark-price refresh loops and reporting).
    pub fn list_exposures(&self) -> Vec<MarketExposure> {
        self.market_exposures.iter().map(|e| e.value().clone()).collect()
    }

    /// Marks all positions of a market to their settlement value (1.0 for the
    /// resolved side, 0.0 otherwise), realizing the outcome into NAV.
    pub fn settle_market_positions(&self, market_id: &str, resolved_outcome: &OutcomeSide) {
        for mut entry in self.market_exposures.iter_mut() {
            if entry.market_id == market_id {
                let final_price = if entry.outcome == *resolved_outcome {
                    dec!(1.0)
                } else {
                    dec!(0.0)
                };
                entry.current_mark_price = final_price;
                entry.notional_usdc = entry.shares * final_price;
                info!(
                    market_id = %market_id,
                    token_id = %entry.token_id,
                    outcome = ?entry.outcome,
                    resolved_outcome = ?resolved_outcome,
                    marked = %final_price,
                    "Settled market position to final outcome value"
                );
            }
        }
    }

    /// Converts a settled WINNING position into cash (post on-chain redeem, or
    /// simulated in paper mode). Cash is credited with `proceeds` and the token
    /// position is removed, keeping NAV continuous (the position was already
    /// marked at 1.0 by [`Self::settle_market_positions`]).
    ///
    /// Returns the credited amount, or `None` if no matching position exists.
    pub fn realize_settled_position(&self, token_id: &str, proceeds: Decimal) -> Option<Decimal> {
        let exists = self
            .market_exposures
            .get(token_id)
            .map(|e| e.shares > Decimal::ZERO)
            .unwrap_or(false);
        if !exists {
            return None;
        }

        if let Ok(mut cash) = self.cash_balance.write() {
            *cash += proceeds;
        }
        if let Some(mut entry) = self.market_exposures.get_mut(token_id) {
            entry.shares = Decimal::ZERO;
            entry.notional_usdc = Decimal::ZERO;
        }
        info!(
            token_id = %token_id,
            proceeds = %proceeds,
            "Realized settled position into cash"
        );
        Some(proceeds)
    }

    /// Removes zero-value/settled positions (shares gone OR marked to 0.0) so
    /// they stop consuming mark-refresh and exposure accounting.
    pub fn prune_settled_positions(&self) -> usize {
        let mut removed = 0usize;
        let keys: Vec<String> = self
            .market_exposures
            .iter()
            .filter(|e| e.shares <= Decimal::ZERO || e.notional_usdc <= Decimal::ZERO)
            .map(|e| e.key().clone())
            .collect();
        for key in keys {
            if self.market_exposures.remove(&key).is_some() {
                removed += 1;
            }
        }
        removed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::copy_trading::types::CopyTradingConfig;
    use rust_decimal_macros::dec;

    #[test]
    fn settlement_closed_loop_realizes_winner_and_prunes_loser() {
        let now = Utc::now();
        let risk = RiskController::new(CopyTradingConfig::default(), dec!(10_000.0), now);

        // Hold YES (winner) and NO (loser) in the same market.
        risk.record_position_fill("mkt-s", "tok-yes", &OutcomeSide::Yes, dec!(200.0), dec!(0.60));
        risk.record_position_fill("mkt-s", "tok-no", &OutcomeSide::No, dec!(300.0), dec!(0.40));
        // cash = 10000 - 120 - 120 = 9760; NAV = 9760 + 120 + 120 = 10000

        // Market resolves YES: mark YES -> 1.0, NO -> 0.0.
        risk.settle_market_positions("mkt-s", &OutcomeSide::Yes);
        let nav_before_realize = risk.calculate_total_nav();
        assert_eq!(nav_before_realize, dec!(9960.0)); // 9760 + 200 + 0

        // Realize the winning YES position into cash (proceeds = shares x 1.0).
        let credited = risk.realize_settled_position("tok-yes", dec!(200.0)).unwrap();
        assert_eq!(credited, dec!(200.0));
        // NAV must be CONTINUOUS across realize (position value -> cash 1:1).
        assert_eq!(risk.calculate_total_nav(), nav_before_realize);

        // Cash actually increased so future buys are not blocked.
        let cash = *risk.cash_balance.read().unwrap();
        assert_eq!(cash, dec!(9960.0));

        // The loser (shares>0 but marked 0.0) and the realized winner (shares=0)
        // are both pruned.
        assert_eq!(risk.prune_settled_positions(), 2);
        assert!(risk.list_exposures().is_empty());
    }

    #[test]
    fn realize_settled_position_is_idempotent_guarded() {
        let now = Utc::now();
        let risk = RiskController::new(CopyTradingConfig::default(), dec!(10_000.0), now);
        risk.record_position_fill("mkt-x", "tok-a", &OutcomeSide::Yes, dec!(50.0), dec!(0.50));

        assert!(risk.realize_settled_position("tok-a", dec!(50.0)).is_some());
        // Second call: position already zeroed -> None, cash not double-credited.
        assert!(risk.realize_settled_position("tok-a", dec!(50.0)).is_none());
        // cash = 10000 - (50*0.50=25) + 50 realized = 10025; NAV unchanged by realize.
        assert_eq!(*risk.cash_balance.read().unwrap(), dec!(10025.0));
        assert_eq!(risk.calculate_total_nav(), dec!(10025.0));
    }
}
