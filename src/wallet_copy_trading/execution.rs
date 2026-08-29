use crate::wallet_copy_trading::types::{
    ConsensusSignal, ExecutionOrderProposal, ExecutionOrderState, OrderBookSnapshot,
    OrderLifecycleStatus, OrderSide, WalletCopyTradingConfig,
};
use chrono::{DateTime, Duration, Utc};
use dashmap::DashMap;
use rust_decimal::Decimal;
use std::fmt;
use std::sync::Arc;
use tracing::{debug, info, warn};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutionError {
    BestAskExceedsSlippage {
        best_ask: Decimal,
        max_allowed_price: Decimal,
    },
    BestAskExceedsPriceOffset {
        best_ask: Decimal,
        max_allowed_price: Decimal,
    },
    OrderSizeBelowMinimum {
        size_usdc: Decimal,
        min_size: Decimal,
    },
    InsufficientOrderBookLiquidity {
        top5_depth_usdc: Decimal,
    },
    NoAskLiquidity,
    /// Consensus target VWAP outside sanity bounds; refusing to build on it.
    TargetVwapOutOfBounds {
        vwap: Decimal,
        min_vwap: Decimal,
        max_vwap: Decimal,
    },
}

impl fmt::Display for ExecutionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExecutionError::BestAskExceedsSlippage {
                best_ask,
                max_allowed_price,
            } => write!(
                f,
                "Best ask ${:.4} exceeds maximum slippage cap ${:.4}",
                best_ask, max_allowed_price
            ),
            ExecutionError::BestAskExceedsPriceOffset {
                best_ask,
                max_allowed_price,
            } => write!(
                f,
                "Best ask ${:.4} exceeds price offset cap ${:.4}",
                best_ask, max_allowed_price
            ),
            ExecutionError::OrderSizeBelowMinimum {
                size_usdc,
                min_size,
            } => write!(
                f,
                "Calculated order size ${:.2} is below minimum allowed ${:.2}",
                size_usdc, min_size
            ),
            ExecutionError::InsufficientOrderBookLiquidity { top5_depth_usdc } => write!(
                f,
                "Top-5 depth liquidity ${:.2} is too thin for safe execution",
                top5_depth_usdc
            ),
            ExecutionError::NoAskLiquidity => write!(f, "No ask liquidity in orderbook"),
            ExecutionError::TargetVwapOutOfBounds {
                vwap,
                min_vwap,
                max_vwap,
            } => write!(
                f,
                "Consensus target VWAP ${:.4} outside sanity bounds [{:.2}, {:.2}]",
                vwap, min_vwap, max_vwap
            ),
        }
    }
}

impl std::error::Error for ExecutionError {}

/// Smart limit order generation and execution protection engine.
#[derive(Debug, Clone)]
pub struct SmartExecutionEngine {
    config: WalletCopyTradingConfig,
    active_orders: Arc<DashMap<String, ExecutionOrderState>>,
}

impl SmartExecutionEngine {
    pub fn new(config: WalletCopyTradingConfig) -> Self {
        Self {
            config,
            active_orders: Arc::new(DashMap::new()),
        }
    }

