use crate::wallet_copy_trading::types::{OutcomeSide, WalletCopyTradingConfig};
use chrono::{DateTime, Duration, Utc};
use dashmap::DashMap;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::PathBuf;
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketExposure {
    pub market_id: String,
    pub token_id: String,
    pub outcome: OutcomeSide,
    pub notional_usdc: Decimal,
    pub shares: Decimal,
    pub avg_entry_price: Decimal,
    pub current_mark_price: Decimal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReservedOrderRisk {
    market_id: String,
    notional_usdc: Decimal,
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistedRiskState {
    cash_balance: Decimal,
    daily_base_nav: Decimal,
    daily_base_timestamp: DateTime<Utc>,
    market_exposures: Vec<MarketExposure>,
    circuit_breaker_until: Option<DateTime<Utc>>,
}

/// Portfolio risk manager and circuit breaker controller.
#[derive(Debug, Clone)]
pub struct RiskController {
    config: WalletCopyTradingConfig,
    cash_balance: Arc<RwLock<Decimal>>,
    daily_base_nav: Decimal,
    daily_base_timestamp: DateTime<Utc>,
    market_exposures: Arc<DashMap<String, MarketExposure>>, // keyed by token_id
    reserved_orders: Arc<DashMap<String, ReservedOrderRisk>>, // keyed by proposal_id
    circuit_breaker_until: Option<DateTime<Utc>>,
    state_path: Option<PathBuf>,
}

impl RiskController {
    pub fn new(config: WalletCopyTradingConfig, initial_nav: Decimal, now: DateTime<Utc>) -> Self {
        Self {
            config,
            cash_balance: Arc::new(RwLock::new(initial_nav)),
            daily_base_nav: initial_nav,
            daily_base_timestamp: now,
            market_exposures: Arc::new(DashMap::new()),
            reserved_orders: Arc::new(DashMap::new()),
            circuit_breaker_until: None,
            state_path: None,
        }
    }

    pub fn new_with_state(
        config: WalletCopyTradingConfig,
        initial_nav: Decimal,
        now: DateTime<Utc>,
        state_path: impl Into<PathBuf>,
    ) -> Self {
        let path = state_path.into();
        let mut controller = Self::new(config, initial_nav, now);
        controller.state_path = Some(path.clone());
        if let Ok(raw) = std::fs::read(&path) {
            match serde_json::from_slice::<PersistedRiskState>(&raw) {
                Ok(state) => {
                    *controller.cash_balance.write().unwrap() = state.cash_balance;
                    controller.daily_base_nav = state.daily_base_nav;
                    controller.daily_base_timestamp = state.daily_base_timestamp;
                    controller.circuit_breaker_until = state.circuit_breaker_until;
                    for exposure in state.market_exposures {
                        controller
                            .market_exposures
                            .insert(exposure.token_id.clone(), exposure);
                    }
                    info!(path = %path.display(), "Restored wallet-copy-trading risk state");
                }
                Err(e) => {
                    warn!(path = %path.display(), error = %e, "Risk state corrupt; starting from reconciled NAV")
                }
            }
        }
        controller
    }

    fn save_state(&self) {
        let Some(path) = &self.state_path else { return };
        let state = PersistedRiskState {
            cash_balance: self.cash_balance(),
            daily_base_nav: self.daily_base_nav,
            daily_base_timestamp: self.daily_base_timestamp,
            market_exposures: self.list_exposures(),
            circuit_breaker_until: self.circuit_breaker_until,
        };
        let write = || -> std::io::Result<()> {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let tmp = path.with_extension("json.tmp");
            std::fs::write(
                &tmp,
                serde_json::to_vec_pretty(&state).map_err(std::io::Error::other)?,
            )?;
            std::fs::rename(tmp, path)
        };
        if let Err(e) = write() {
            warn!(path = %path.display(), error = %e, "Risk state persistence failed");
        }
    }

    /// Resets daily baseline at 00:00 UTC or upon new day.
    pub fn check_daily_rollover(&mut self, now: DateTime<Utc>) {
        let mut changed = false;
        if now.date_naive() != self.daily_base_timestamp.date_naive() {
            let current_nav = self.calculate_total_nav();
            self.daily_base_nav = current_nav;
            self.daily_base_timestamp = now;
            info!(
                date = %now.date_naive(),
                base_nav = %self.daily_base_nav,
                "Rolled over daily risk baseline NAV"
            );
            changed = true;
        }

        // Lift circuit breaker if cooldown passed
        if let Some(until) = self.circuit_breaker_until {
            if now >= until {
                self.circuit_breaker_until = None;
                changed = true;
                info!(now = %now, "Daily drawdown circuit breaker cooldown elapsed. Trading resumed.");
            }
        }
        if changed {
            self.save_state();
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

    /// Current cash (USDC) balance held by the risk engine.
    pub fn cash_balance(&self) -> Decimal {
        *self.cash_balance.read().unwrap()
    }

    /// Reconciles collateral against the exchange/chain source of truth while
    /// preserving restored outcome-token positions.
    pub fn reconcile_cash_balance(&self, actual_cash: Decimal) {
        if actual_cash < Decimal::ZERO {
            return;
        }
        *self.cash_balance.write().unwrap() = actual_cash;
        self.save_state();
    }

    /// Applies a strategy allocation cap after restoring PAPER state. Existing
    /// positions keep their marks, while free cash is reduced so total NAV does
    /// not exceed the configured Wallet-Copy Trading budget.
    pub fn reconcile_paper_budget(&self, budget: Decimal) {
        if budget < Decimal::ZERO {
            return;
        }
        let position_value: Decimal = self
            .market_exposures
            .iter()
            .map(|entry| entry.shares * entry.current_mark_price)
            .sum();
        *self.cash_balance.write().unwrap() = (budget - position_value).max(Decimal::ZERO);
        self.save_state();
    }

    pub fn reconcile_position_shares(&self, token_id: &str, actual_shares: Decimal) {
        if actual_shares < Decimal::ZERO {
            return;
        }
        if let Some(mut exposure) = self.market_exposures.get_mut(token_id) {
            exposure.shares = actual_shares;
            exposure.notional_usdc = actual_shares * exposure.current_mark_price;
        }
        self.save_state();
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
                    "CRITICAL: Daily max drawdown (5%) breached! Halting wallet-copy-trading for 24 hours."
                );
                self.save_state();
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
            let until = self
                .circuit_breaker_until
                .unwrap_or(now + Duration::hours(24));
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
        let reserved_market: Decimal = self
            .reserved_orders
            .iter()
            .filter(|e| e.market_id == market_id)
            .map(|e| e.notional_usdc)
            .sum();
        let current_market_exposure = current_market_exposure + reserved_market;

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
        let total_exposure: Decimal = self
            .market_exposures
            .iter()
            .map(|e| e.notional_usdc)
            .sum::<Decimal>()
            + self
                .reserved_orders
                .iter()
                .map(|e| e.notional_usdc)
                .sum::<Decimal>();
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
        let reserved_cash: Decimal = self.reserved_orders.iter().map(|e| e.notional_usdc).sum();
        let available_cash =
            (*self.cash_balance.read().unwrap() - reserved_cash).max(Decimal::ZERO);
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

    /// Reserves cash/exposure for a proposal before it leaves the strategy. This
    /// closes the gap where several pending orders could each pass limits using
    /// the same cash balance.
    pub fn reserve_order(&self, proposal_id: &str, market_id: &str, notional: Decimal) -> bool {
        if notional <= Decimal::ZERO || self.reserved_orders.contains_key(proposal_id) {
            return false;
        }
        self.reserved_orders.insert(
            proposal_id.to_string(),
            ReservedOrderRisk {
                market_id: market_id.to_string(),
                notional_usdc: notional,
            },
        );
        true
    }

    pub fn release_order_reservation(&self, proposal_id: &str) {
        self.reserved_orders.remove(proposal_id);
    }

    pub fn reserved_notional(&self) -> Decimal {
        self.reserved_orders.iter().map(|e| e.notional_usdc).sum()
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
        if added_shares <= Decimal::ZERO || fill_price <= Decimal::ZERO || fill_price > Decimal::ONE
        {
            warn!(token_id = %token_id, shares = %added_shares, price = %fill_price, "Invalid BUY fill ignored");
            return;
        }
        let cost = added_shares * fill_price;
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

        if let Ok(mut cash) = self.cash_balance.write() {
            *cash -= cost;
        }

        let total_shares = entry.shares + added_shares;
        if total_shares > Decimal::ZERO {
            let total_cost = (entry.shares * entry.avg_entry_price) + (added_shares * fill_price);
            entry.avg_entry_price = total_cost / total_shares;
            entry.shares = total_shares;
            entry.current_mark_price = fill_price;
            entry.notional_usdc = total_shares * fill_price;
        }
        drop(entry);
        self.save_state();
    }

    /// Records a filled SELL (position reduction/close), keyed by token.
    pub fn record_position_reduction(
        &self,
        token_id: &str,
        reduced_shares: Decimal,
        exit_price: Decimal,
    ) {
        if reduced_shares <= Decimal::ZERO
            || exit_price <= Decimal::ZERO
            || exit_price > Decimal::ONE
        {
            warn!(token_id = %token_id, shares = %reduced_shares, price = %exit_price, "Invalid SELL fill ignored");
            return;
        }
        let Some(mut entry) = self.market_exposures.get_mut(token_id) else {
            warn!(token_id = %token_id, "SELL fill for unknown position ignored");
            return;
        };
        let actual_reduction = reduced_shares.min(entry.shares).max(Decimal::ZERO);
        if actual_reduction <= Decimal::ZERO {
            return;
        }
        if reduced_shares > entry.shares {
            warn!(token_id = %token_id, requested = %reduced_shares, held = %entry.shares, "SELL fill capped to recorded holding");
        }
        let proceeds = actual_reduction * exit_price;
        if actual_reduction >= entry.shares {
            entry.shares = Decimal::ZERO;
            entry.notional_usdc = Decimal::ZERO;
        } else {
            entry.shares -= actual_reduction;
            entry.current_mark_price = exit_price;
            entry.notional_usdc = entry.shares * exit_price;
        }
        drop(entry);
        if let Ok(mut cash) = self.cash_balance.write() {
            *cash += proceeds;
        }
        self.save_state();
    }

    /// Returns current exposure for a specific (market, outcome) direction.
    /// Direction-aware lookup: an exit signal on the NO side cannot ever touch a
    /// YES position, even within the same market.
    pub fn get_market_exposure(
        &self,
        market_id: &str,
        outcome: &OutcomeSide,
    ) -> Option<MarketExposure> {
        self.market_exposures
            .iter()
            .find(|e| e.market_id == market_id && e.outcome == *outcome)
            .map(|e| e.value().clone())
    }

    /// Returns all open positions (for mark-price refresh loops and reporting).
    pub fn list_exposures(&self) -> Vec<MarketExposure> {
        self.market_exposures
            .iter()
            .map(|e| e.value().clone())
            .collect()
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
        self.save_state();
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
        self.save_state();
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
        if removed > 0 {
            self.save_state();
        }
        removed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wallet_copy_trading::types::WalletCopyTradingConfig;
    use rust_decimal_macros::dec;

    #[test]
    fn settlement_closed_loop_realizes_winner_and_prunes_loser() {
        let now = Utc::now();
        let risk = RiskController::new(WalletCopyTradingConfig::default(), dec!(10_000.0), now);

        // Hold YES (winner) and NO (loser) in the same market.
        risk.record_position_fill(
            "mkt-s",
            "tok-yes",
            &OutcomeSide::Yes,
            dec!(200.0),
            dec!(0.60),
        );
        risk.record_position_fill("mkt-s", "tok-no", &OutcomeSide::No, dec!(300.0), dec!(0.40));
        // cash = 10000 - 120 - 120 = 9760; NAV = 9760 + 120 + 120 = 10000

        // Market resolves YES: mark YES -> 1.0, NO -> 0.0.
        risk.settle_market_positions("mkt-s", &OutcomeSide::Yes);
        let nav_before_realize = risk.calculate_total_nav();
        assert_eq!(nav_before_realize, dec!(9960.0)); // 9760 + 200 + 0

        // Realize the winning YES position into cash (proceeds = shares x 1.0).
        let credited = risk
            .realize_settled_position("tok-yes", dec!(200.0))
            .unwrap();
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
        let risk = RiskController::new(WalletCopyTradingConfig::default(), dec!(10_000.0), now);
        risk.record_position_fill("mkt-x", "tok-a", &OutcomeSide::Yes, dec!(50.0), dec!(0.50));

        assert!(risk.realize_settled_position("tok-a", dec!(50.0)).is_some());
        // Second call: position already zeroed -> None, cash not double-credited.
        assert!(risk.realize_settled_position("tok-a", dec!(50.0)).is_none());
        // cash = 10000 - (50*0.50=25) + 50 realized = 10025; NAV unchanged by realize.
        assert_eq!(*risk.cash_balance.read().unwrap(), dec!(10025.0));
        assert_eq!(risk.calculate_total_nav(), dec!(10025.0));
    }

    #[test]
    fn pending_orders_reserve_cash_and_exposure() {
        let now = Utc::now();
        let cfg = WalletCopyTradingConfig {
            single_trade_allocation_pct: dec!(0.10),
            max_single_market_exposure_pct: dec!(0.10),
            ..WalletCopyTradingConfig::default()
        };
        let mut risk = RiskController::new(cfg, dec!(1_000), now);
        let first = risk.allocate_trade_budget("mkt-a", now).unwrap();
        assert_eq!(first, dec!(100));
        assert!(risk.reserve_order("p1", "mkt-a", first));
        assert!(risk.allocate_trade_budget("mkt-a", now).is_err());
        risk.release_order_reservation("p1");
        assert!(risk.allocate_trade_budget("mkt-a", now).is_ok());
    }

    #[test]
    fn unknown_or_oversized_sell_cannot_create_cash() {
        let now = Utc::now();
        let risk = RiskController::new(WalletCopyTradingConfig::default(), dec!(1_000), now);
        risk.record_position_reduction("missing", dec!(100), dec!(0.5));
        assert_eq!(risk.cash_balance(), dec!(1_000));
        risk.record_position_fill("m", "tok", &OutcomeSide::Yes, dec!(10), dec!(0.5));
        risk.record_position_reduction("tok", dec!(100), dec!(0.6));
        assert_eq!(risk.cash_balance(), dec!(1001)); // 1000 - 5 + min(100, 10)*0.6
    }

    #[test]
    fn risk_positions_survive_restart_and_cash_reconciles() {
        let now = Utc::now();
        let path = std::env::temp_dir().join(format!("copy-risk-{}.json", uuid::Uuid::new_v4()));
        {
            let risk = RiskController::new_with_state(
                WalletCopyTradingConfig::default(),
                dec!(1_000),
                now,
                path.clone(),
            );
            risk.record_position_fill("m", "tok", &OutcomeSide::Yes, dec!(10), dec!(0.5));
        }
        let restored = RiskController::new_with_state(
            WalletCopyTradingConfig::default(),
            dec!(9999),
            now,
            path.clone(),
        );
        assert_eq!(restored.list_exposures().len(), 1);
        assert_eq!(restored.cash_balance(), dec!(995));
        restored.reconcile_cash_balance(dec!(900));
        assert_eq!(restored.cash_balance(), dec!(900));
        let _ = std::fs::remove_file(path);
    }
}
