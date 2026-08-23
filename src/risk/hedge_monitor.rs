use alloy::signers::local::LocalSigner;
use alloy::signers::Signer;
use anyhow::Result;
use dashmap::DashMap;
use polymarket_client_sdk::clob::ws::types::response::BookUpdate;
use polymarket_client_sdk::types::{Decimal, U256};
use polymarket_client_sdk_v2::clob::types::{OrderStatusType, OrderType, Side};
use polymarket_client_sdk_v2::POLYGON;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal_macros::dec;
use std::str::FromStr;
use std::sync::Arc;
use tracing::{debug, error, info, warn};

use super::owned_orders::{FillObservation, OrderPurpose, OwnedOrder, OwnedOrderRegistry};
use super::positions::PositionTracker;
use super::recovery::RecoveryAction;
use super::state_journal::{JournalEvent, JournalRecoverySnapshot, JournalState, StateJournal};
use crate::monitor::orderbook::best_bid;
use crate::trading::executor::outcome_shares_filled;
use crate::trading::AuthenticatedClobClient;

#[derive(Debug, Clone)]
pub struct HedgePosition {
    pub token_id: U256,
    pub opposite_token_id: U256, // Opposite side token_id (for diff calc)
    pub target_amount: Decimal,
    pub entry_price: Decimal,       // Buy price (best ask)
    pub take_profit_price: Decimal, // Take-profit price
    pub stop_loss_price: Decimal,   // Stop-loss price
    pub market_display: String,     // Market display name (e.g. "btc prediction")
    pub pair_id: String,
    pub order_id: Option<String>,     // GTC order ID if placed
    pub pending_sell_amount: Decimal, // Pending sell amount
    pub accounted_sell_fill: Decimal, // Cumulative fill already applied locally
    close_state: HedgeCloseState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HedgeCloseState {
    Open,
    Closing,
    Resting,
    Failed,
}

impl HedgeCloseState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closing => "closing",
            Self::Resting => "resting",
            Self::Failed => "failed",
        }
    }

    fn restore(value: &str, order_id: Option<&str>) -> Self {
        match value {
            "closing" | "failed" => Self::Failed,
            "resting" => Self::Resting,
            _ if order_id.is_some() => Self::Resting,
            _ => Self::Open,
        }
    }
}

pub struct HedgeMonitor {
    client: AuthenticatedClobClient,
    private_key: String,
    positions: DashMap<String, HedgePosition>, // pair_id -> position
    position_tracker: Arc<PositionTracker>,    // For exposure updates
    journal: Arc<StateJournal>,
    owned_orders: Arc<OwnedOrderRegistry>,
}

impl HedgeMonitor {
    pub fn new(
        client: AuthenticatedClobClient,
        private_key: String,
        position_tracker: Arc<PositionTracker>,
        journal: Arc<StateJournal>,
        owned_orders: Arc<OwnedOrderRegistry>,
    ) -> Self {
        Self {
            client,
            private_key,
            positions: DashMap::new(),
            position_tracker,
            journal,
            owned_orders,
        }
    }

    /// Add hedge position to monitor
    pub fn add_position(&self, action: &RecoveryAction) -> Result<()> {
        if let RecoveryAction::MonitorForExit {
            token_id,
            opposite_token_id,
            amount,
            entry_price,
            take_profit_pct,
            stop_loss_pct,
            pair_id,
            market_display,
        } = action
        {
            // Compute take-profit and stop-loss prices
            let take_profit_price = *entry_price * (dec!(1.0) + *take_profit_pct);
            let stop_loss_price = *entry_price * (dec!(1.0) - *stop_loss_pct);

            info!(
                "🛡️ Start hedge monitor | market:{} | pos:{} | entry:{:.4} | TP:{:.4} | SL:{:.4}",
                market_display, amount, entry_price, take_profit_price, stop_loss_price
            );

            let position = HedgePosition {
                token_id: *token_id,
                opposite_token_id: *opposite_token_id,
                target_amount: *amount,
                entry_price: *entry_price,
                take_profit_price,
                stop_loss_price,
                market_display: market_display.clone(),
                pair_id: pair_id.clone(),
                order_id: None,
                pending_sell_amount: dec!(0),
                accounted_sell_fill: dec!(0),
                close_state: HedgeCloseState::Open,
            };

            self.positions.insert(pair_id.clone(), position.clone());
            append_recovery_event(
                &self.journal,
                &position,
                JournalState::Recovering,
                "recovery monitor registered",
            );
        }
        Ok(())
    }

