use anyhow::Result;
use dashmap::DashMap;
use polymarket_client_sdk::types::{Decimal, B256, U256};
use polymarket_client_sdk_v2::clob::ws::types::response::OrderMessage;
use rust_decimal_macros::dec;
use std::str::FromStr;
use tracing::{debug, error, info, warn};

use super::owned_orders::{FillObservation, OrderPurpose, OwnedOrder, OwnedOrderRegistry};
use super::positions::PositionTracker;
use super::recovery::{RecoveryAction, RecoveryStrategy};
use super::state_journal::{
    JournalEvent, JournalPairSnapshot, JournalRecoverySnapshot, JournalState, StateJournal,
};
use crate::config::Config as BotConfig;
use crate::trading::executor::OrderPairResult;
use crate::trading::OrderStatusReader;

#[derive(Debug, Clone, PartialEq)]
pub enum PairStatus {
    BothFilled,
    PartiallyFilled,
    OneFailed,
    BothFailed,
}

#[derive(Debug, Clone)]
pub struct OrderPair {
    pub pair_id: String,
    pub market_id: B256,
    pub yes_token_id: U256,
    pub no_token_id: U256,
    pub yes_order_id: String,
    pub no_order_id: String,
    pub yes_size: Decimal,
    pub no_size: Decimal,
    pub yes_filled: Decimal,
    pub no_filled: Decimal,
    pub yes_price: Decimal,
    pub no_price: Decimal,
    pub status: PairStatus,
}

pub struct RiskManager {
    pending_pairs: DashMap<String, OrderPair>,
    owned_orders: std::sync::Arc<OwnedOrderRegistry>,
    position_tracker: std::sync::Arc<PositionTracker>,
    recovery_strategy: RecoveryStrategy,
    journal: std::sync::Arc<StateJournal>,
}

impl RiskManager {
    pub fn new(config: &BotConfig) -> Result<Self> {
        let journal = std::sync::Arc::new(StateJournal::open(&config.state_journal_file)?);
        Self::with_journal(config, journal)
    }

    pub fn new_isolated_paper(config: &BotConfig) -> Result<Self> {
        anyhow::ensure!(
            config.dry_run,
            "isolated in-memory risk state requires dry-run"
        );
        Self::with_journal(config, std::sync::Arc::new(StateJournal::in_memory()))
    }

    fn with_journal(config: &BotConfig, journal: std::sync::Arc<StateJournal>) -> Result<Self> {
        let unresolved = journal.unresolved()?;
        if !unresolved.is_empty() {
            warn!(
                count = unresolved.len(),
                "State journal has unresolved pairs; startup reconciliation required"
            );
        }
        Ok(Self {
            pending_pairs: DashMap::new(),
            owned_orders: std::sync::Arc::new(OwnedOrderRegistry::default()),
            position_tracker: std::sync::Arc::new(PositionTracker::new(
                Decimal::try_from(config.arbitrage_budget_usdc).unwrap_or(dec!(1000.0)),
            )),
            recovery_strategy: RecoveryStrategy::new(
                config.risk_imbalance_threshold,
                config.hedge_take_profit_pct,
                config.hedge_stop_loss_pct,
            ),
            journal,
        })
    }