    /// Creates a strictly priced limit order proposal adhering to Module 3 rules.
    pub fn build_buy_proposal(
        &self,
        consensus: &ConsensusSignal,
        book: &OrderBookSnapshot,
        allocated_budget_usdc: Decimal,
        now: DateTime<Utc>,
    ) -> Result<ExecutionOrderProposal, ExecutionError> {
        // 0. Sanity bounds on consensus VWAP. Even a whitelisted consensus must not
        //    be allowed to drive the price cap toward 1.0 on manipulated/far-book data.
        if consensus.target_vwap < self.config.min_signal_vwap
            || consensus.target_vwap > self.config.max_signal_vwap
        {
            return Err(ExecutionError::TargetVwapOutOfBounds {
                vwap: consensus.target_vwap,
                min_vwap: self.config.min_signal_vwap,
                max_vwap: self.config.max_signal_vwap,
            });
        }

        let best_ask = book.best_ask().ok_or(ExecutionError::NoAskLiquidity)?;

        // 1. Calculate price protection caps
        // Cap A: Target VWAP + max price offset ($0.01 ~ $0.02)
        let price_cap_offset = consensus.target_vwap + self.config.max_price_offset_usd;
        // Cap B: Target VWAP * (1 + max_slippage_pct)
        let price_cap_slippage =
            consensus.target_vwap * (Decimal::ONE + self.config.max_slippage_pct);

        let max_allowed_price = price_cap_offset.min(price_cap_slippage);

        // Limit price is placed at min(best_ask, max_allowed_price)
        // If current best ask already exceeds our max allowed price, we strictly refuse to chase high.
        if best_ask > max_allowed_price {
            if best_ask > price_cap_slippage {
                return Err(ExecutionError::BestAskExceedsSlippage {
                    best_ask,
                    max_allowed_price: price_cap_slippage,
                });
            } else {
                return Err(ExecutionError::BestAskExceedsPriceOffset {
                    best_ask,
                    max_allowed_price: price_cap_offset,
                });
            }
        }

        // Cross only the first five levels that remain inside the strategy cap.
        // The old implementation sized from five levels but placed at best ask,
        // so paper mode assumed liquidity that a live order could not reach.
        let eligible_asks: Vec<_> = book
            .asks
            .iter()
            .take(5)
            .filter(|level| level.price <= max_allowed_price)
            .collect();
        let limit_price = eligible_asks
            .last()
            .map(|level| level.price)
            .ok_or(ExecutionError::NoAskLiquidity)?;

        // 2. Anti-dump circuit breaker: eligible Top-5 depth liquidity <= 10%
        let top5_ask_notional: Decimal = eligible_asks
            .iter()
            .map(|level| level.price * level.size)
            .sum();
        if top5_ask_notional <= Decimal::ZERO {
            return Err(ExecutionError::InsufficientOrderBookLiquidity {
                top5_depth_usdc: Decimal::ZERO,
            });
        }

        let max_safe_notional_from_depth = top5_ask_notional * self.config.max_top5_depth_pct;

        // 3. Size order clipping
        let target_notional = allocated_budget_usdc.min(max_safe_notional_from_depth);

        if target_notional < self.config.min_order_notional_usdc {
            return Err(ExecutionError::OrderSizeBelowMinimum {
                size_usdc: target_notional,
                min_size: self.config.min_order_notional_usdc,
            });
        }

        // Shares calculation
        let shares = (target_notional / limit_price).round_dp(2);
        let actual_notional = (shares * limit_price).round_dp(2);

        let proposal_id = format!("prop-{}", Uuid::new_v4());
        let proposal = ExecutionOrderProposal {
            proposal_id,
            market_id: consensus.market_id.clone(),
            token_id: consensus.token_id.clone(),
            outcome: consensus.outcome.clone(),
            side: OrderSide::Buy,
            limit_price,
            shares,
            notional_usdc: actual_notional,
            ttl_seconds: self.config.default_ttl_seconds,
            reason: format!(
                "Consensus Buy: {} wallets, VWAP ${:.4}, Top-5 Depth Cap ${:.2}",
                consensus.contributing_wallets.len(),
                consensus.target_vwap,
                max_safe_notional_from_depth
            ),
            created_at: now,
        };

        info!(
            market_id = %proposal.market_id,
            token_id = %proposal.token_id,
            limit_price = %proposal.limit_price,
            shares = %proposal.shares,
            notional = %proposal.notional_usdc,
            ttl = proposal.ttl_seconds,
            "Generated safe limit order proposal"
        );

        Ok(proposal)
    }

