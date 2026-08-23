use anyhow::Result;
use dashmap::DashMap;
use polymarket_client_sdk::types::{Decimal, U256};
use rust_decimal_macros::dec;
use std::sync::{Arc, Mutex};
use tracing::{info, trace};

use polypulse::positions::{get_positions, Position};

pub struct PositionTracker {
    positions: DashMap<U256, Decimal>, // token_id -> amount (pos=long, neg=short)
    exposure_costs: DashMap<U256, Decimal>, // token_id -> cost (USD) for risk exposure
    pending_costs: DashMap<String, Decimal>, // reservation_id -> planned order cost
    reservation_lock: Mutex<()>,
    ledger_lock: Mutex<()>,
    max_exposure: Decimal,
}

pub struct ExposureReservation {
    tracker: Arc<PositionTracker>,
    reservation_id: Option<String>,
}

impl Drop for ExposureReservation {
    fn drop(&mut self) {
        if let Some(reservation_id) = self.reservation_id.take() {
            self.tracker.pending_costs.remove(&reservation_id);
        }
    }
}

impl PositionTracker {
    pub fn new(max_exposure: Decimal) -> Self {
        Self {
            positions: DashMap::new(),
            exposure_costs: DashMap::new(),
            pending_costs: DashMap::new(),
            reservation_lock: Mutex::new(()),
            ledger_lock: Mutex::new(()),
            max_exposure,
        }
    }

    fn update_position_unlocked(&self, token_id: U256, delta: Decimal) {
        trace!(
            "update_position: start | token_id:{} | delta:{}",
            token_id,
            delta
        );

        trace!("update_position: acquiring positions write lock");
        let mut entry = self.positions.entry(token_id).or_insert(dec!(0));
        trace!("update_position: positions write lock acquired");
        *entry += delta;
        trace!("update_position: position updated, new value:{}", *entry);

        // Clean up if position goes to ~0
        // Key fix: release positions write lock before accessing exposure_costs to avoid deadlock
        let should_remove = entry.abs() < dec!(0.0001);
        trace!("update_position: should_remove:{}", should_remove);
        if should_remove {
            *entry = dec!(0);
            trace!("update_position: position zeroed");
        }
        drop(entry);
        trace!("update_position: positions write lock released");

        // Now safe to access exposure_costs
        if should_remove {
            trace!("update_position: removing exposure_costs");
            self.exposure_costs.remove(&token_id);
            trace!("update_position: exposure_costs removed");
        }

        trace!("update_position: done");
    }

    fn update_exposure_cost_unlocked(&self, token_id: U256, price: Decimal, delta: Decimal) {
        trace!(
            "update_exposure_cost: start | token_id:{} | price:{} | delta:{}",
            token_id,
            price,
            delta
        );

        if delta == dec!(0) {
            trace!("update_exposure_cost: delta=0, return");
            return;
        }

        trace!("update_exposure_cost: acquiring positions read lock");
        // Key fix: get positions read lock first, then exposure_costs write lock to avoid deadlock
        let current_pos = if delta < dec!(0) {
            trace!("update_exposure_cost: sell, getting positions read lock");
            let pos = self.positions.get(&token_id);
            trace!("update_exposure_cost: positions read lock acquired");
            let result = pos.map(|v| *v.value()).unwrap_or(dec!(0));
            trace!(
                "update_exposure_cost: positions read released, current_pos:{}",
                result
            );
            result
        } else {
            trace!("update_exposure_cost: buy, no positions read needed");
            dec!(0)
        };

        trace!("update_exposure_cost: acquiring exposure_costs write lock");
        let mut entry = self.exposure_costs.entry(token_id).or_insert(dec!(0));
        trace!("update_exposure_cost: exposure_costs write lock acquired");

        if delta > dec!(0) {
            trace!("update_exposure_cost: buy branch, compute cost_delta");
            let cost_delta = price * delta;
            *entry += cost_delta;
            trace!("update_exposure_cost: buy done, new cost:{}", *entry);
        } else {
            trace!(
                "update_exposure_cost: sell branch, current_pos:{}",
                current_pos
            );
            if current_pos > dec!(0) {
                trace!("update_exposure_cost: compute sell ratio");
                let sell_amount = (-delta).min(current_pos);
                let reduction_ratio = sell_amount / current_pos;
                trace!(
                    "update_exposure_cost: sell_amount:{} | reduction_ratio:{} | current cost:{}",
                    sell_amount,
                    reduction_ratio,
                    *entry
                );
                *entry = (*entry * (dec!(1) - reduction_ratio)).max(dec!(0));
                trace!("update_exposure_cost: sell done, new cost:{}", *entry);
            } else {
                trace!("update_exposure_cost: current_pos=0, zero out");
                *entry = dec!(0);
            }
        }

        trace!(
            "update_exposure_cost: check cleanup, current cost:{}",
            *entry
        );
        if *entry < dec!(0.01) {
            trace!("update_exposure_cost: cost near 0, cleanup");
            *entry = dec!(0);
            drop(entry);
            trace!("update_exposure_cost: lock released, removing");
            self.exposure_costs.remove(&token_id);
            trace!("update_exposure_cost: remove done");
        } else {
            trace!("update_exposure_cost: cost nonzero, keep entry");
            drop(entry);
        }

        trace!("update_exposure_cost: done");
    }