    /// Rebuild owned pairs and refresh cumulative fills before live trading. A
    /// legacy or malformed record cannot be managed safely and still blocks live
    /// startup unless the operator explicitly overrides it.
    pub async fn reconcile_startup(
        &self,
        config: &BotConfig,
        client: &dyn OrderStatusReader,
    ) -> Result<Vec<RecoveryAction>> {
        let unresolved = self.journal.unresolved()?;
        let mut actions = Vec::new();
        let mut unmanaged = Vec::new();
        let account_synced = if unresolved.is_empty() || config.proxy_address.is_none() {
            false
        } else {
            match self.position_tracker.sync_from_api().await {
                Ok(_) => true,
                Err(e) if config.dry_run || config.allow_unresolved_state => {
                    warn!(error = %e, "Startup account position sync failed; using persisted fills");
                    false
                }
                Err(e) => {
                    return Err(e.context(
                        "startup account position sync failed while unresolved risk exists",
                    ));
                }
            }
        };

        for event in unresolved {
            let Some(snapshot) = event.pair.clone() else {
                unmanaged.push(event.pair_id.clone());
                continue;
            };
            let parsed = (|| -> Result<OrderPair> {
                let yes_filled = Decimal::from_str(&event.yes_filled)?;
                let no_filled = Decimal::from_str(&event.no_filled)?;
                let yes_size = Decimal::from_str(&snapshot.yes_size)?;
                let no_size = Decimal::from_str(&snapshot.no_size)?;
                Ok(OrderPair {
                    pair_id: event.pair_id.clone(),
                    market_id: B256::from_str(&snapshot.market_id)?,
                    yes_token_id: U256::from_str(&snapshot.yes_token_id)?,
                    no_token_id: U256::from_str(&snapshot.no_token_id)?,
                    yes_order_id: snapshot.yes_order_id.clone(),
                    no_order_id: snapshot.no_order_id.clone(),
                    yes_size,
                    no_size,
                    yes_filled,
                    no_filled,
                    yes_price: Decimal::from_str(&snapshot.yes_price)?,
                    no_price: Decimal::from_str(&snapshot.no_price)?,
                    status: classify_pair(yes_filled, no_filled, yes_size, no_size),
                })
            })();
            let mut pair = match parsed {
                Ok(pair) => pair,
                Err(e) => {
                    warn!(pair_id = %event.pair_id, error = %e, "Cannot parse persisted pair snapshot");
                    unmanaged.push(event.pair_id.clone());
                    continue;
                }
            };

            let (yes_fill, no_fill) = tokio::join!(
                cumulative_order_fill(client, &pair.yes_order_id, pair.yes_filled),
                cumulative_order_fill(client, &pair.no_order_id, pair.no_filled)
            );
            pair.yes_filled = yes_fill;
            pair.no_filled = no_fill;
            pair.status =
                classify_pair(pair.yes_filled, pair.no_filled, pair.yes_size, pair.no_size);

            self.restore_pair_tracking(&pair, !account_synced);
            if !account_synced {
                if let Some(recovery) = event.recovery.as_ref() {
                    if let (Ok(token), Ok(accounted)) = (
                        U256::from_str(&recovery.token_id),
                        Decimal::from_str(&recovery.accounted_sell_fill),
                    ) {
                        self.position_tracker.record_sell_fill(token, accounted);
                    }
                }
            }
            if matches!(pair.status, PairStatus::BothFilled) {
                self.record_pair(
                    &pair,
                    JournalState::BothFilled,
                    Some("startup reconciliation confirmed both legs".into()),
                );
                continue;
            }
            if pair.yes_filled == pair.no_filled {
                self.record_pair(
                    &pair,
                    JournalState::Recovered,
                    Some("startup reconciliation confirmed a balanced partial pair".into()),
                );
                continue;
            }

            if event.recovery.is_some() {
                self.record_pair_with_recovery(
                    &pair,
                    JournalState::Recovering,
                    Some("startup reconciliation refreshed cumulative fills".into()),
                    event.recovery,
                );
                continue;
            }

            match self.handle_order_pair(&pair.pair_id).await? {
                action @ RecoveryAction::MonitorForExit { .. } => actions.push(action),
                RecoveryAction::None => {}
                RecoveryAction::ManualIntervention { reason } => {
                    warn!(pair_id = %pair.pair_id, reason, "Startup pair needs manual intervention");
                    unmanaged.push(pair.pair_id.clone());
                }
            }
        }

        if !unmanaged.is_empty() && !config.allow_unresolved_state && !config.dry_run {
            anyhow::bail!(
                "{} unresolved pair(s) could not be automatically managed: {}; inspect {} or use DRY_RUN",
                unmanaged.len(),
                unmanaged.join(","),
                config.state_journal_file
            );
        }
        if !unmanaged.is_empty() {
            warn!(
                count = unmanaged.len(),
                dry_run = config.dry_run,
                "Continuing with unmanaged journal pairs by safe mode/override"
            );
        }
        Ok(actions)
    }

    pub fn has_unresolved_state(&self) -> bool {
        self.journal.has_unresolved()
    }

    fn restore_pair_tracking(&self, pair: &OrderPair, restore_position: bool) {
        self.register_pair_orders(pair);
        self.pending_pairs
            .insert(pair.pair_id.clone(), pair.clone());
        if restore_position {
            self.position_tracker.record_buy_fill(
                pair.yes_token_id,
                pair.yes_price,
                pair.yes_filled,
            );
            self.position_tracker
                .record_buy_fill(pair.no_token_id, pair.no_price, pair.no_filled);
        }
    }

