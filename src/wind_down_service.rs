use crate::config::Config;
use crate::risk::positions::PositionTracker;
use crate::risk::RiskManager;
use crate::trading::TradingGateway;
use polypulse::merge;
use polypulse::positions::{get_positions, Position};

use polymarket_client_sdk::types::{Address, B256, U256};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::{HashMap, HashSet};
use std::str::FromStr as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::time::sleep;
use tracing::{debug, info, warn};

type MergeInfo = HashMap<B256, (U256, U256, Decimal)>;

fn mergeable_conditions(positions: &[Position]) -> Vec<B256> {
    let mut outcomes: HashMap<B256, HashSet<i32>> = HashMap::new();
    for position in positions {
        if position.size > dec!(0) && position.mergeable {
            outcomes
                .entry(position.condition_id)
                .or_default()
                .insert(position.outcome_index);
        }
    }
    outcomes
        .into_iter()
        .filter(|(_, sides)| {
            (sides.contains(&0) && sides.contains(&1)) || (sides.contains(&1) && sides.contains(&2))
        })
        .map(|(condition_id, _)| condition_id)
        .collect()
}

fn merge_info(positions: &[Position]) -> MergeInfo {
    let mut outcomes: HashMap<B256, HashMap<i32, (U256, Decimal)>> = HashMap::new();
    for position in positions {
        if position.size > dec!(0) && position.mergeable {
            outcomes
                .entry(position.condition_id)
                .or_default()
                .insert(position.outcome_index, (position.asset, position.size));
        }
    }
    outcomes
        .into_iter()
        .filter_map(|(condition_id, sides)| {
            let pair = match (sides.get(&1).copied(), sides.get(&2).copied()) {
                (Some(yes), Some(no)) => Some((yes, no)),
                _ => match (sides.get(&0).copied(), sides.get(&1).copied()) {
                    (Some(yes), Some(no)) => Some((yes, no)),
                    _ => None,
                },
            }?;
            Some((
                condition_id,
                (pair.0 .0, pair.1 .0, pair.0 .1.min(pair.1 .1)),
            ))
        })
        .collect()
}

fn raw_shares_to_decimal(raw: U256) -> Decimal {
    Decimal::from_str(&raw.to_string()).unwrap_or(dec!(0)) / dec!(1_000_000)
}

async fn merge_one(
    condition_id: B256,
    proxy: Address,
    private_key: &str,
    details: &MergeInfo,
    positions: &PositionTracker,
) -> anyhow::Result<String> {
    let asset_hint = details.get(&condition_id).map(|(yes, no, _)| (*yes, *no));
    let result = merge::merge_max(condition_id, proxy, private_key, None, asset_hint).await?;
    let chain_amount = raw_shares_to_decimal(result.merged_amount);
    if let Some((yes, no, available)) = details.get(&condition_id) {
        let amount = chain_amount.min(*available);
        positions.record_merge(*yes, *no, amount);
        info!(%condition_id, %amount, %chain_amount, "Merge deducted exposure");
    }
    Ok(result.tx_hash.to_string())
}

pub struct ScheduledMergeService {
    pub interval_minutes: u64,
    pub proxy: Address,
    pub private_key: String,
    pub positions: Arc<PositionTracker>,
    pub wind_down_in_progress: Arc<AtomicBool>,
}

impl ScheduledMergeService {
    pub async fn run(self) {
        const BETWEEN_MERGES: Duration = Duration::from_secs(30);
        const RETRY_BACKOFF: Duration = Duration::from_secs(12);
        let interval = Duration::from_secs(self.interval_minutes * 60);
        sleep(Duration::from_secs(10)).await;
        loop {
            if self.wind_down_in_progress.load(Ordering::Relaxed) {
                info!("Wind-down in progress; scheduled merge round skipped");
                sleep(interval).await;
                continue;
            }
            let fetched = match get_positions().await {
                Ok(fetched) => fetched,
                Err(error) => {
                    warn!(%error, "Failed to fetch positions; scheduled merge round skipped");
                    sleep(interval).await;
                    continue;
                }
            };
            let condition_ids = mergeable_conditions(&fetched);
            let details = merge_info(&fetched);
            debug!(count = condition_ids.len(), "Scheduled merge round ready");
            for (index, condition_id) in condition_ids.iter().copied().enumerate() {
                if index > 0 {
                    sleep(BETWEEN_MERGES).await;
                }
                let execute = || {
                    merge_one(
                        condition_id,
                        self.proxy,
                        &self.private_key,
                        &details,
                        &self.positions,
                    )
                };
                let mut result = execute().await;
                if result.as_ref().is_err_and(|error| {
                    let message = error.to_string();
                    message.contains("rate limit") || message.contains("retry in")
                }) {
                    sleep(RETRY_BACKOFF).await;
                    result = execute().await;
                }
                match result {
                    Ok(tx_hash) => info!(%condition_id, %tx_hash, "Scheduled merge complete"),
                    Err(error) if error.to_string().contains("no mergeable shares") => {
                        debug!(%condition_id, "No mergeable shares")
                    }
                    Err(error) => warn!(%condition_id, %error, "Scheduled merge failed"),
                }
                tokio::task::yield_now().await;
            }
            sleep(interval).await;
        }
    }
}

#[derive(Clone)]
pub struct WindDownService {
    config: Config,
    executor: Arc<dyn TradingGateway>,
    risk_manager: Arc<RiskManager>,
    in_progress: Arc<AtomicBool>,
}

impl WindDownService {
    pub fn new(
        config: Config,
        executor: Arc<dyn TradingGateway>,
        risk_manager: Arc<RiskManager>,
        in_progress: Arc<AtomicBool>,
    ) -> Self {
        Self {
            config,
            executor,
            risk_manager,
            in_progress,
        }
    }