    pub fn record_buy_fill(&self, token_id: U256, price: Decimal, shares: Decimal) {
        if shares <= dec!(0) {
            return;
        }
        let _ledger = self
            .ledger_lock
            .lock()
            .expect("position ledger lock poisoned");
        self.update_position_unlocked(token_id, shares);
        self.update_exposure_cost_unlocked(token_id, price, shares);
    }

    pub fn record_sell_fill(&self, token_id: U256, shares: Decimal) {
        if shares <= dec!(0) {
            return;
        }
        let _ledger = self
            .ledger_lock
            .lock()
            .expect("position ledger lock poisoned");
        let current = self
            .positions
            .get(&token_id)
            .map(|entry| *entry.value())
            .unwrap_or(dec!(0));
        let applied = shares.min(current.max(dec!(0)));
        if applied <= dec!(0) {
            return;
        }
        self.update_exposure_cost_unlocked(token_id, dec!(0), -applied);
        self.update_position_unlocked(token_id, -applied);
    }

    pub fn record_merge(&self, yes_token: U256, no_token: U256, shares: Decimal) {
        if shares <= dec!(0) {
            return;
        }
        let _ledger = self
            .ledger_lock
            .lock()
            .expect("position ledger lock poisoned");
        let yes_position = self
            .positions
            .get(&yes_token)
            .map(|entry| *entry.value())
            .unwrap_or(dec!(0));
        let no_position = self
            .positions
            .get(&no_token)
            .map(|entry| *entry.value())
            .unwrap_or(dec!(0));
        let applied = shares.min(yes_position).min(no_position).max(dec!(0));
        if applied <= dec!(0) {
            return;
        }
        self.update_exposure_cost_unlocked(yes_token, dec!(0), -applied);
        self.update_exposure_cost_unlocked(no_token, dec!(0), -applied);
        self.update_position_unlocked(yes_token, -applied);
        self.update_position_unlocked(no_token, -applied);
    }

    /// Get max risk exposure limit
    pub fn max_exposure(&self) -> Decimal {
        self.max_exposure
    }

    pub fn get_position(&self, token_id: U256) -> Decimal {
        let _ledger = self
            .ledger_lock
            .lock()
            .expect("position ledger lock poisoned");
        self.positions
            .get(&token_id)
            .map(|v| *v.value())
            .unwrap_or(dec!(0))
    }

    /// Compute total risk exposure (USD), sum of all position costs
    pub fn calculate_exposure(&self) -> Decimal {
        let _ledger = self
            .ledger_lock
            .lock()
            .expect("position ledger lock poisoned");
        // Sum all position costs; collect to Vec to avoid holding lock long
        let costs: Vec<Decimal> = self
            .exposure_costs
            .iter()
            .map(|entry| *entry.value())
            .collect();
        let pending: Decimal = self.pending_costs.iter().map(|entry| *entry.value()).sum();
        costs.iter().sum::<Decimal>() + pending
    }