    /// Register new order pair
    /// yes_price: YES buy price, no_price: NO buy price
    pub fn register_order_pair(
        &self,
        result: OrderPairResult,
        market_id: B256,
        yes_token: U256,
        no_token: U256,
    ) {
        let status = classify_pair(
            result.yes_filled,
            result.no_filled,
            result.yes_size,
            result.no_size,
        );

        let pair = OrderPair {
            pair_id: result.pair_id.clone(),
            market_id,
            yes_token_id: yes_token,
            no_token_id: no_token,
            yes_order_id: result.yes_order_id.clone(),
            no_order_id: result.no_order_id.clone(),
            yes_size: result.yes_size,
            no_size: result.no_size,
            yes_filled: result.yes_filled,
            no_filled: result.no_filled,
            yes_price: result.yes_price,
            no_price: result.no_price,
            status: status.clone(),
        };

        // Commit only actual fills to positions and exposure. Planned order cost is
        // tracked separately as a reservation and released by the caller.
        self.position_tracker
            .record_buy_fill(yes_token, result.yes_price, pair.yes_filled);
        self.position_tracker
            .record_buy_fill(no_token, result.no_price, pair.no_filled);

        // Log already printed in executor
        debug!(
            pair_id = %pair.pair_id,
            status = ?status,
            yes_filled = %pair.yes_filled,
            no_filled = %pair.no_filled,
            "Register order pair"
        );

        // Use pair.pair_id clone; DashMap needs ownership
        self.record_pair(&pair, journal_state_for_pair(status), None);
        self.register_pair_orders(&pair);
        self.pending_pairs.insert(pair.pair_id.clone(), pair);
    }

    /// Handle order pair and decide recovery strategy
    pub async fn handle_order_pair(&self, pair_id: &str) -> Result<RecoveryAction> {
        let pair = self
            .pending_pairs
            .get(pair_id)
            .ok_or_else(|| anyhow::anyhow!("Order pair {} not found", pair_id))?
            .clone();

        match pair.status {
            PairStatus::BothFilled => {
                info!(pair_id = %pair.pair_id, "Both orders fully filled, no recovery needed");
                Ok(RecoveryAction::None)
            }
            PairStatus::PartiallyFilled => {
                let action = self
                    .recovery_strategy
                    .handle_partial_fill(&pair, &self.position_tracker)
                    .await?;
                if matches!(action, RecoveryAction::MonitorForExit { .. }) {
                    self.record_pair(&pair, JournalState::Recovering, None);
                } else if matches!(action, RecoveryAction::None) {
                    self.record_pair(
                        &pair,
                        JournalState::Recovered,
                        Some("partial imbalance accepted within configured threshold".into()),
                    );
                }
                Ok(action)
            }
            PairStatus::OneFailed => {
                let action = self
                    .recovery_strategy
                    .handle_one_sided_fill(&pair, &self.position_tracker)
                    .await?;
                if matches!(action, RecoveryAction::MonitorForExit { .. }) {
                    self.record_pair(&pair, JournalState::Recovering, None);
                }
                Ok(action)
            }
            PairStatus::BothFailed => {
                error!(
                    "❌ Arbitrage failed | YES and NO orders both unfilled; possible causes: price moved or liquidity insufficient"
                );
                Ok(RecoveryAction::ManualIntervention {
                    reason: "Both orders failed".to_string(),
                })
            }
        }
    }

    /// Get position tracker (Arc reference)
    pub fn position_tracker(&self) -> std::sync::Arc<PositionTracker> {
        self.position_tracker.clone()
    }

    pub fn journal(&self) -> std::sync::Arc<StateJournal> {
        self.journal.clone()
    }

    pub fn owned_orders(&self) -> std::sync::Arc<OwnedOrderRegistry> {
        self.owned_orders.clone()
    }

    /// Reconcile cumulative matched size from the authenticated user WebSocket.
    pub fn observe_order_event(&self, event: &OrderMessage) {
        self.apply_cumulative_fill(
            &event.id,
            event.size_matched.unwrap_or(dec!(0)),
            "user websocket update",
        );
    }