    pub async fn run(
        self,
        allowed_conditions: HashSet<B256>,
        allowed_tokens: HashSet<U256>,
        protected_tokens: Arc<RwLock<HashSet<U256>>>,
    ) {
        const BETWEEN_MERGES: Duration = Duration::from_secs(30);
        if self.config.dry_run {
            info!("DRY_RUN: simulated wind-down; no writes submitted");
            self.in_progress.store(false, Ordering::Relaxed);
            return;
        }
        let protected_count = protected_tokens
            .read()
            .expect("winner token lock poisoned")
            .len();
        if protected_count > 0 {
            info!(
                protected = protected_count,
                "Late-Resolution Winner Trading hold-to-settlement tokens excluded from wind-down"
            );
        }
        let wind_down_tokens = unprotected_tokens(&allowed_tokens, &protected_tokens);
        match self.executor.cancel_window_orders(&wind_down_tokens).await {
            Ok(count) => info!(count, "Wind-down orders cancelled"),
            Err(error) => warn!(%error, "Wind-down cancellation failed; continuing"),
        }
        sleep(Duration::from_secs(10)).await;

        let positions = self.risk_manager.position_tracker();
        let mut did_merge = false;
        if let Some(proxy) = self.config.proxy_address {
            match get_positions().await {
                Ok(fetched) => {
                    let details = merge_info(&fetched);
                    let protected_snapshot = protected_tokens
                        .read()
                        .expect("winner token lock poisoned")
                        .clone();
                    let ids: Vec<_> = mergeable_conditions(&fetched)
                        .into_iter()
                        .filter(|id| {
                            allowed_conditions.contains(id)
                                && !condition_contains_protected_token(
                                    *id,
                                    &details,
                                    &protected_snapshot,
                                )
                        })
                        .collect();
                    let count = ids.len();
                    for (index, condition_id) in ids.into_iter().enumerate() {
                        match merge_one(
                            condition_id,
                            proxy,
                            &self.config.private_key,
                            &details,
                            &positions,
                        )
                        .await
                        {
                            Ok(tx_hash) => {
                                did_merge = true;
                                info!(%condition_id, %tx_hash, "Wind-down merge complete");
                            }
                            Err(error) => warn!(%condition_id, %error, "Wind-down merge failed"),
                        }
                        if index + 1 < count {
                            sleep(BETWEEN_MERGES).await;
                        }
                    }
                }
                Err(error) => warn!(%error, "Wind-down position fetch failed; merge skipped"),
            }
        } else {
            warn!("Wind-down merge skipped because proxy address is unset");
        }
        if did_merge {
            sleep(BETWEEN_MERGES).await;
        }

        let price = Decimal::try_from(self.config.wind_down_sell_price).unwrap_or(dec!(0.01));
        let wind_down_tokens = unprotected_tokens(&allowed_tokens, &protected_tokens);
        match get_positions().await {
            Ok(fetched) => {
                for position in fetched.iter().filter(|position| {
                    position.size > dec!(0) && wind_down_tokens.contains(&position.asset)
                }) {
                    let size = (position.size * dec!(100)).floor() / dec!(100);
                    if size < dec!(0.01) {
                        debug!(token_id = %position.asset, %size, "Wind-down position too small");
                        continue;
                    }
                    match self.executor.sell(position.asset, price, size).await {
                        Ok(_) => {
                            info!(token_id = %position.asset, %size, %price, "Wind-down sell submitted")
                        }
                        Err(error) => {
                            warn!(token_id = %position.asset, %size, %error, "Wind-down sell failed")
                        }
                    }
                }
            }
            Err(error) => warn!(%error, "Wind-down position fetch failed; sell skipped"),
        }
        self.in_progress.store(false, Ordering::Relaxed);
        info!("Wind-down complete; monitoring continues until window end");
    }
}

fn unprotected_tokens(
    allowed_tokens: &HashSet<U256>,
    protected_tokens: &RwLock<HashSet<U256>>,
) -> HashSet<U256> {
    let protected = protected_tokens.read().expect("winner token lock poisoned");
    allowed_tokens.difference(&protected).copied().collect()
}

fn condition_contains_protected_token(
    condition_id: B256,
    details: &MergeInfo,
    protected_tokens: &HashSet<U256>,
) -> bool {
    details
        .get(&condition_id)
        .is_some_and(|(yes, no, _)| protected_tokens.contains(yes) || protected_tokens.contains(no))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_raw_shares_using_six_decimals() {
        assert_eq!(raw_shares_to_decimal(U256::from(1_250_000_u64)), dec!(1.25));
    }

    #[test]
    fn protected_winner_token_excludes_its_condition_from_merge() {
        let condition = B256::repeat_byte(7);
        let yes = U256::from(10);
        let no = U256::from(11);
        let details = HashMap::from([(condition, (yes, no, dec!(5)))]);

        assert!(condition_contains_protected_token(
            condition,
            &details,
            &HashSet::from([yes])
        ));
        assert!(!condition_contains_protected_token(
            condition,
            &details,
            &HashSet::new()
        ));
    }

    #[test]
    fn wind_down_reads_winner_protection_dynamically() {
        let yes = U256::from(10);
        let no = U256::from(11);
        let allowed = HashSet::from([yes, no]);
        let protected = RwLock::new(HashSet::new());
        assert_eq!(unprotected_tokens(&allowed, &protected), allowed);

        protected.write().unwrap().insert(yes);
        assert_eq!(
            unprotected_tokens(&allowed, &protected),
            HashSet::from([no])
        );
    }
}