    /// Atomically reserve risk capacity. The returned guard releases capacity on
    /// every exit path, including task cancellation or panic unwinding.
    pub fn reserve(
        self: &Arc<Self>,
        reservation_id: String,
        cost: Decimal,
    ) -> Option<ExposureReservation> {
        let _guard = self
            .reservation_lock
            .lock()
            .expect("reservation lock poisoned");
        if cost <= dec!(0) || self.calculate_exposure() + cost > self.max_exposure {
            return None;
        }
        self.pending_costs.insert(reservation_id.clone(), cost);
        Some(ExposureReservation {
            tracker: self.clone(),
            reservation_id: Some(reservation_id),
        })
    }

    /// Replace the account-wide notional of active buy orders observed from CLOB.
    pub fn set_active_order_exposure(&self, cost: Decimal) {
        const KEY: &str = "__active_clob_orders__";
        if cost <= dec!(0) {
            self.pending_costs.remove(KEY);
        } else {
            self.pending_costs.insert(KEY.to_string(), cost);
        }
    }

    /// Get YES and NO positions
    pub fn get_pair_positions(&self, yes_token: U256, no_token: U256) -> (Decimal, Decimal) {
        (self.get_position(yes_token), self.get_position(no_token))
    }

    /// Sync positions from Data API, fully overwrite local cache
    /// For scheduled sync; ensures local matches on-chain positions
    pub async fn sync_from_api(&self) -> Result<Vec<Position>> {
        use polymarket_client_sdk::types::B256;
        use std::collections::HashMap;

        let positions = get_positions().await?;

        let _ledger = self
            .ledger_lock
            .lock()
            .expect("position ledger lock poisoned");

        // Reconcile both positions and cost exposure with the account-wide API
        // view. Data API exposes initialValue/avgPrice; using $1 per share would
        // double-count a normal YES+NO pair and spuriously trip the risk limit.
        // Pending order reservations remain separate.
        self.positions.clear();
        self.exposure_costs.clear();

        // Update local cache from API positions
        let mut updated_count = 0;
        let mut valid_positions = Vec::new();

        for pos in positions {
            if pos.size > dec!(0) {
                // Position.asset is token_id
                self.positions.insert(pos.asset, pos.size);
                self.exposure_costs
                    .insert(pos.asset, api_position_cost(&pos));
                valid_positions.push(pos);
                updated_count += 1;
            }
        }

        // Print positions grouped by market
        if !valid_positions.is_empty() {
            let mut by_market: HashMap<B256, Vec<&Position>> = HashMap::new();
            for pos in &valid_positions {
                by_market.entry(pos.condition_id).or_default().push(pos);
            }

            info!(
                "📊 Position sync done | {} positions, {} markets",
                updated_count,
                by_market.len()
            );

            // Print one line per market
            for (_condition_id, market_positions) in by_market.iter() {
                let mut yes_pos = dec!(0);
                let mut no_pos = dec!(0);
                let mut market_title = "";
                let uses_ctf_indices = market_positions
                    .iter()
                    .any(|position| position.outcome_index == 2);

                for pos in market_positions {
                    if (!uses_ctf_indices && pos.outcome_index == 0)
                        || (uses_ctf_indices && pos.outcome_index == 1)
                    {
                        yes_pos = pos.size;
                    } else if (!uses_ctf_indices && pos.outcome_index == 1)
                        || (uses_ctf_indices && pos.outcome_index == 2)
                    {
                        no_pos = pos.size;
                    }
                    if market_title.is_empty() {
                        market_title = &pos.title;
                    }
                }

                // Truncate long titles
                let title_display = if market_title.len() > 40 {
                    format!("{}...", &market_title[..37])
                } else {
                    market_title.to_string()
                };

                info!("  📈 {} | YES:{} NO:{}", title_display, yes_pos, no_pos);
            }
        } else {
            info!("📊 Position sync done | no positions");
        }

        Ok(valid_positions)
    }
}