    /// Restore active recovery monitors, including any resting sell order, from
    /// the latest durable journal records.
    pub fn restore_from_journal(&self) -> Result<usize> {
        let mut restored = 0;
        for event in self.journal.unresolved()? {
            let Some(snapshot) = event.recovery else {
                continue;
            };
            let close_state =
                HedgeCloseState::restore(&snapshot.close_state, snapshot.order_id.as_deref());
            let position = HedgePosition {
                token_id: U256::from_str(&snapshot.token_id)?,
                opposite_token_id: U256::from_str(&snapshot.opposite_token_id)?,
                target_amount: Decimal::from_str(&snapshot.target_amount)?,
                entry_price: Decimal::from_str(&snapshot.entry_price)?,
                take_profit_price: Decimal::from_str(&snapshot.take_profit_price)?,
                stop_loss_price: Decimal::from_str(&snapshot.stop_loss_price)?,
                market_display: snapshot.market_display,
                pair_id: event.pair_id.clone(),
                order_id: snapshot.order_id,
                pending_sell_amount: Decimal::from_str(&snapshot.pending_sell_amount)?,
                accounted_sell_fill: Decimal::from_str(&snapshot.accounted_sell_fill)?,
                close_state,
            };
            if let Some(order_id) = position.order_id.clone() {
                self.owned_orders.register(OwnedOrder {
                    order_id,
                    pair_id: position.pair_id.clone(),
                    token_id: position.token_id,
                    purpose: OrderPurpose::RecoverySell,
                    price: position.entry_price,
                    original_size: position.accounted_sell_fill + position.pending_sell_amount,
                    accounted_fill: position.accounted_sell_fill,
                });
            }
            self.positions.insert(event.pair_id, position);
            restored += 1;
        }
        Ok(restored)
    }

    pub fn monitored_tokens(&self) -> Vec<U256> {
        self.positions
            .iter()
            .map(|entry| entry.value().token_id)
            .collect()
    }

    pub async fn reconcile_owned_orders(&self) {
        for owned in self.owned_orders.by_purpose(OrderPurpose::RecoverySell) {
            if !self.journal.pair_is_unresolved(&owned.pair_id) {
                continue;
            }
            let Some(position) = self
                .positions
                .get(&owned.pair_id)
                .map(|entry| entry.clone())
            else {
                continue;
            };
            match self.client.order(&owned.order_id).await {
                Ok(order) => {
                    let delta = match self
                        .owned_orders
                        .observe_cumulative(&owned.order_id, order.size_matched)
                    {
                        FillObservation::Applied { delta, .. } => delta,
                        _ => dec!(0),
                    };
                    self.position_tracker
                        .record_sell_fill(position.token_id, delta);
                    let remaining = (order.original_size - order.size_matched).max(dec!(0));
                    if remaining.is_zero() || matches!(order.status, OrderStatusType::Matched) {
                        self.positions.remove(&owned.pair_id);
                        self.record_recovered(
                            &position,
                            "post-disconnect recovery-order reconciliation completed",
                        );
                    } else if let Some(mut stored) = self.positions.get_mut(&owned.pair_id) {
                        stored.pending_sell_amount = remaining;
                        stored.accounted_sell_fill = order.size_matched;
                        if matches!(order.status, OrderStatusType::Canceled) {
                            stored.order_id = None;
                            stored.close_state = HedgeCloseState::Open;
                        } else {
                            stored.close_state = HedgeCloseState::Resting;
                        }
                        append_recovery_event(
                            &self.journal,
                            &stored,
                            JournalState::Recovering,
                            "post-disconnect recovery order refreshed",
                        );
                    }
                }
                Err(e) => {
                    warn!(order_id = %owned.order_id, error = %e, "Failed to reconcile recovery order after disconnect")
                }
            }
        }
    }