    /// Registers placed order into active lifecycle tracker.
    pub fn register_placed_order(
        &self,
        order_id: String,
        proposal: ExecutionOrderProposal,
        placed_at: DateTime<Utc>,
    ) {
        let ttl_duration = Duration::seconds(proposal.ttl_seconds as i64);
        let expires_at = placed_at + ttl_duration;

        let state = ExecutionOrderState {
            order_id: order_id.clone(),
            proposal,
            status: OrderLifecycleStatus::Active,
            filled_shares: Decimal::ZERO,
            filled_avg_price: Decimal::ZERO,
            placed_at,
            expires_at,
            last_updated_at: placed_at,
        };

        self.active_orders.insert(order_id, state);
    }

    /// Identifies all active orders that have exceeded their TTL (30~60s) and require automatic cancellation.
    pub fn check_ttl_expirations(&self, now: DateTime<Utc>) -> Vec<String> {
        let mut expired_order_ids = Vec::new();

        for mut entry in self.active_orders.iter_mut() {
            if (entry.status == OrderLifecycleStatus::Active
                || entry.status == OrderLifecycleStatus::PartiallyFilled)
                && now >= entry.expires_at
            {
                entry.status = OrderLifecycleStatus::CancelledByTTL;
                entry.last_updated_at = now;
                expired_order_ids.push(entry.key().clone());
                warn!(
                    order_id = %entry.key(),
                    placed_at = %entry.placed_at,
                    expires_at = %entry.expires_at,
                    "Order TTL expired. Marked for immediate cancellation."
                );
            }
        }

        expired_order_ids
    }

    /// Returns a snapshot of a registered order's lifecycle state.
    pub fn get_order_state(&self, order_id: &str) -> Option<ExecutionOrderState> {
        self.active_orders.get(order_id).map(|e| e.value().clone())
    }

    /// Updates fill state of an order.
    ///
    /// Contract: `filled_shares` is the size of THIS fill event (incremental), not
    /// a cumulative total. The engine accumulates it into the order state and keeps
    /// a weighted average fill price. Duplicate delivery of the same fill event
    /// must be de-duplicated by the upstream feed.
    pub fn record_fill(
        &self,
        order_id: &str,
        filled_shares: Decimal,
        avg_price: Decimal,
        now: DateTime<Utc>,
    ) -> Decimal {
        if let Some(mut entry) = self.active_orders.get_mut(order_id) {
            if filled_shares <= Decimal::ZERO || avg_price <= Decimal::ZERO {
                return Decimal::ZERO;
            }
            let prev_filled = entry.filled_shares;
            let remaining = (entry.proposal.shares - prev_filled).max(Decimal::ZERO);
            let accepted_fill = filled_shares.min(remaining);
            if accepted_fill <= Decimal::ZERO {
                warn!(order_id = %order_id, "Duplicate/excess fill ignored");
                return Decimal::ZERO;
            }
            let new_filled = prev_filled + accepted_fill;
            if prev_filled > Decimal::ZERO {
                // Weighted average of old + new fill prices
                let total_cost =
                    (prev_filled * entry.filled_avg_price) + (accepted_fill * avg_price);
                entry.filled_avg_price = total_cost / new_filled;
            } else {
                entry.filled_avg_price = avg_price;
            }
            entry.filled_shares = new_filled;
            entry.last_updated_at = now;
            if new_filled >= entry.proposal.shares {
                entry.status = OrderLifecycleStatus::Filled;
            } else if new_filled > Decimal::ZERO {
                entry.status = OrderLifecycleStatus::PartiallyFilled;
            }
            debug!(order_id = %order_id, filled = %new_filled, status = ?entry.status, "Updated order fill state");
            accepted_fill
        } else {
            Decimal::ZERO
        }
    }

    pub fn finalize_order(&self, order_id: &str, now: DateTime<Utc>) -> Option<String> {
        let mut entry = self.active_orders.get_mut(order_id)?;
        if entry.filled_shares >= entry.proposal.shares {
            entry.status = OrderLifecycleStatus::Filled;
        } else if entry.filled_shares > Decimal::ZERO {
            entry.status = OrderLifecycleStatus::PartiallyFilled;
        } else {
            entry.status = OrderLifecycleStatus::CancelledByTTL;
        }
        entry.last_updated_at = now;
        Some(entry.proposal.proposal_id.clone())
    }
}
