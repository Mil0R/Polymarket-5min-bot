use crate::wallet_copy_trading::types::{
    ConsensusSignal, OutcomeSide, SignalAction, SignalEvent, WalletCopyTradingConfig, WalletTier,
};
use crate::wallet_copy_trading::whitelist::WhitelistManager;
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
///
/// Sybil defense (Opinion 7): wallets declared as correlated (same operator /
/// funding source) count as ONE independent source. Two such wallets alone can
/// never satisfy `min_consensus_wallets`; an additional INDEPENDENT wallet is
/// required.
#[derive(Debug, Clone)]
pub struct ConsensusEngine {
    config: WalletCopyTradingConfig,
    whitelist: WhitelistManager,
    buckets: Arc<DashMap<ConsensusKey, SignalBucket>>,
    /// wallet address -> correlation group id (same id = same operator).
    correlated: Arc<dashmap::DashMap<String, usize>>,
}

impl ConsensusEngine {
    pub fn new(config: WalletCopyTradingConfig, whitelist: WhitelistManager) -> Self {
        Self {
            config,
            whitelist,
            buckets: Arc::new(DashMap::new()),
            correlated: Arc::new(dashmap::DashMap::new()),
        }
    }

    /// Declares two wallets as correlated (same operator). Accepts both
    /// lowercase and checksummed addresses (compared case-insensitively).
    pub fn declare_correlated(&self, a: &str, b: &str) {
        let a = a.to_ascii_lowercase();
        let b = b.to_ascii_lowercase();
        if a == b {
            return;
        }
        // Union-find-lite: merge by taking min of existing group ids.
        let gid_a = self.correlated.get(&a).map(|g| *g);
        let gid_b = self.correlated.get(&b).map(|g| *g);
        match (gid_a, gid_b) {
            (Some(ga), Some(gb)) => {
                let keep = ga.min(gb);
                let drop = ga.max(gb);
                if ga != gb {
                    self.correlated.insert(a.clone(), keep);
                    self.correlated.insert(b.clone(), keep);
                    // Reassign any wallet in the dropped group to `keep`.
                    let to_fix: Vec<(String, usize)> = self
                        .correlated
                        .iter()
                        .map(|e| (e.key().clone(), *e.value()))
                        .filter(|(_, g)| *g == drop)
                        .collect();
                    for (addr, _) in to_fix {
                        self.correlated.insert(addr, keep);
                    }
                }
            }
            (Some(ga), None) => {
                self.correlated.insert(b.clone(), ga);
            }
            (None, Some(gb)) => {
                self.correlated.insert(a.clone(), gb);
            }
            (None, None) => {
                let gid = self.correlated.len() + 1;
                self.correlated.insert(a.clone(), gid);
                self.correlated.insert(b.clone(), gid);
            }
        }
    }