    /// Fill the WebSocket gap after a disconnect before a new connection is
    /// allowed to observe further strategy signals.
    pub async fn reconcile_owned_orders(&self, client: &dyn OrderStatusReader) {
        let orders: Vec<String> = self
            .owned_orders
            .all()
            .into_iter()
            .filter(|order| {
                order.purpose != OrderPurpose::RecoverySell
                    && self.journal.pair_is_unresolved(&order.pair_id)
            })
            .map(|order| order.order_id)
            .collect();
        for order_id in orders {
            match client.cumulative_fill(&order_id).await {
                Ok(cumulative) => self.apply_cumulative_fill(
                    &order_id,
                    cumulative,
                    "post-disconnect HTTP reconciliation",
                ),
                Err(e) => {
                    warn!(order_id, error = %e, "Failed to reconcile owned order after disconnect")
                }
            }
        }
    }

    fn apply_cumulative_fill(&self, order_id: &str, cumulative: Decimal, source: &str) {
        let (owned, delta, cumulative) =
            match self.owned_orders.observe_cumulative(order_id, cumulative) {
                FillObservation::Applied {
                    order,
                    delta,
                    cumulative,
                } => (order, delta, cumulative),
                FillObservation::DuplicateOrOutOfOrder => return,
                FillObservation::UnknownOrder => {
                    debug!(order_id, "Order update is not owned by this process");
                    return;
                }
            };
        if owned.purpose == OrderPurpose::RecoverySell {
            self.position_tracker
                .record_sell_fill(owned.token_id, delta);
            self.persist_recovery_fill(&owned, cumulative, source);
            return;
        }
        let Some(mut pair) = self.pending_pairs.get_mut(&owned.pair_id) else {
            return;
        };
        match owned.purpose {
            OrderPurpose::ArbitrageYes => pair.yes_filled = cumulative,
            OrderPurpose::ArbitrageNo => pair.no_filled = cumulative,
            OrderPurpose::RecoverySell => return,
        }
        self.position_tracker
            .record_buy_fill(owned.token_id, owned.price, delta);
        pair.status = classify_pair(pair.yes_filled, pair.no_filled, pair.yes_size, pair.no_size);
        self.record_pair(
            &pair,
            journal_state_for_pair(pair.status.clone()),
            Some(format!("{source} for order {order_id}")),
        );
    }

    fn register_pair_orders(&self, pair: &OrderPair) {
        for (order_id, token_id, purpose, price, size, fill) in [
            (
                &pair.yes_order_id,
                pair.yes_token_id,
                OrderPurpose::ArbitrageYes,
                pair.yes_price,
                pair.yes_size,
                pair.yes_filled,
            ),
            (
                &pair.no_order_id,
                pair.no_token_id,
                OrderPurpose::ArbitrageNo,
                pair.no_price,
                pair.no_size,
                pair.no_filled,
            ),
        ] {
            if order_id.is_empty() {
                continue;
            }
            self.owned_orders.register(OwnedOrder {
                order_id: order_id.clone(),
                pair_id: pair.pair_id.clone(),
                token_id,
                purpose,
                price,
                original_size: size,
                accounted_fill: fill,
            });
        }
    }

    fn persist_recovery_fill(&self, owned: &OwnedOrder, cumulative: Decimal, source: &str) {
        let Ok(Some(mut event)) = self.journal.latest_event(&owned.pair_id) else {
            return;
        };
        let Some(mut recovery) = event.recovery.take() else {
            return;
        };
        recovery.accounted_sell_fill = cumulative.to_string();
        recovery.pending_sell_amount = (owned.original_size - cumulative).max(dec!(0)).to_string();
        event.timestamp = chrono::Utc::now().to_rfc3339();
        event.detail = Some(format!("{source} for recovery order {}", owned.order_id));
        event.state = if cumulative >= owned.original_size {
            JournalState::Recovered
        } else {
            JournalState::Recovering
        };
        event.recovery = Some(recovery);
        if let Err(e) = self.journal.append(&event) {
            error!(error = %e, order_id = %owned.order_id, "Failed to persist recovery fill");
        }
    }

    fn record_pair(&self, pair: &OrderPair, state: JournalState, detail: Option<String>) {
        self.record_pair_with_recovery(pair, state, detail, None);
    }