    /// Check orderbook; sell if take-profit or stop-loss hit
    pub async fn check_and_execute(&self, book: &BookUpdate) -> Result<()> {
        // Find positions to check
        let positions_to_check: Vec<(String, HedgePosition)> = self
            .positions
            .iter()
            .filter(|entry| entry.value().token_id == book.asset_id)
            .map(|entry| (entry.key().clone(), entry.value().clone()))
            .collect();

        for (pair_id, position) in positions_to_check {
            // Closing is acquired atomically below. A failed/ambiguous write is
            // deliberately fail-closed until restart/manual reconciliation.
            if matches!(
                position.close_state,
                HedgeCloseState::Closing | HedgeCloseState::Failed
            ) {
                continue;
            }
            // A late fill on the missing buy leg can remove the imbalance while
            // recovery monitoring is active. Stop before considering another sell.
            let current_position = self.position_tracker.get_position(position.token_id);
            let opposite_position = self
                .position_tracker
                .get_position(position.opposite_token_id);
            let difference = current_position - opposite_position;
            if difference <= dec!(0) && position.order_id.is_none() {
                info!(
                    "⏸️ No sell needed | market:{} | pos:{} | opposite:{} | diff:{} | opposite covers",
                    position.market_display, current_position, opposite_position, difference
                );
                self.positions.remove(&pair_id);
                self.record_recovered(&position, "opposite position covers imbalance");
                continue;
            }

            // Reconcile a previously posted recovery order before deciding whether
            // to submit anything else. The CLOB endpoint reports cumulative fills.
            if let Some(ref order_id) = position.order_id {
                match self.client.order(order_id).await {
                    Ok(order) => {
                        let delta = match self
                            .owned_orders
                            .observe_cumulative(order_id, order.size_matched)
                        {
                            FillObservation::Applied { delta, .. } => delta,
                            FillObservation::DuplicateOrOutOfOrder => dec!(0),
                            FillObservation::UnknownOrder => {
                                self.owned_orders.register(OwnedOrder {
                                    order_id: order_id.clone(),
                                    pair_id: position.pair_id.clone(),
                                    token_id: position.token_id,
                                    purpose: OrderPurpose::RecoverySell,
                                    price: position.entry_price,
                                    original_size: order.original_size,
                                    accounted_fill: position.accounted_sell_fill,
                                });
                                match self
                                    .owned_orders
                                    .observe_cumulative(order_id, order.size_matched)
                                {
                                    FillObservation::Applied { delta, .. } => delta,
                                    _ => dec!(0),
                                }
                            }
                        };
                        self.position_tracker
                            .record_sell_fill(position.token_id, delta);
                        let remaining = (order.original_size - order.size_matched).max(dec!(0));
                        if remaining.is_zero() || matches!(order.status, OrderStatusType::Matched) {
                            self.positions.remove(&pair_id);
                            self.record_recovered(
                                &position,
                                &format!(
                                    "recovery sell cumulatively filled {}",
                                    order.size_matched
                                ),
                            );
                        } else if matches!(order.status, OrderStatusType::Canceled) {
                            if let Some(mut stored) = self.positions.get_mut(&pair_id) {
                                stored.order_id = None;
                                stored.close_state = HedgeCloseState::Open;
                                stored.pending_sell_amount = remaining;
                                stored.accounted_sell_fill = order.size_matched;
                                append_recovery_event(
                                    &self.journal,
                                    &stored,
                                    JournalState::Recovering,
                                    "recovery sell canceled; retry enabled",
                                );
                            }
                            info!(order_id, remaining = %remaining, "Recovery sell was canceled; retry is enabled");
                        } else if let Some(mut stored) = self.positions.get_mut(&pair_id) {
                            stored.close_state = HedgeCloseState::Resting;
                            stored.pending_sell_amount = remaining;
                            stored.accounted_sell_fill = order.size_matched;
                            append_recovery_event(
                                &self.journal,
                                &stored,
                                JournalState::Recovering,
                                "recovery sell cumulative fill refreshed",
                            );
                        }
                    }
                    Err(e) => {
                        warn!(order_id, error = %e, "Failed to reconcile recovery sell order")
                    }
                }
                debug!(
                    order_id,
                    pending = %position.pending_sell_amount,
                    "Recovery sell already resting; refusing to create a duplicate"
                );
                continue;
            }

            let Some(best_bid) = best_bid(book) else {
                continue;
            };
            let best_bid_price = best_bid.price;

            // Check take-profit or stop-loss
            let (should_sell, reason) = if best_bid_price >= position.take_profit_price {
                let profit_pct = ((best_bid_price - position.entry_price) / position.entry_price
                    * dec!(100.0))
                .to_f64()
                .unwrap_or(0.0);
                (true, format!("Take-profit ({:.2}%)", profit_pct))
            } else if best_bid_price <= position.stop_loss_price {
                let loss_pct = ((position.entry_price - best_bid_price) / position.entry_price
                    * dec!(100.0))
                .to_f64()
                .unwrap_or(0.0);
                (true, format!("Stop-loss ({:.2}%)", loss_pct))
            } else {
                (false, String::new())
            };

            if should_sell {
                // Determine sell amount
                let sell_amount =
                    if position.order_id.is_some() && position.pending_sell_amount > dec!(0) {
                        // Use pending_sell_amount if unfilled order exists
                        position.pending_sell_amount
                    } else {
                        // Else use diff
                        difference
                    };

                // Diff > 0; sell diff amount
                info!(
                    "✅ {} hit | market:{} | bid:{:.4} | entry:{:.4} | pos:{} | opposite:{} | diff:{} | sell:{}",
                    reason,
                    position.market_display,
                    best_bid_price,
                    position.entry_price,
                    current_position,
                    opposite_position,
                    difference,
                    sell_amount
                );

                // Sell via GTC order
                // Spawn async to avoid blocking main loop
                // Compare-and-transition while holding the DashMap entry guard.
                // Concurrent book callbacks can observe the same earlier snapshot,
                // but exactly one is allowed to move Open -> Closing.
                let Some(position_clone) = begin_close(&self.positions, &pair_id) else {
                    continue;
                };
                let pair_id_clone = pair_id.clone();
                let position_tracker = self.position_tracker.clone();
                let positions = self.positions.clone();
                let client = self.client.clone();
                let private_key = self.private_key.clone();
                let journal = self.journal.clone();
                let owned_orders = self.owned_orders.clone();

                tokio::spawn(async move {
                    // Recreate signer (cannot use self in spawn)
                    let signer = match LocalSigner::from_str(&private_key) {
                        Ok(s) => s.with_chain_id(Some(POLYGON)),
                        Err(e) => {
                            error!(
                                "❌ Create signer failed | market:{} | err:{}",
                                position_clone.market_display, e
                            );
                            if let Some(mut pos) = positions.get_mut(&pair_id_clone) {
                                pos.close_state = HedgeCloseState::Open;
                                append_recovery_event(
                                    &journal,
                                    &pos,
                                    JournalState::Recovering,
                                    "recovery signer creation failed before submission; retry enabled",
                                );
                            }
                            return;
                        }
                    };

                    // Execute sell
                    match Self::execute_sell_order(
                        &client,
                        &signer,
                        &position_clone,
                        best_bid_price,
                        sell_amount,
                    )
                    .await
                    {
                        Ok((order_id, filled, remaining)) => {
                            owned_orders.register(OwnedOrder {
                                order_id: order_id.clone(),
                                pair_id: position_clone.pair_id.clone(),
                                token_id: position_clone.token_id,
                                purpose: OrderPurpose::RecoverySell,
                                price: best_bid_price,
                                original_size: filled + remaining,
                                accounted_fill: filled,
                            });
                            // Update position, mark order placed
                            let order_id_short: String = order_id.chars().take(16).collect();
                            if let Some((_, mut pos)) = positions.remove(&pair_id_clone) {
                                if remaining > dec!(0) {
                                    // Partial fill; save order_id
                                    pos.order_id = Some(order_id);
                                    pos.close_state = HedgeCloseState::Resting;
                                    pos.pending_sell_amount = remaining;
                                    pos.accounted_sell_fill = filled;
                                    append_recovery_event(
                                        &journal,
                                        &pos,
                                        JournalState::Recovering,
                                        "recovery sell order is resting",
                                    );
                                    info!("🔒 Position order_id updated | market:{} | id:{} | remain:{}",
                                        position_clone.market_display, order_id_short, remaining);
                                    positions.insert(pair_id_clone.clone(), pos);
                                } else {
                                    info!("✅ Sell order fully filled | market:{} | id:{} | filled:{}",
                                        position_clone.market_display, order_id_short, filled);
                                    append_recovery_event(
                                        &journal,
                                        &position_clone,
                                        JournalState::Recovered,
                                        &format!("recovery sell filled {filled}"),
                                    );
                                }
                            } else {
                                warn!("⚠️ Position not found | pair_id:{}", pair_id_clone);
                            }

                            // Only update position and exposure for actual fills
                            if filled > dec!(0) {
                                info!(
                                    "📊 Updating position | market:{} | reduce:{}",
                                    position_clone.market_display, filled
                                );
                                position_tracker.record_sell_fill(position_clone.token_id, filled);
                                info!(
                                    "📊 Position update done | market:{}",
                                    position_clone.market_display
                                );

                                // Update exposure cost
                                info!(
                                    "💰 Updating exposure | market:{} | entry:{} | sell:{}",
                                    position_clone.market_display,
                                    position_clone.entry_price,
                                    filled
                                );
                                info!(
                                    "💰 Exposure update done | market:{}",
                                    position_clone.market_display
                                );

                                // Compute exposure
                                let current_exposure = position_tracker.calculate_exposure();
                                info!(
                                    "📉 Exposure updated | market:{} | sold:{} | exposure:{:.2} USD",
                                    position_clone.market_display,
                                    filled,
                                    current_exposure
                                );
                            }
                        }
                        Err(e) => {
                            error!(
                                "❌ Sell order failed | market:{} | price:{:.4} | err:{}",
                                position_clone.market_display, best_bid_price, e
                            );
                            // A transport error after submission can be ambiguous:
                            // retrying could duplicate the sell. Persist Failed and
                            // require reconciliation/restart before another attempt.
                            if let Some(mut pos) = positions.get_mut(&pair_id_clone) {
                                pos.close_state = HedgeCloseState::Failed;
                                append_recovery_event(
                                    &journal,
                                    &pos,
                                    JournalState::ManualReview,
                                    "recovery sell submission failed or is ambiguous; automatic retry blocked",
                                );
                            }
                        }
                    }
                });
            }
        }

        Ok(())
    }