fn api_position_cost(position: &Position) -> Decimal {
    if position.initial_value > dec!(0) {
        position.initial_value
    } else if position.avg_price > dec!(0) {
        position.avg_price * position.size
    } else {
        // Fail conservatively only when the API omitted both cost fields.
        position.size
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api_position(asset: &str, size: &str, avg_price: &str, initial_value: &str) -> Position {
        serde_json::from_str(&format!(
            r#"{{
                "proxyWallet":"0x0000000000000000000000000000000000000001",
                "asset":"{asset}",
                "conditionId":"0x0000000000000000000000000000000000000000000000000000000000000007",
                "size":"{size}","avgPrice":"{avg_price}","initialValue":"{initial_value}",
                "currentValue":"0","cashPnl":"0","percentPnl":"0",
                "totalBought":"{size}","realizedPnl":"0","percentRealizedPnl":"0",
                "curPrice":"0","redeemable":false,"mergeable":true,
                "title":"Fixture","slug":"fixture","icon":"","eventSlug":"fixture",
                "eventId":"fixture","outcome":"Up","outcomeIndex":0,
                "oppositeOutcome":"Down","oppositeAsset":"43",
                "endDate":"2026-08-13","negativeRisk":false
            }}"#
        ))
        .unwrap()
    }

    #[test]
    fn reservations_are_bounded_and_releasable() {
        let tracker = Arc::new(PositionTracker::new(dec!(10)));
        let reservation = tracker.reserve("a".into(), dec!(6)).unwrap();
        assert!(tracker.reserve("b".into(), dec!(5)).is_none());
        assert_eq!(tracker.calculate_exposure(), dec!(6));
        drop(reservation);
        assert_eq!(tracker.calculate_exposure(), dec!(0));
    }

    #[test]
    fn actual_fill_cost_survives_reservation_release() {
        let tracker = Arc::new(PositionTracker::new(dec!(10)));
        let token = U256::from(7);
        let reservation = tracker.reserve("a".into(), dec!(4)).unwrap();
        tracker.record_buy_fill(token, dec!(0.4), dec!(5));
        drop(reservation);
        assert_eq!(tracker.calculate_exposure(), dec!(2));
    }

    #[test]
    fn active_order_exposure_is_replaced_not_accumulated() {
        let tracker = PositionTracker::new(dec!(20));
        tracker.set_active_order_exposure(dec!(4));
        tracker.set_active_order_exposure(dec!(3));
        assert_eq!(tracker.calculate_exposure(), dec!(3));
        tracker.set_active_order_exposure(dec!(0));
        assert_eq!(tracker.calculate_exposure(), dec!(0));
    }

    #[test]
    fn api_sync_cost_prefers_initial_value_instead_of_one_dollar_per_share() {
        let yes = api_position("42", "100", "0.45", "45");
        let no = api_position("43", "100", "0.50", "50");
        assert_eq!(api_position_cost(&yes) + api_position_cost(&no), dec!(95));
    }

    #[test]
    fn api_sync_cost_falls_back_to_average_price_then_conservative_notional() {
        let average = api_position("42", "10", "0.40", "0");
        let missing = api_position("43", "10", "0", "0");
        assert_eq!(api_position_cost(&average), dec!(4));
        assert_eq!(api_position_cost(&missing), dec!(10));
    }

    #[test]
    fn partial_sale_reduces_cost_proportionally() {
        let tracker = PositionTracker::new(dec!(20));
        let token = U256::from(8);
        tracker.record_buy_fill(token, dec!(0.4), dec!(10));
        tracker.record_sell_fill(token, dec!(4));
        assert_eq!(tracker.get_position(token), dec!(6));
        assert_eq!(tracker.calculate_exposure(), dec!(2.4));
    }

    #[test]
    fn oversell_is_clamped_without_creating_negative_position() {
        let tracker = PositionTracker::new(dec!(20));
        let token = U256::from(10);
        tracker.record_buy_fill(token, dec!(0.4), dec!(3));
        tracker.record_sell_fill(token, dec!(10));
        assert_eq!(tracker.get_position(token), dec!(0));
        assert_eq!(tracker.calculate_exposure(), dec!(0));
    }

    #[tokio::test]
    async fn reservation_guard_releases_capacity_when_task_is_aborted() {
        let tracker = Arc::new(PositionTracker::new(dec!(10)));
        let reservation = tracker.reserve("abort".into(), dec!(6)).unwrap();
        let task = tokio::spawn(async move {
            let _reservation = reservation;
            std::future::pending::<()>().await;
        });
        tokio::task::yield_now().await;
        assert_eq!(tracker.calculate_exposure(), dec!(6));
        task.abort();
        let _ = task.await;
        assert_eq!(tracker.calculate_exposure(), dec!(0));
    }
}
