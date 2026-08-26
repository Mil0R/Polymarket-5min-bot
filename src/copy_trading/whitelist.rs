use crate::copy_trading::types::{
    CopyTradingConfig, DomainTag, WalletProfile, WalletTier, WalletTradeRecord,
};
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{info, warn};

/// Result of evaluating a wallet against the multi-dimensional funnel.
#[derive(Debug, Clone)]
pub struct WalletEvaluationResult {
    pub is_accepted: bool,
    pub tier: WalletTier,
    pub profile: WalletProfile,
    pub failure_reasons: Vec<String>,
}

/// Dynamic whitelist and signal source profiling manager.
///
/// Optionally persists wallet profiles (including 3-strike counters) to a JSON
/// state file so a restart does not require re-fetching all history from the
/// Data API (Opinion 4: persistence & warm-up). Writes are atomic (tmp + rename).
#[derive(Debug, Clone)]
pub struct WhitelistManager {
    config: CopyTradingConfig,
    wallets: Arc<DashMap<String, WalletProfile>>,
    state_path: Option<PathBuf>,
}

impl WhitelistManager {
    fn address_key(address: &str) -> String {
        address.trim().to_ascii_lowercase()
    }

    pub fn new(config: CopyTradingConfig) -> Self {
        Self {
            config,
            wallets: Arc::new(DashMap::new()),
            state_path: None,
        }
    }

    /// Creates a manager with persistence enabled; loads existing state if present.
    pub fn new_with_state(config: CopyTradingConfig, state_path: impl Into<PathBuf>) -> Self {
        let manager = Self {
            config,
            wallets: Arc::new(DashMap::new()),
            state_path: Some(state_path.into()),
        };
        manager.load_state();
        manager
    }

    fn load_state(&self) {
        let path = match &self.state_path {
            Some(p) => p,
            None => return,
        };
        if !path.exists() {
            return;
        }
        let raw = match std::fs::read_to_string(path) {
            Ok(r) => r,
            Err(e) => {
                warn!(path = %path.display(), error = %e, "Whitelist state read failed; starting fresh");
                return;
            }
        };
        match serde_json::from_str::<HashMap<String, WalletProfile>>(&raw) {
            Ok(profiles) => {
                for (addr, profile) in profiles {
                    self.wallets.insert(Self::address_key(&addr), profile);
                }
                info!(
                    path = %path.display(),
                    loaded = self.wallets.len(),
                    "Loaded persisted whitelist state"
                );
            }
            Err(e) => {
                warn!(path = %path.display(), error = %e, "Whitelist state corrupt; starting fresh");
            }
        }
    }