    fn record_recovered(&self, position: &HedgePosition, detail: &str) {
        append_recovery_event(&self.journal, position, JournalState::Recovered, detail);
    }

    /// Static: execute sell order
    async fn execute_sell_order(
        client: &AuthenticatedClobClient,
        signer: &impl Signer<alloy::primitives::Signature>,
        position: &HedgePosition,
        price: Decimal,
        size: Decimal,
    ) -> Result<(String, Decimal, Decimal)> {
        // Fees affect proceeds, not the number of outcome shares available to sell.
        let floored_size = (size * dec!(100.0)).floor() / dec!(100.0);
        let order_size = if floored_size.is_zero() {
            dec!(0.01)
        } else {
            floored_size
        };

        info!(
            "💰 Sell amount | market:{} | base:{:.2} | entry:{:.4} | order:{:.2}",
            position.market_display, size, position.entry_price, order_size
        );

        // Build GTC sell order
        let sell_order = client
            .limit_order()
            .token_id(position.token_id)
            .side(Side::Sell)
            .price(price)
            .size(order_size)
            .order_type(OrderType::GTC)
            .build()
            .await?;

        // Sign order
        let signed_order = client.sign(signer, sell_order).await?;

        // Submit order
        let result = client.post_order(signed_order).await?;

        if !result.success {
            let error_msg = result.error_msg.as_deref().unwrap_or("unknown error");
            return Err(anyhow::anyhow!("GTC sell order failed: {}", error_msg));
        }

        // For a SELL order, outcome shares are the maker asset. The response's
        // taking_amount is sale proceeds and must not be treated as share fill.
        let filled = outcome_shares_filled(&Side::Sell, result.making_amount, result.taking_amount);
        let remaining = order_size - filled;

        if filled > dec!(0) {
            info!(
                "💰 Sell order partial fill | market:{} | id:{} | filled:{} | remain:{}",
                position.market_display,
                result.order_id.chars().take(16).collect::<String>(),
                filled,
                remaining
            );
        } else {
            info!(
                "📋 Sell order posted (no immediate fill) | market:{} | id:{} | size:{} | price:{:.4}",
                position.market_display,
                result.order_id.chars().take(16).collect::<String>(),
                order_size,
                price
            );
        }

        Ok((result.order_id, filled, remaining))
    }
}

