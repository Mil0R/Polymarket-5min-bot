use crate::copy_trading::types::{
    ConsensusSignal, CopyTradingConfig, OutcomeSide, SignalAction, SignalEvent, WalletTier,
};
use crate::copy_trading::whitelist::WhitelistManager;
use chrono::{DateTime, Duration, Utc};
use dashmap::DashMap;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::HashSet;
use std::sync::Arc;
use tracing::{debug, info};
use uuid::Uuid;

/// Key representing a specific market directional opportunity.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ConsensusKey {
    market_id: String,
    outcome: OutcomeSide,
}

/// Accumulator of signals for a specific market & outcome within sliding window.
#[derive(Debug, Clone)]
struct SignalBucket {
    signals: Vec<SignalEvent>,
    last_consensus_at: Option<DateTime<Utc>>,
}

/// Multi-source consensus engine implementing Module 5 rules.
#[derive(Debug, Clone)]
pub struct ConsensusEngine {
    config: CopyTradingConfig,
    whitelist: WhitelistManager,
    buckets: Arc<DashMap<ConsensusKey, SignalBucket>>,
}

impl ConsensusEngine {
    pub fn new(config: CopyTradingConfig, whitelist: WhitelistManager) -> Self {
        Self {
            config,
            whitelist,
            buckets: Arc::new(DashMap::new()),
        }
    }

    /// Ingests a new signal event from a target wallet.
    /// If consensus is reached according to rules, returns `Some(ConsensusSignal)`.
    pub fn ingest_signal(&self, signal: SignalEvent, now: DateTime<Utc>) -> Option<ConsensusSignal> {
        // 1. Only process OpenBuy signals for consensus opening
        if signal.action != SignalAction::OpenBuy {
            return None;
        }

        // 1b. Minimum signal size gate: ignore dust signals from any source.
        //     Prevents tiny trades from two wallets triggering a full-size copy.
        if signal.target_notional_usdc < self.config.min_signal_notional_usdc {
            debug!(
                wallet = %signal.source_wallet,
                notional = %signal.target_notional_usdc,
                min_notional = %self.config.min_signal_notional_usdc,
                "Ignored signal: below minimum signal notional"
            );
            return None;
        }

        // 1c. Sanity bounds on the observed fill VWAP. A whitelisted wallet whose
        //     observed trade price is outside [0.05, 0.95] is either data noise or
        //     a spoofed/manipulated signal; do not let it drive the price cap.
        if signal.target_vwap < self.config.min_signal_vwap || signal.target_vwap > self.config.max_signal_vwap {
            debug!(
                wallet = %signal.source_wallet,
                vwap = %signal.target_vwap,
                min_vwap = %self.config.min_signal_vwap,
                max_vwap = %self.config.max_signal_vwap,
                "Ignored signal: target VWAP outside sanity bounds"
            );
            return None;
        }

        // 2. Validate wallet authorization in the specified domain
        let (is_auth, tier_opt) = self.whitelist.is_authorized_in_domain(&signal.source_wallet, &signal.domain);
        if !is_auth {
            debug!(
                wallet = %signal.source_wallet,
                domain = %signal.domain,
                "Ignored signal: wallet not authorized in this domain"
            );
            return None;
        }

        let tier = tier_opt.unwrap_or(WalletTier::TierA);

        // Fast-path: TierS elite forecaster can trigger directly if configured
        if tier == WalletTier::TierS {
            info!(
                wallet = %signal.source_wallet,
                market_id = %signal.market_id,
                outcome = %signal.outcome,
                "TierS elite forecaster triggered direct signal"
            );
            return Some(ConsensusSignal {
                consensus_id: format!("cs-{}", Uuid::new_v4()),
                market_id: signal.market_id.clone(),
                token_id: signal.token_id.clone(),
                outcome: signal.outcome.clone(),
                domain: signal.domain.clone(),
                contributing_wallets: vec![signal.source_wallet.clone()],
                consensus_score: dec!(1.0),
                target_vwap: signal.target_vwap,
                created_at: now,
            });
        }

        // 3. Multi-source consensus accumulation within 12h window
        let key = ConsensusKey {
            market_id: signal.market_id.clone(),
            outcome: signal.outcome.clone(),
        };

        let window_duration = Duration::hours(self.config.consensus_window_hours);
        let cutoff_time = now - window_duration;

        let mut bucket_entry = self.buckets.entry(key.clone()).or_insert_with(|| SignalBucket {
            signals: Vec::new(),
            last_consensus_at: None,
        });

        // Prune expired signals outside 12h window
        bucket_entry.signals.retain(|s| s.timestamp >= cutoff_time);

        // Avoid duplicate signals from the same wallet in the same window
        if !bucket_entry.signals.iter().any(|s| s.source_wallet == signal.source_wallet) {
            bucket_entry.signals.push(signal.clone());
        }

        // Check if recent consensus was already triggered in the last 6 hours to avoid re-triggering
        if let Some(last_time) = bucket_entry.last_consensus_at {
            if (now - last_time) < Duration::hours(6) {
                return None;
            }
        }

        // 4. Count distinct authorized wallets in the same domain
        let mut distinct_wallets = HashSet::new();
        let mut total_vwap_weight = Decimal::ZERO;
        let mut total_shares_weight = Decimal::ZERO;

        for s in &bucket_entry.signals {
            if s.domain == signal.domain {
                distinct_wallets.insert(s.source_wallet.clone());
                total_vwap_weight += s.target_vwap * s.target_shares;
                total_shares_weight += s.target_shares;
            }
        }

        if distinct_wallets.len() >= self.config.min_consensus_wallets {
            let avg_vwap = if total_shares_weight > Decimal::ZERO {
                total_vwap_weight / total_shares_weight
            } else {
                signal.target_vwap
            };

            let contributing_wallets: Vec<String> = distinct_wallets.into_iter().collect();
            bucket_entry.last_consensus_at = Some(now);
            // Clear the consumed signals so the SAME set of wallets cannot re-trigger
            // a duplicate entry after the 6h cooldown with stale information.
            bucket_entry.signals.clear();

            info!(
                market_id = %signal.market_id,
                outcome = %signal.outcome,
                domain = %signal.domain,
                wallets_count = contributing_wallets.len(),
                avg_vwap = %avg_vwap,
                "Multi-source consensus achieved (>= 2 top wallets in 12h window)"
            );

            Some(ConsensusSignal {
                consensus_id: format!("cs-{}", Uuid::new_v4()),
                market_id: signal.market_id,
                token_id: signal.token_id,
                outcome: signal.outcome,
                domain: signal.domain,
                contributing_wallets,
                consensus_score: dec!(0.90),
                target_vwap: avg_vwap,
                created_at: now,
            })
        } else {
            debug!(
                market_id = %signal.market_id,
                outcome = %signal.outcome,
                current_wallets = bucket_entry.signals.len(),
                required = self.config.min_consensus_wallets,
                "Signal recorded in consensus accumulator; waiting for partner signal"
            );
            None
        }
    }

    /// Periodic cleanup of stale signal buckets.
    pub fn prune_stale_buckets(&self, now: DateTime<Utc>) {
        let cutoff = now - Duration::hours(self.config.consensus_window_hours * 2);
        self.buckets.retain(|_, bucket| {
            bucket.signals.retain(|s| s.timestamp >= cutoff);
            !bucket.signals.is_empty()
        });
    }
}