    /// Persists the current wallet map atomically (tmp file + rename).
    fn save_state(&self) {
        let path = match &self.state_path {
            Some(p) => p,
            None => return,
        };
        let snapshot: HashMap<String, WalletProfile> = self
            .wallets
            .iter()
            .map(|e| (e.key().clone(), e.value().clone()))
            .collect();
        let raw = match serde_json::to_vec_pretty(&snapshot) {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, "Whitelist state serialization failed");
                return;
            }
        };
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                let _ = std::fs::create_dir_all(parent);
            }
        }
        let tmp = path.with_extension("json.tmp");
        if let Err(e) = std::fs::write(&tmp, &raw) {
            warn!(path = %tmp.display(), error = %e, "Whitelist state write failed");
            return;
        }
        if let Err(e) = std::fs::rename(&tmp, path) {
            warn!(path = %path.display(), error = %e, "Whitelist state rename failed");
        }
    }

    /// Returns the persisted profile if the loaded state is fresh enough.
    pub fn get_fresh_profile(
        &self,
        address: &str,
        max_age_days: i64,
        now: DateTime<Utc>,
    ) -> Option<WalletProfile> {
        let profile = self.get_wallet(address)?;
        let age = now
            .signed_duration_since(profile.last_evaluated_at)
            .num_days();
        (age <= max_age_days).then_some(profile)
    }

    pub fn state_path(&self) -> Option<&Path> {
        self.state_path.as_deref()
    }

    /// Evaluates a wallet based on its historical trade records according to Module 1 rules.
    pub fn evaluate_wallet(
        &self,
        address: &str,
        records: &[WalletTradeRecord],
        now: DateTime<Utc>,
    ) -> WalletEvaluationResult {
        let mut failure_reasons = Vec::new();

        // Filter settled records only
        let settled: Vec<&WalletTradeRecord> =
            records.iter().filter(|r| r.settled_at.is_some()).collect();
        let total_settled = settled.len();

        // 1. Check settled event count (>= 50)
        if total_settled < self.config.min_settled_events {
            failure_reasons.push(format!(
                "Insufficient settled events: {} < {}",
                total_settled, self.config.min_settled_events
            ));
        }

        // 2. Check active span days (>= 90 days)
        let min_opened = records.iter().map(|r| r.opened_at).min().unwrap_or(now);
        let max_opened = records.iter().map(|r| r.opened_at).max().unwrap_or(now);
        let active_span_days = (max_opened - min_opened).num_days().max(0) as u32;

        if active_span_days < self.config.min_active_days {
            failure_reasons.push(format!(
                "Active time span too short: {} days < {} days",
                active_span_days, self.config.min_active_days
            ));
        }

        // 3. Check weighted average entry price (0.20 ~ 0.80)
        let mut total_notional = Decimal::ZERO;
        let mut sum_price_weight = Decimal::ZERO;
        let mut total_maker_vol = Decimal::ZERO;
        let mut behavior_known_notional = Decimal::ZERO;
        let mut behavior_known_count = 0usize;
        let mut arbitrage_trade_count = 0;
        let mut positive_pnl_sum = Decimal::ZERO;
        let mut max_single_profit = Decimal::ZERO;
        let mut total_pnl = Decimal::ZERO;
        let mut win_count = 0;

        let mut domain_volumes: HashMap<DomainTag, Decimal> = HashMap::new();
        let mut brier_sum = Decimal::ZERO;
        let mut brier_count = 0;

        // Hard blacklist flags (set by anti-gaming / bot-detection filters)
        let mut hard_blacklist = false;

        for r in &settled {
            let notional = if r.notional_usdc > Decimal::ZERO {
                r.notional_usdc
            } else {
                r.entry_price * dec!(100.0) // fallback
            };

            total_notional += notional;
            sum_price_weight += r.entry_price * notional;

            if r.behavior_data_known {
                behavior_known_notional += notional;
                behavior_known_count += 1;
                if r.is_maker {
                    total_maker_vol += notional;
                }
                if r.is_arbitrage_leg {
                    arbitrage_trade_count += 1;
                }
            }

            total_pnl += r.pnl_usdc;
            if r.pnl_usdc > Decimal::ZERO {
                positive_pnl_sum += r.pnl_usdc;
                if r.pnl_usdc > max_single_profit {
                    max_single_profit = r.pnl_usdc;
                }
            }

            // Domain breakdown
            *domain_volumes
                .entry(r.domain.clone())
                .or_insert(Decimal::ZERO) += notional;

            // Brier Score calculation for binary outcomes
            // BS = (f - o)^2 where f = entry_price (implied prob), o = 1 if won else 0
            if let Some(resolved) = &r.resolved_outcome {
                let outcome_val = if &r.outcome_bought == resolved {
                    win_count += 1;
                    Decimal::ONE
                } else {
                    Decimal::ZERO
                };
                let diff = r.entry_price - outcome_val;
                brier_sum += diff * diff;
                brier_count += 1;
            }
        }

        let avg_entry_price = if total_notional > Decimal::ZERO {
            sum_price_weight / total_notional
        } else {
            Decimal::ZERO
        };

        if avg_entry_price < self.config.min_avg_entry_price
            || avg_entry_price > self.config.max_avg_entry_price
        {
            failure_reasons.push(format!(
                "Average entry price out of range [{}, {}]: {:.4}",
                self.config.min_avg_entry_price, self.config.max_avg_entry_price, avg_entry_price
            ));
        }

        // 4. Max single profit dominance (<= 25%)
        let max_single_profit_ratio = if positive_pnl_sum > Decimal::ZERO {
            max_single_profit / positive_pnl_sum
        } else {
            Decimal::ONE
        };

        let is_single_strike_survivor =
            max_single_profit_ratio > self.config.max_single_profit_ratio;
        if is_single_strike_survivor {
            hard_blacklist = true;
            failure_reasons.push(format!(
                "Single trade profit dominance too high (survivor bias): {:.2}% > {:.2}%",
                max_single_profit_ratio * dec!(100),
                self.config.max_single_profit_ratio * dec!(100)
            ));
        }

        // 5. Blacklist filter: Maker ratio (> 90% is hard rejected)
        let maker_volume_ratio = if behavior_known_notional > Decimal::ZERO {
            total_maker_vol / behavior_known_notional
        } else {
            Decimal::ZERO
        };

        let is_maker_bot = maker_volume_ratio > self.config.max_maker_ratio;
        if is_maker_bot {
            hard_blacklist = true;
            failure_reasons.push(format!(
                "High-frequency maker bot detected: maker ratio {:.2}% > {:.2}%",
                maker_volume_ratio * dec!(100),
                self.config.max_maker_ratio * dec!(100)
            ));
        }

        // 6. Blacklist filter: Arbitrageur / Multi-leg wash trader
        let is_arbitrageur =
            behavior_known_count > 0 && arbitrage_trade_count > (behavior_known_count / 5); // > 20% known legs
        if is_arbitrageur {
            hard_blacklist = true;
            failure_reasons.push(format!(
                "Arbitrageur/multi-leg hedging pattern detected: {}/{} arbitrage legs",
                arbitrage_trade_count, total_settled
            ));
        }

        // 7. Profitability gate: a copy-trading signal source must be profitable.
        // Brier alone does not discriminate edge: a trader buying at 0.50 and winning
        // 66% of the time gets the same Brier (0.25) as a coin-flip trader, so we
        // require positive realized P&L over the evaluation window.
        let profitable = total_pnl > Decimal::ZERO;
        if !profitable {
            failure_reasons.push(format!(
                "Not profitable over evaluation window: total P&L ${:.2} <= 0",
                total_pnl
            ));
        }

        // Domain classification
        let mut domain_shares = Vec::new();
        let mut primary_domain = DomainTag::General;
        let mut max_domain_share = Decimal::ZERO;

        for (domain, vol) in &domain_volumes {
            let share = if total_notional > Decimal::ZERO {
                vol / total_notional
            } else {
                Decimal::ZERO
            };
            domain_shares.push((domain.clone(), share));
            if share > max_domain_share {
                max_domain_share = share;
                primary_domain = domain.clone();
            }
        }

        let brier_score = if brier_count > 0 {
            brier_sum / Decimal::from(brier_count)
        } else {
            dec!(0.50) // neutral default
        };

        if brier_score > self.config.max_acceptable_brier_score {
            failure_reasons.push(format!(
                "Brier score too high: {:.4} > {:.4}",
                brier_score, self.config.max_acceptable_brier_score
            ));
        }

        let win_rate = if total_settled > 0 {
            Decimal::from(win_count) / Decimal::from(total_settled)
        } else {
            Decimal::ZERO
        };

        // Determine Tier:
        //  - hard blacklist reasons (maker bot / arbitrageur / survivor bias) -> Blacklisted
        //  - soft failures (short span, few events, entry range, not profitable) -> Probation
        //  - all clear + profitable: Brier + win-rate based S/A tiering
        let (tier, is_accepted) = if hard_blacklist {
            (WalletTier::Blacklisted, false)
        } else if !failure_reasons.is_empty() {
            (WalletTier::Probation, false)
        } else if brier_score <= dec!(0.15) && total_settled >= 80 && win_rate >= dec!(0.65) {
            (WalletTier::TierS, true)
        } else if brier_score <= self.config.max_acceptable_brier_score {
            (WalletTier::TierA, true)
        } else {
            (WalletTier::Probation, false)
        };

        let rejection_reason = if failure_reasons.is_empty() {
            None
        } else {
            Some(failure_reasons.join("; "))
        };

        // Preserve the 3-strike counter across re-evaluations so a wallet cannot
        // wipe its strike history by simply being re-evaluated.
        let key = Self::address_key(address);
        let prev_strikes = self
            .wallets
            .get(&key)
            .map(|p| p.consecutive_major_direction_errors)
            .unwrap_or(0);

        let profile = WalletProfile {
            address: key,
            tier,
            total_settled_events: total_settled,
            active_span_days,
            avg_entry_price,
            max_single_profit_ratio,
            maker_volume_ratio,
            behavior_data_coverage: if total_notional > Decimal::ZERO {
                behavior_known_notional / total_notional
            } else {
                Decimal::ZERO
            },
            is_arbitrageur,
            is_single_strike_survivor,
            primary_domain,
            domain_shares,
            brier_score,
            win_rate,
            total_pnl_usdc: total_pnl,
            consecutive_major_direction_errors: prev_strikes,
            last_evaluated_at: now,
            rejection_reason,
        };

        WalletEvaluationResult {
            is_accepted,
            tier,
            profile,
            failure_reasons,
        }
    }

    /// Registers or updates an evaluated wallet profile.
    pub fn upsert_wallet(&self, profile: WalletProfile) {
        let addr = Self::address_key(&profile.address);
        let tier = profile.tier;
        let mut profile = profile;
        profile.address = addr.clone();
        self.wallets.insert(addr.clone(), profile);
        self.save_state();
        info!(address = %addr, tier = ?tier, "Upserted wallet profile in whitelist store");
    }

    /// Retrieves an active wallet profile if present.
    pub fn get_wallet(&self, address: &str) -> Option<WalletProfile> {
        self.wallets
            .get(&Self::address_key(address))
            .map(|w| w.value().clone())
    }

    /// Checks if a wallet is an authorized signal source in a specific domain.
    pub fn is_authorized_in_domain(
        &self,
        address: &str,
        domain: &DomainTag,
    ) -> (bool, Option<WalletTier>) {
        if let Some(profile) = self.wallets.get(&Self::address_key(address)) {
            if profile.tier != WalletTier::TierS && profile.tier != WalletTier::TierA {
                return (false, Some(profile.tier));
            }

            // Verify domain specialization matches
            let matches_domain = &profile.primary_domain == domain
                || profile
                    .domain_shares
                    .iter()
                    .any(|(d, share)| d == domain && *share >= dec!(0.30));

            if matches_domain {
                (true, Some(profile.tier))
            } else {
                warn!(
                    address = %address,
                    profile_domain = %profile.primary_domain,
                    target_domain = %domain,
                    "Signal rejected: wallet not authorized in this domain"
                );
                (false, Some(profile.tier))
            }
        } else {
            (false, None)
        }
    }

    /// Records a major direction failure (e.g. bought > 0.70 but resolved to 0).
    /// Enforces the 3-strike rule.
    pub fn record_direction_result(&self, address: &str, is_major_error: bool) {
        let mut changed = false;
        if let Some(mut entry) = self.wallets.get_mut(&Self::address_key(address)) {
            if is_major_error {
                entry.consecutive_major_direction_errors += 1;
                warn!(
                    address = %address,
                    errors = entry.consecutive_major_direction_errors,
                    "Recorded major direction error for wallet"
                );

                if entry.consecutive_major_direction_errors
                    >= self.config.max_consecutive_direction_errors
                {
                    entry.tier = WalletTier::Blacklisted;
                    entry.rejection_reason = Some(format!(
                        "Disqualified: {} consecutive major direction errors",
                        entry.consecutive_major_direction_errors
                    ));
                    warn!(
                        address = %address,
                        "Wallet disqualified and blacklisted under 3-strike rule"
                    );
                }
                changed = true;
            } else {
                // Reset consecutive error counter on valid prediction
                entry.consecutive_major_direction_errors = 0;
                changed = true;
            }
        }
        // Persist AFTER releasing the entry guard (save_state iterates the map;
        // holding a shard guard while iterating would deadlock).
        if changed {
            self.save_state();
        }
    }

    /// Returns list of all active whitelisted wallets (TierS or TierA).
    pub fn list_active_whitelist(&self) -> Vec<WalletProfile> {
        self.wallets
            .iter()
            .filter(|entry| entry.tier == WalletTier::TierS || entry.tier == WalletTier::TierA)
            .map(|entry| entry.value().clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::copy_trading::types::OutcomeSide;

    fn qualified_records() -> Vec<WalletTradeRecord> {
        (0..60)
            .map(|i| {
                let days_ago = 120 - (i as i64 * 2);
                let won = i % 3 != 0;
                WalletTradeRecord {
                    trade_id: format!("t-{}", i),
                    market_id: format!("mkt-{}", i),
                    domain: DomainTag::MacroInterestRate,
                    outcome_bought: OutcomeSide::Yes,
                    entry_price: dec!(0.50),
                    exit_price: Some(if won { dec!(1.0) } else { dec!(0.0) }),
                    notional_usdc: dec!(100.0),
                    pnl_usdc: if won { dec!(20.0) } else { dec!(-20.0) },
                    return_pct: dec!(0.2),
                    is_maker: false,
                    is_arbitrage_leg: false,
                    behavior_data_known: true,
                    resolved_outcome: Some(if won {
                        OutcomeSide::Yes
                    } else {
                        OutcomeSide::No
                    }),
                    opened_at: Utc::now() - chrono::Duration::days(days_ago),
                    settled_at: Some(Utc::now() - chrono::Duration::days(days_ago - 1)),
                }
            })
            .collect()
    }

    #[test]
    fn persistence_round_trip_preserves_profile_and_strikes() {
        let dir = std::env::temp_dir().join(format!("copy-wl-test-{}", std::process::id()));
        let path = dir.join("whitelist.json");
        let _ = std::fs::remove_file(&path);
        let now = Utc::now();

        // Writer: persist a wallet + 2 strikes.
        let manager = WhitelistManager::new_with_state(CopyTradingConfig::default(), path.clone());
        let res = manager.evaluate_wallet("0xExpert", &qualified_records(), now);
        assert!(res.is_accepted);
        manager.upsert_wallet(res.profile);
        manager.record_direction_result("0xExpert", true);
        manager.record_direction_result("0xExpert", true);
        assert!(path.exists(), "state file must be written");

        // Reader: fresh manager loads profile + strike history.
        let reader = WhitelistManager::new_with_state(CopyTradingConfig::default(), path.clone());
        let profile = reader.get_wallet("0xExpert").expect("profile persisted");
        assert_eq!(profile.tier, WalletTier::TierA);
        assert_eq!(
            profile.consecutive_major_direction_errors, 2,
            "strikes must survive restart"
        );

        // Warm-start helper: fresh profile is reusable, old one is not.
        assert!(reader.get_fresh_profile("0xExpert", 7, now).is_some());
        let stale = reader.get_fresh_profile("0xExpert", 7, now + chrono::Duration::days(10));
        assert!(stale.is_none(), "stale profile must not warm-start");

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_state_file_starts_fresh() {
        let dir = std::env::temp_dir().join(format!("copy-wl-bad-{}", std::process::id()));
        let path = dir.join("whitelist.json");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, "{{{not json").unwrap();
        let manager = WhitelistManager::new_with_state(CopyTradingConfig::default(), path.clone());
        assert!(
            manager.list_active_whitelist().is_empty(),
            "corrupt state must not panic"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn high_brier_rejection_always_has_an_explanation() {
        let now = Utc::now();
        let records = (0..60)
            .map(|i| {
                let days_ago = 120 - (i as i64 * 2);
                let won = i % 2 == 0;
                WalletTradeRecord {
                    trade_id: format!("brier-{i}"),
                    market_id: format!("brier-market-{i}"),
                    domain: DomainTag::MacroInterestRate,
                    outcome_bought: OutcomeSide::Yes,
                    entry_price: dec!(0.80),
                    exit_price: Some(if won { dec!(1.0) } else { dec!(0.0) }),
                    notional_usdc: dec!(100.0),
                    // Keep profitability independent from this calibration fixture.
                    pnl_usdc: dec!(1.0),
                    return_pct: dec!(0.01),
                    is_maker: false,
                    is_arbitrage_leg: false,
                    behavior_data_known: true,
                    resolved_outcome: Some(if won {
                        OutcomeSide::Yes
                    } else {
                        OutcomeSide::No
                    }),
                    opened_at: now - chrono::Duration::days(days_ago),
                    settled_at: Some(now - chrono::Duration::days(days_ago - 1)),
                }
            })
            .collect::<Vec<_>>();

        let result = WhitelistManager::new(CopyTradingConfig::default()).evaluate_wallet(
            "0xPoorCalibration",
            &records,
            now,
        );
        assert!(!result.is_accepted);
        assert!(result
            .failure_reasons
            .iter()
            .any(|reason| reason.starts_with("Brier score too high:")));
    }
}