    /// Loads correlated pairs from `WALLET_COPY_TRADING_CORRELATED_WALLET_PAIRS`
    /// (comma-separated `addr1:addr2` pairs).
    pub fn load_correlated_from_env(&self) {
        if let Ok(raw) = std::env::var("WALLET_COPY_TRADING_CORRELATED_WALLET_PAIRS") {
            for pair in raw.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()) {
                let mut parts = pair.split(':');
                if let (Some(a), Some(b)) = (parts.next(), parts.next()) {
                    let a = a.trim();
                    let b = b.trim();
                    if !a.is_empty() && !b.is_empty() && a != b {
                        self.declare_correlated(a, b);
                        info!(a = %a, b = %b, "Declared correlated wallet pair");
                    }
                }
            }
        }
    }

    /// Ingests a new signal event from a target wallet.
    /// If consensus is reached according to rules, returns `Some(ConsensusSignal)`.
    pub fn ingest_signal(
        &self,
        signal: SignalEvent,
        now: DateTime<Utc>,
    ) -> Option<ConsensusSignal> {
        // Reject startup replays, delayed indexer events and implausible future
        // timestamps before they can enter a bucket (Tier-S included).
        let age = now.signed_duration_since(signal.timestamp).num_seconds();
        if age > self.config.max_signal_age_seconds || age < -30 {
            debug!(
                signal = %signal.signal_id,
                age_seconds = age,
                max_age_seconds = self.config.max_signal_age_seconds,
                "Ignored stale or future-dated signal"
            );
            return None;
        }

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
        if signal.target_vwap < self.config.min_signal_vwap
            || signal.target_vwap > self.config.max_signal_vwap
        {
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
        let (is_auth, tier_opt) = self
            .whitelist
            .is_authorized_in_domain(&signal.source_wallet, &signal.domain);
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

        let mut bucket_entry = self
            .buckets
            .entry(key.clone())
            .or_insert_with(|| SignalBucket {
                signals: Vec::new(),
                last_consensus_at: None,
            });

        // Prune expired signals outside 12h window
        bucket_entry.signals.retain(|s| s.timestamp >= cutoff_time);

        // Avoid duplicate signals from the same wallet in the same window
        if !bucket_entry
            .signals
            .iter()
            .any(|s| s.source_wallet == signal.source_wallet)
        {
            bucket_entry.signals.push(signal.clone());
        }

        // Check if recent consensus was already triggered in the last 6 hours to avoid re-triggering
        if let Some(last_time) = bucket_entry.last_consensus_at {
            if (now - last_time) < Duration::hours(6) {
                return None;
            }
        }

        // 4. Count distinct INDEPENDENT sources in the same domain. Correlated
        //    wallets (declared same-operator) collapse into a single source so a
        //    two-wallet sybil pair cannot manufacture consensus (Opinion 7).
        //    `Source::Group` and `Source::Wallet` are disjoint by construction,
        //    so there is no id-collision risk.
        #[derive(Hash, PartialEq, Eq)]
        enum Source {
            Group(usize),
            Wallet(String),
        }
        let mut distinct_sources: HashSet<Source> = HashSet::new();
        let mut contributing_wallets: Vec<String> = Vec::new();
        let mut total_vwap_weight = Decimal::ZERO;
        let mut total_shares_weight = Decimal::ZERO;

        for s in &bucket_entry.signals {
            if s.domain != signal.domain {
                continue;
            }
            let lower = s.source_wallet.to_ascii_lowercase();
            let source = match self.correlated.get(&lower) {
                Some(g) => Source::Group(*g),
                None => Source::Wallet(lower.clone()),
            };
            // One independent actor gets one vote and one VWAP contribution.
            // Otherwise ten correlated wallets could still dominate the price
            // cap despite correctly counting as a single consensus source.
            if distinct_sources.insert(source) {
                contributing_wallets.push(s.source_wallet.clone());
                total_vwap_weight += s.target_vwap * s.target_shares;
                total_shares_weight += s.target_shares;
            }
        }

        if distinct_sources.len() >= self.config.min_consensus_wallets {
            let avg_vwap = if total_shares_weight > Decimal::ZERO {
                total_vwap_weight / total_shares_weight
            } else {
                signal.target_vwap
            };
            info!(
                market_id = %signal.market_id,
                outcome = %signal.outcome,
                domain = %signal.domain,
                independent_sources = distinct_sources.len(),
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

    /// Commits a consensus only after downstream risk reservation and proposal
    /// construction succeed. Rejected execution attempts leave observations
    /// available for a later fresh-book retry.
    pub fn commit_consensus(&self, consensus: &ConsensusSignal, now: DateTime<Utc>) {
        let key = ConsensusKey {
            market_id: consensus.market_id.clone(),
            outcome: consensus.outcome.clone(),
        };
        if let Some(mut bucket) = self.buckets.get_mut(&key) {
            bucket.last_consensus_at = Some(now);
            bucket.signals.clear();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wallet_copy_trading::types::{DomainTag, WalletTradeRecord};

    fn seed_wallet(manager: &WhitelistManager, addr: &str, now: DateTime<Utc>) {
        let records: Vec<WalletTradeRecord> = (0..60)
            .map(|i| {
                let days_ago = 120 - (i as i64 * 2);
                WalletTradeRecord {
                    trade_id: format!("{}-{}", addr, i),
                    market_id: format!("mkt-{}", i),
                    domain: DomainTag::MacroInterestRate,
                    outcome_bought: OutcomeSide::Yes,
                    entry_price: dec!(0.50),
                    exit_price: Some(dec!(1.0)),
                    notional_usdc: dec!(100.0),
                    pnl_usdc: dec!(20.0),
                    return_pct: dec!(0.2),
                    is_maker: false,
                    is_arbitrage_leg: false,
                    behavior_data_known: true,
                    resolved_outcome: Some(OutcomeSide::Yes),
                    opened_at: now - Duration::days(days_ago),
                    settled_at: Some(now - Duration::days(days_ago - 1)),
                }
            })
            .collect();
        let res = manager.evaluate_wallet(addr, &records, now);
        assert!(res.is_accepted);
        manager.upsert_wallet(res.profile);
    }

    fn mk(addr: &str, ts: DateTime<Utc>) -> SignalEvent {
        SignalEvent {
            signal_id: format!("s-{}-{}", addr, ts.timestamp()),
            source_wallet: addr.to_string(),
            market_id: "mkt-sybil".to_string(),
            token_id: "tok-yes".to_string(),
            outcome: OutcomeSide::Yes,
            domain: DomainTag::MacroInterestRate,
            action: SignalAction::OpenBuy,
            target_vwap: dec!(0.50),
            target_shares: dec!(500.0),
            target_notional_usdc: dec!(250.0),
            target_reduction_pct: None,
            timestamp: ts,
        }
    }

    #[test]
    fn correlated_wallets_cannot_manufacture_consensus() {
        let config = WalletCopyTradingConfig::default();
        let manager = WhitelistManager::new(config.clone());
        let now = Utc::now();
        for addr in ["0xAlpha", "0xBeta", "0xGamma"] {
            seed_wallet(&manager, addr, now);
        }
        let engine = ConsensusEngine::new(config, manager.clone());
        // Declare Alpha and Beta as the same operator (sybil pair).
        engine.declare_correlated("0xAlpha", "0xBeta");

        // Alpha + Beta alone: ONE independent source -> NO consensus (would have
        // fired before the sybil fix).
        assert!(engine.ingest_signal(mk("0xAlpha", now), now).is_none());
        assert!(
            engine
                .ingest_signal(
                    mk("0xBeta", now + Duration::minutes(5)),
                    now + Duration::minutes(5)
                )
                .is_none(),
            "correlated pair must not satisfy consensus"
        );

        // A third INDEPENDENT wallet provides the second source -> consensus.
        let t3 = now + Duration::minutes(30);
        let sig = engine
            .ingest_signal(mk("0xGamma", t3), t3)
            .expect("independent third wallet must complete consensus");
        assert_eq!(sig.contributing_wallets.len(), 2);
        assert!(sig.contributing_wallets.iter().any(|w| w == "0xGamma"));
    }

    #[test]
    fn case_insensitive_correlation_merges() {
        let config = WalletCopyTradingConfig::default();
        let manager = WhitelistManager::new(config.clone());
        let now = Utc::now();
        for addr in ["0xAlpha", "0xBeta"] {
            seed_wallet(&manager, addr, now);
        }
        let engine = ConsensusEngine::new(config, manager.clone());
        engine.declare_correlated("0xALPHA", "0xbeta");

        assert!(engine.ingest_signal(mk("0xAlpha", now), now).is_none());
        assert!(
            engine
                .ingest_signal(
                    mk("0xBeta", now + Duration::minutes(5)),
                    now + Duration::minutes(5)
                )
                .is_none(),
            "case-insensitive pair must still count as one source"
        );
    }
}
