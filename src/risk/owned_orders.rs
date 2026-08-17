//! Process-owned order registry with idempotent cumulative-fill accounting.

use dashmap::DashMap;
use polymarket_client_sdk::types::{Decimal, U256};
use rust_decimal_macros::dec;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderPurpose {
    ArbitrageYes,
    ArbitrageNo,
    RecoverySell,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OwnedOrder {
    pub order_id: String,
    pub pair_id: String,
    pub token_id: U256,
    pub purpose: OrderPurpose,
    pub price: Decimal,
    pub original_size: Decimal,
    pub accounted_fill: Decimal,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FillObservation {
    Applied {
        order: OwnedOrder,
        delta: Decimal,
        cumulative: Decimal,
    },
    DuplicateOrOutOfOrder,
    UnknownOrder,
}

#[derive(Default)]
pub struct OwnedOrderRegistry {
    orders: DashMap<String, OwnedOrder>,
}

impl OwnedOrderRegistry {
    pub fn register(&self, mut order: OwnedOrder) {
        if order.order_id.is_empty() {
            return;
        }
        order.accounted_fill = order.accounted_fill.clamp(dec!(0), order.original_size);
        self.orders
            .entry(order.order_id.clone())
            .and_modify(|existing| {
                existing.accounted_fill = existing.accounted_fill.max(order.accounted_fill);
                existing.original_size = existing.original_size.max(order.original_size);
                existing.price = order.price;
                existing.token_id = order.token_id;
                existing.pair_id.clone_from(&order.pair_id);
                existing.purpose = order.purpose;
            })
            .or_insert(order);
    }

    pub fn observe_cumulative(&self, order_id: &str, cumulative: Decimal) -> FillObservation {
        let Some(mut order) = self.orders.get_mut(order_id) else {
            return FillObservation::UnknownOrder;
        };
        let bounded = cumulative.clamp(dec!(0), order.original_size);
        if bounded <= order.accounted_fill {
            return FillObservation::DuplicateOrOutOfOrder;
        }
        let delta = bounded - order.accounted_fill;
        order.accounted_fill = bounded;
        FillObservation::Applied {
            order: order.clone(),
            delta,
            cumulative: bounded,
        }
    }

    pub fn by_purpose(&self, purpose: OrderPurpose) -> Vec<OwnedOrder> {
        self.orders
            .iter()
            .filter(|entry| entry.value().purpose == purpose)
            .map(|entry| entry.value().clone())
            .collect()
    }

    pub fn all(&self) -> Vec<OwnedOrder> {
        self.orders
            .iter()
            .map(|entry| entry.value().clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::risk::positions::PositionTracker;

    fn order() -> OwnedOrder {
        OwnedOrder {
            order_id: "order-1".into(),
            pair_id: "pair-1".into(),
            token_id: U256::from(1),
            purpose: OrderPurpose::ArbitrageYes,
            price: dec!(0.4),
            original_size: dec!(10),
            accounted_fill: dec!(2),
        }
    }

    #[test]
    fn cumulative_fills_are_idempotent_and_ignore_out_of_order_events() {
        let registry = OwnedOrderRegistry::default();
        registry.register(order());
        assert!(matches!(
            registry.observe_cumulative("order-1", dec!(5)),
            FillObservation::Applied { delta, .. } if delta == dec!(3)
        ));
        assert_eq!(
            registry.observe_cumulative("order-1", dec!(5)),
            FillObservation::DuplicateOrOutOfOrder
        );
        assert_eq!(
            registry.observe_cumulative("order-1", dec!(4)),
            FillObservation::DuplicateOrOutOfOrder
        );
        assert!(matches!(
            registry.observe_cumulative("order-1", dec!(99)),
            FillObservation::Applied { delta, cumulative, .. }
                if delta == dec!(5) && cumulative == dec!(10)
        ));
    }

    #[test]
    fn duplicate_registration_never_rewinds_accounted_fill() {
        let registry = OwnedOrderRegistry::default();
        registry.register(order());
        let mut replayed = order();
        replayed.accounted_fill = dec!(1);
        registry.register(replayed);
        assert!(matches!(
            registry.observe_cumulative("order-1", dec!(3)),
            FillObservation::Applied { delta, .. } if delta == dec!(1)
        ));
    }

    #[test]
    fn duplicate_and_out_of_order_recovery_events_change_ledger_once() {
        let registry = OwnedOrderRegistry::default();
        let tracker = PositionTracker::new(dec!(20));
        let token = U256::from(9);
        tracker.record_buy_fill(token, dec!(0.4), dec!(10));
        registry.register(OwnedOrder {
            order_id: "sell".into(),
            pair_id: "pair".into(),
            token_id: token,
            purpose: OrderPurpose::RecoverySell,
            price: dec!(0.5),
            original_size: dec!(10),
            accounted_fill: dec!(0),
        });

        for cumulative in [dec!(3), dec!(3), dec!(2), dec!(5)] {
            if let FillObservation::Applied { delta, .. } =
                registry.observe_cumulative("sell", cumulative)
            {
                tracker.record_sell_fill(token, delta);
            }
        }
        assert_eq!(tracker.get_position(token), dec!(5));
        assert_eq!(tracker.calculate_exposure(), dec!(2));
    }
}
