use crate::copy_trading::types::{
    CopyTradingConfig, DomainTag, WalletProfile, WalletTier, WalletTradeRecord,
};
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::HashMap;
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
#[derive(Debug, Clone)]
pub struct WhitelistManager {
    config: CopyTradingConfig,
    wallets: Arc<DashMap<String, WalletProfile>>,
}

impl WhitelistManager {
    pub fn new(config: CopyTradingConfig) -> Self {
        Self {
            config,
            wallets: Arc::new(DashMap::new()),
        }
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
        let settled: Vec<&WalletTradeRecord> = records.iter().filter(|r| r.settled_at.is_some()).collect();
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

            if r.is_maker {
                total_maker_vol += notional;
            }

            if r.is_arbitrage_leg {
                arbitrage_trade_count += 1;
            }

            total_pnl += r.pnl_usdc;
            if r.pnl_usdc > Decimal::ZERO {
                positive_pnl_sum += r.pnl_usdc;
                if r.pnl_usdc > max_single_profit {
                    max_single_profit = r.pnl_usdc;
                }
                win_count += 1;
            }

            // Domain breakdown
            *domain_volumes.entry(r.domain.clone()).or_insert(Decimal::ZERO) += notional;

            // Brier Score calculation for binary outcomes
            // BS = (f - o)^2 where f = entry_price (implied prob), o = 1 if won else 0
            if let Some(resolved) = &r.resolved_outcome {
                let outcome_val = if &r.outcome_bought == resolved {
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

        if avg_entry_price < self.config.min_avg_entry_price || avg_entry_price > self.config.max_avg_entry_price {
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

        let is_single_strike_survivor = max_single_profit_ratio > self.config.max_single_profit_ratio;
        if is_single_strike_survivor {
            hard_blacklist = true;
            failure_reasons.push(format!(
                "Single trade profit dominance too high (survivor bias): {:.2}% > {:.2}%",
                max_single_profit_ratio * dec!(100),
                self.config.max_single_profit_ratio * dec!(100)
            ));
        }

        // 5. Blacklist filter: Maker ratio (> 90% is hard rejected)
        let maker_volume_ratio = if total_notional > Decimal::ZERO {
            total_maker_vol / total_notional
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
        let is_arbitrageur = arbitrage_trade_count > (total_settled / 5); // > 20% arbitrage legs
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
        let prev_strikes = self
            .wallets
            .get(address)
            .map(|p| p.consecutive_major_direction_errors)
            .unwrap_or(0);

        let profile = WalletProfile {
            address: address.to_string(),
            tier,
            total_settled_events: total_settled,
            active_span_days,
            avg_entry_price,
            max_single_profit_ratio,
            maker_volume_ratio,
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
        let addr = profile.address.clone();
        let tier = profile.tier;
        self.wallets.insert(addr.clone(), profile);
        info!(address = %addr, tier = ?tier, "Upserted wallet profile in whitelist store");
    }

    /// Retrieves an active wallet profile if present.
    pub fn get_wallet(&self, address: &str) -> Option<WalletProfile> {
        self.wallets.get(address).map(|w| w.value().clone())
    }

    /// Checks if a wallet is an authorized signal source in a specific domain.
    pub fn is_authorized_in_domain(&self, address: &str, domain: &DomainTag) -> (bool, Option<WalletTier>) {
        if let Some(profile) = self.wallets.get(address) {
            if profile.tier != WalletTier::TierS && profile.tier != WalletTier::TierA {
                return (false, Some(profile.tier));
            }

            // Verify domain specialization matches
            let matches_domain = &profile.primary_domain == domain
                || profile.domain_shares.iter().any(|(d, share)| d == domain && *share >= dec!(0.30));

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
        if let Some(mut entry) = self.wallets.get_mut(address) {
            if is_major_error {
                entry.consecutive_major_direction_errors += 1;
                warn!(
                    address = %address,
                    errors = entry.consecutive_major_direction_errors,
                    "Recorded major direction error for wallet"
                );

                if entry.consecutive_major_direction_errors >= self.config.max_consecutive_direction_errors {
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
            } else {
                // Reset consecutive error counter on valid prediction
                entry.consecutive_major_direction_errors = 0;
            }
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
