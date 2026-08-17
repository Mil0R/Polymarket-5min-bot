use anyhow::Result;
use polymarket_client_sdk::types::{Decimal, U256};
use rust_decimal_macros::dec;
use tracing::debug;

use super::manager::OrderPair;
use super::positions::PositionTracker;

#[derive(Debug, Clone)]
pub enum RecoveryAction {
    None,
    MonitorForExit {
        token_id: U256,
        opposite_token_id: U256, // Opposite side token_id (for diff calc)
        amount: Decimal,
        entry_price: Decimal,     // Buy price (best ask)
        take_profit_pct: Decimal, // Take-profit % (e.g. 0.05 = 5%)
        stop_loss_pct: Decimal,   // Stop-loss % (e.g. 0.05 = 5%)
        pair_id: String,
        market_display: String, // Market display name (e.g. "btc prediction")
    },
    ManualIntervention {
        reason: String,
    },
}

pub struct RecoveryStrategy {
    imbalance_threshold: Decimal,
    take_profit_pct: Decimal, // Take-profit %
    stop_loss_pct: Decimal,   // Stop-loss %
}

impl RecoveryStrategy {
    pub fn new(imbalance_threshold: f64, take_profit_pct: f64, stop_loss_pct: f64) -> Self {
        Self {
            imbalance_threshold: Decimal::try_from(imbalance_threshold).unwrap_or(dec!(0.1)),
            take_profit_pct: Decimal::try_from(take_profit_pct).unwrap_or(dec!(0.05)), // default 5% take-profit
            stop_loss_pct: Decimal::try_from(stop_loss_pct).unwrap_or(dec!(0.05)), // default 5% stop-loss
        }
    }

    /// Handle partial fill (GTC orders); hedge disabled, no action on imbalance
    pub async fn handle_partial_fill(
        &self,
        pair: &OrderPair,
        _position_tracker: &PositionTracker,
    ) -> Result<RecoveryAction> {
        // Compute imbalance amount
        let imbalance = (pair.yes_filled - pair.no_filled).abs();
        let total_filled = pair.yes_filled + pair.no_filled;

        // Compute imbalance ratio
        let imbalance_ratio = if total_filled > dec!(0) {
            imbalance / total_filled
        } else {
            dec!(0)
        };

        if imbalance_ratio > self.imbalance_threshold {
            let (side, token_id, opposite_token_id, amount, entry_price) =
                if pair.yes_filled > pair.no_filled {
                    (
                        "YES",
                        pair.yes_token_id,
                        pair.no_token_id,
                        pair.yes_filled - pair.no_filled,
                        pair.yes_price,
                    )
                } else {
                    (
                        "NO",
                        pair.no_token_id,
                        pair.yes_token_id,
                        pair.no_filled - pair.yes_filled,
                        pair.no_price,
                    )
                };

            debug!(
                pair_id = %pair.pair_id,
                side = side,
                imbalance_amount = %amount,
                imbalance_ratio = %imbalance_ratio,
                "Partial fill imbalance; starting bounded exit monitor"
            );
            return Ok(RecoveryAction::MonitorForExit {
                token_id,
                opposite_token_id,
                amount,
                entry_price,
                take_profit_pct: self.take_profit_pct,
                stop_loss_pct: self.stop_loss_pct,
                pair_id: pair.pair_id.clone(),
                market_display: format!("market {:#x}", pair.market_id),
            });
        }

        Ok(RecoveryAction::None)

        // Legacy: if imbalance > threshold, hedge
        // if imbalance_ratio > self.imbalance_threshold {
        //     let (token_to_sell, amount) = if pair.yes_filled > pair.no_filled {
        //         // YES filled more, sell excess YES
        //         (pair.yes_token_id, pair.yes_filled - pair.no_filled)
        //     } else {
        //         // NO filled more, sell excess NO
        //         (pair.no_token_id, pair.no_filled - pair.yes_filled)
        //     };
        //
        //     info!(
        //         pair_id = %pair.pair_id,
        //         token_id = %token_to_sell,
        //         amount = %amount,
        //         imbalance_ratio = %imbalance_ratio,
        //         "Partial fill imbalance, hedge"
        //     );
        //
        //     return Ok(RecoveryAction::SellExcess {
        //         token_id: token_to_sell.to_string(),
        //         amount,
        //     });
        // }
        //
        // // Imbalance within acceptable range
        // Ok(RecoveryAction::None)
    }

    /// Handle one-sided fill (GTC orders); hedge disabled, no action
    pub async fn handle_one_sided_fill(
        &self,
        pair: &OrderPair,
        _position_tracker: &PositionTracker,
    ) -> Result<RecoveryAction> {
        // Determine which order succeeded, which failed
        let (side, token_id, opposite_token_id, filled_amount, entry_price) =
            if pair.yes_filled > dec!(0) && pair.no_filled == dec!(0) {
                (
                    "YES",
                    pair.yes_token_id,
                    pair.no_token_id,
                    pair.yes_filled,
                    pair.yes_price,
                )
            } else if pair.no_filled > dec!(0) && pair.yes_filled == dec!(0) {
                (
                    "NO",
                    pair.no_token_id,
                    pair.yes_token_id,
                    pair.no_filled,
                    pair.no_price,
                )
            } else {
                return Ok(RecoveryAction::None);
            };

        // Hedge disabled; one-sided fill logged by executor
        debug!(
            "One-sided fill | {} filled {} shares | starting bounded exit monitor",
            side, filled_amount
        );
        Ok(RecoveryAction::MonitorForExit {
            token_id,
            opposite_token_id,
            amount: filled_amount,
            entry_price,
            take_profit_pct: self.take_profit_pct,
            stop_loss_pct: self.stop_loss_pct,
            pair_id: pair.pair_id.clone(),
            market_display: format!("market {:#x}", pair.market_id),
        })

        // Legacy: hedge strategy - monitor best bid, sell on TP/SL
        // // Resolve opposite side token_id
        // let success_token = if pair.yes_filled > dec!(0) {
        //     pair.yes_token_id
        // } else {
        //     pair.no_token_id
        // };
        // let opposite_token = if success_token == pair.yes_token_id {
        //     pair.no_token_id
        // } else {
        //     pair.yes_token_id
        // };
        //
        // Ok(RecoveryAction::MonitorForExit {
        //     token_id: success_token,
        //     opposite_token_id: opposite_token,
        //     amount: filled_amount,
        //     entry_price: dec!(0), // Placeholder, get from orderbook in main
        //     take_profit_pct: self.take_profit_pct,
        //     stop_loss_pct: self.stop_loss_pct,
        //     pair_id: pair.pair_id.clone(),
        //     market_display: "unknown".to_string(), // Placeholder, get from market info in main
        // })
    }
}