fn begin_close(positions: &DashMap<String, HedgePosition>, pair_id: &str) -> Option<HedgePosition> {
    let mut position = positions.get_mut(pair_id)?;
    if position.close_state != HedgeCloseState::Open || position.order_id.is_some() {
        return None;
    }
    position.close_state = HedgeCloseState::Closing;
    Some(position.clone())
}

fn append_recovery_event(
    journal: &StateJournal,
    position: &HedgePosition,
    state: JournalState,
    detail: &str,
) {
    let previous = journal.latest_event(&position.pair_id).ok().flatten();
    let pair = previous.as_ref().and_then(|event| event.pair.clone());
    let event = JournalEvent {
        timestamp: chrono::Utc::now().to_rfc3339(),
        pair_id: position.pair_id.clone(),
        market_id: position.market_display.clone(),
        state,
        yes_filled: previous
            .as_ref()
            .map(|event| event.yes_filled.clone())
            .unwrap_or_default(),
        no_filled: previous
            .as_ref()
            .map(|event| event.no_filled.clone())
            .unwrap_or_default(),
        detail: Some(detail.to_string()),
        pair,
        recovery: Some(JournalRecoverySnapshot {
            token_id: position.token_id.to_string(),
            opposite_token_id: position.opposite_token_id.to_string(),
            target_amount: position.target_amount.to_string(),
            entry_price: position.entry_price.to_string(),
            take_profit_price: position.take_profit_price.to_string(),
            stop_loss_price: position.stop_loss_price.to_string(),
            market_display: position.market_display.clone(),
            order_id: position.order_id.clone(),
            pending_sell_amount: position.pending_sell_amount.to_string(),
            accounted_sell_fill: position.accounted_sell_fill.to_string(),
            close_state: position.close_state.as_str().to_string(),
        }),
    };
    if let Err(e) = journal.append(&event) {
        error!(error = %e, pair_id = %position.pair_id, "Failed to persist recovery state");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_position() -> HedgePosition {
        HedgePosition {
            token_id: U256::from(1),
            opposite_token_id: U256::from(2),
            target_amount: dec!(2),
            entry_price: dec!(0.5),
            take_profit_price: dec!(0.55),
            stop_loss_price: dec!(0.45),
            market_display: "test".into(),
            pair_id: "pair".into(),
            order_id: None,
            pending_sell_amount: dec!(0),
            accounted_sell_fill: dec!(0),
            close_state: HedgeCloseState::Open,
        }
    }

    #[test]
    fn close_transition_can_only_be_acquired_once() {
        let positions = DashMap::new();
        positions.insert("pair".into(), open_position());

        assert!(begin_close(&positions, "pair").is_some());
        assert!(begin_close(&positions, "pair").is_none());
        assert_eq!(
            positions.get("pair").unwrap().close_state,
            HedgeCloseState::Closing
        );
    }

    #[test]
    fn ambiguous_close_is_restored_fail_closed() {
        assert_eq!(
            HedgeCloseState::restore("closing", None),
            HedgeCloseState::Failed
        );
        assert_eq!(
            HedgeCloseState::restore("failed", None),
            HedgeCloseState::Failed
        );
        assert_eq!(
            HedgeCloseState::restore("", Some("order")),
            HedgeCloseState::Resting
        );
    }
}