    fn record_pair_with_recovery(
        &self,
        pair: &OrderPair,
        state: JournalState,
        detail: Option<String>,
        recovery: Option<JournalRecoverySnapshot>,
    ) {
        let event = JournalEvent {
            timestamp: chrono::Utc::now().to_rfc3339(),
            pair_id: pair.pair_id.clone(),
            market_id: format!("{:#x}", pair.market_id),
            state,
            yes_filled: pair.yes_filled.to_string(),
            no_filled: pair.no_filled.to_string(),
            detail,
            pair: Some(JournalPairSnapshot {
                market_id: format!("{:#x}", pair.market_id),
                yes_token_id: pair.yes_token_id.to_string(),
                no_token_id: pair.no_token_id.to_string(),
                yes_order_id: pair.yes_order_id.clone(),
                no_order_id: pair.no_order_id.clone(),
                yes_size: pair.yes_size.to_string(),
                no_size: pair.no_size.to_string(),
                yes_price: pair.yes_price.to_string(),
                no_price: pair.no_price.to_string(),
            }),
            recovery,
        };
        if let Err(e) = self.journal.append(&event) {
            error!(error = %e, pair_id = %pair.pair_id, "Failed to append order state journal");
        }
    }
}

async fn cumulative_order_fill(
    client: &dyn OrderStatusReader,
    order_id: &str,
    persisted: Decimal,
) -> Decimal {
    if order_id.is_empty() {
        return persisted;
    }
    match client.cumulative_fill(order_id).await {
        Ok(cumulative) => cumulative.max(persisted),
        Err(e) => {
            warn!(order_id, error = %e, "Startup order query failed; retaining persisted fill");
            persisted
        }
    }
}

fn journal_state_for_pair(status: PairStatus) -> JournalState {
    match status {
        PairStatus::BothFilled => JournalState::BothFilled,
        PairStatus::PartiallyFilled => JournalState::PartiallyFilled,
        PairStatus::OneFailed => JournalState::OneSided,
        PairStatus::BothFailed => JournalState::BothFailed,
    }
}

fn classify_pair(
    yes_filled: Decimal,
    no_filled: Decimal,
    yes_size: Decimal,
    no_size: Decimal,
) -> PairStatus {
    if yes_filled >= yes_size && no_filled >= no_size {
        PairStatus::BothFilled
    } else if yes_filled > dec!(0) && no_filled > dec!(0) {
        PairStatus::PartiallyFilled
    } else if yes_filled > dec!(0) || no_filled > dec!(0) {
        PairStatus::OneFailed
    } else {
        PairStatus::BothFailed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::future::BoxFuture;

    struct FakeOrderReader(Result<Decimal, &'static str>);

    impl OrderStatusReader for FakeOrderReader {
        fn cumulative_fill<'a>(&'a self, _order_id: &'a str) -> BoxFuture<'a, Result<Decimal>> {
            Box::pin(async move {
                self.0
                    .as_ref()
                    .copied()
                    .map_err(|error| anyhow::anyhow!(*error))
            })
        }
    }

    #[test]
    fn classifies_cumulative_pair_fills() {
        assert_eq!(
            classify_pair(dec!(10), dec!(10), dec!(10), dec!(10)),
            PairStatus::BothFilled
        );
        assert_eq!(
            classify_pair(dec!(10), dec!(5), dec!(10), dec!(10)),
            PairStatus::PartiallyFilled
        );
        assert_eq!(
            classify_pair(dec!(10), dec!(0), dec!(10), dec!(10)),
            PairStatus::OneFailed
        );
        assert_eq!(
            classify_pair(dec!(0), dec!(0), dec!(10), dec!(10)),
            PairStatus::BothFailed
        );
    }

    #[tokio::test]
    async fn reconciliation_reader_is_monotonic_and_failure_safe() {
        let newer = FakeOrderReader(Ok(dec!(7)));
        assert_eq!(
            cumulative_order_fill(&newer, "order", dec!(5)).await,
            dec!(7)
        );
        let stale = FakeOrderReader(Ok(dec!(3)));
        assert_eq!(
            cumulative_order_fill(&stale, "order", dec!(5)).await,
            dec!(5)
        );
        let failed = FakeOrderReader(Err("timeout"));
        assert_eq!(
            cumulative_order_fill(&failed, "order", dec!(5)).await,
            dec!(5)
        );
    }
}
