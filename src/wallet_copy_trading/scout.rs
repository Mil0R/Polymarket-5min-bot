//! Read-only discovery and scoring of wallet-copy-trading candidate wallets.
//!
//! The scout deliberately has no execution dependency. It builds a candidate
//! pool from several public leaderboard slices, enriches each wallet with
//! recent trades and resolved positions, applies hard admission gates, and
//! writes reproducible CSV/NDJSON/PAPER-env artifacts.

use super::feed::{BookFetcher, MarketMetadataFetcher, WalletHistoryFetcher, DATA_API_URL};
use super::types::{DomainTag, WalletCopyTradingConfig, WalletTradeRecord};
use super::whitelist::WhitelistManager;
use anyhow::{Context, Result};
use chrono::{DateTime, Duration, NaiveDate, Utc};
use rust_decimal::prelude::ToPrimitive;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use tracing::{info, warn};

const TAXONOMY_VERSION: &str = "2026-08-25.1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScoutProfile {
    Conservative,
    Research,
}

impl ScoutProfile {
    pub fn parse(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "conservative" | "paper" => Ok(Self::Conservative),
            "research" | "shadow" => Ok(Self::Research),
            _ => {
                anyhow::bail!("unknown scout profile '{value}' (expected conservative or research)")
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScoutConfig {
    pub profile: ScoutProfile,
    pub categories: Vec<String>,
    pub subcategories: Vec<String>,
    pub periods: Vec<String>,
    pub order_by: Vec<String>,
    pub leaderboard_limit: usize,
    pub max_candidates: usize,
    pub candidate_offset: usize,
    pub top: usize,
    pub lookback_days: i64,
    pub recent_days: i64,
    pub max_last_trade_hours: i64,
    pub min_recent_trades: usize,
    pub min_recent_active_days: usize,
    pub min_resolved_markets: usize,
    pub min_data_coverage: f64,
    pub min_net_roi: f64,
    pub min_profit_factor: f64,
    pub max_drawdown: f64,
    pub max_single_profit_ratio: f64,
    pub min_profitable_windows: usize,
    pub live_book_sample: usize,
    pub copy_notional_usdc: f64,
    pub max_live_spread: f64,
    pub min_live_execution_fit: f64,
    pub min_compatible_market_ratio: f64,
    pub min_actionable_signal_ratio: f64,
    pub output_dir: PathBuf,
    pub explicit_wallets: Vec<String>,
}

impl ScoutConfig {
    pub fn conservative() -> Self {
        Self {
            profile: ScoutProfile::Conservative,
            categories: vec![
                "POLITICS".into(),
                "WEATHER".into(),
                "CRYPTO".into(),
                "ECONOMICS".into(),
                "FINANCE".into(),
                "TECH".into(),
            ],
            subcategories: Vec::new(),
            periods: vec!["WEEK".into(), "MONTH".into()],
            order_by: vec!["PNL".into(), "VOL".into()],
            leaderboard_limit: 10,
            max_candidates: 50,
            candidate_offset: 0,
            top: 20,
            lookback_days: 90,
            recent_days: 30,
            max_last_trade_hours: 48,
            min_recent_trades: 20,
            min_recent_active_days: 10,
            min_resolved_markets: 80,
            min_data_coverage: 0.80,
            min_net_roi: 0.08,
            min_profit_factor: 1.25,
            max_drawdown: 0.20,
            max_single_profit_ratio: 0.20,
            min_profitable_windows: 3,
            live_book_sample: 10,
            copy_notional_usdc: 50.0,
            max_live_spread: 0.02,
            min_live_execution_fit: 0.60,
            min_compatible_market_ratio: 0.70,
            min_actionable_signal_ratio: 0.10,
            output_dir: PathBuf::from("results/wallet-copy-candidates"),
            explicit_wallets: Vec::new(),
        }
    }

    pub fn research() -> Self {
        let mut config = Self::conservative();
        config.profile = ScoutProfile::Research;
        config.max_last_trade_hours = 24 * 7;
        config.min_recent_trades = 5;
        config.min_recent_active_days = 3;
        config.min_resolved_markets = 30;
        config.min_data_coverage = 0.50;
        config.min_net_roi = 0.0;
        config.min_profit_factor = 1.0;
        config.max_drawdown = 0.40;
        config.max_single_profit_ratio = 0.35;
        config.min_profitable_windows = 2;
        config.min_live_execution_fit = 0.30;
        config.min_compatible_market_ratio = 0.30;
        config.min_actionable_signal_ratio = 0.0;
        config
    }

    pub fn validate(&self) -> Result<()> {
        if self.categories.is_empty() && self.explicit_wallets.is_empty() {
            anyhow::bail!("at least one category or explicit wallet is required");
        }
        for category in &self.categories {
            validate_enum(
                "category",
                category,
                &[
                    "OVERALL",
                    "POLITICS",
                    "SPORTS",
                    "CRYPTO",
                    "CULTURE",
                    "MENTIONS",
                    "WEATHER",
                    "ECONOMICS",
                    "TECH",
                    "FINANCE",
                ],
            )?;
        }
        for period in &self.periods {
            validate_enum("period", period, &["DAY", "WEEK", "MONTH", "ALL"])?;
        }
        for order in &self.order_by {
            validate_enum("order", order, &["PNL", "VOL"])?;
        }
        if self.leaderboard_limit == 0 || self.leaderboard_limit > 50 {
            anyhow::bail!("leaderboard-limit must be in 1..=50");
        }
        if self.max_candidates == 0 || self.top == 0 {
            anyhow::bail!("max-candidates and top must be positive");
        }
        for (name, value) in [
            ("min-data-coverage", self.min_data_coverage),
            ("min-net-roi", self.min_net_roi),
            ("max-drawdown", self.max_drawdown),
            ("max-single-profit-ratio", self.max_single_profit_ratio),
            ("min-live-execution-fit", self.min_live_execution_fit),
            (
                "min-compatible-market-ratio",
                self.min_compatible_market_ratio,
            ),
            (
                "min-actionable-signal-ratio",
                self.min_actionable_signal_ratio,
            ),
        ] {
            if !(0.0..=1.0).contains(&value) {
                anyhow::bail!("{name} must be in 0..=1");
            }
        }
        Ok(())
    }
}

impl Default for ScoutConfig {
    fn default() -> Self {
        Self::conservative()
    }
}

fn validate_enum(name: &str, value: &str, allowed: &[&str]) -> Result<()> {
    if allowed.contains(&value.to_ascii_uppercase().as_str()) {
        Ok(())
    } else {
        anyhow::bail!("unsupported {name} '{value}'")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScoutReport {
    pub generated_at: DateTime<Utc>,
    pub taxonomy_version: String,
    pub profile: ScoutProfile,
    pub candidates_discovered: usize,
    pub candidates_evaluated: usize,
    pub candidates_accepted: usize,
    pub partial_failures: Vec<String>,
    pub candidates: Vec<CandidateEvaluation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CandidateEvaluation {
    pub address: String,
    pub accepted: bool,
    pub tier: String,
    pub score: f64,
    pub leaderboard_categories: Vec<String>,
    pub primary_category: String,
    pub primary_subcategory: String,
    pub classification_confidence: f64,
    pub last_trade_at: Option<DateTime<Utc>>,
    pub last_trade_age_hours: Option<i64>,
    pub recent_trades: usize,
    pub recent_active_days: usize,
    pub resolved_markets: usize,
    pub data_coverage: f64,
    pub net_roi: f64,
    pub total_pnl_usdc: f64,
    pub profit_factor: f64,
    pub win_rate: f64,
    pub max_drawdown: f64,
    pub max_single_profit_ratio: f64,
    pub profitable_30d_windows: usize,
    pub live_execution_fit: Option<f64>,
    pub live_book_coverage: f64,
    pub risk_score: f64,
    pub skill_score: f64,
    pub consistency_score: f64,
    pub activity_score: f64,
    pub execution_score: Option<f64>,
    pub compatible_market_ratio: Option<f64>,
    pub market_metadata_coverage: f64,
    pub buy_signals_sampled: usize,
    pub median_buy_notional_usdc: Option<f64>,
    pub actionable_signal_ratio: f64,
    pub runtime_compatible: bool,
    pub runtime_tier: String,
    pub runtime_avg_entry_price: f64,
    pub runtime_brier_score: f64,
    pub runtime_active_span_days: u32,
    pub runtime_failure_reasons: Vec<String>,
    pub rejection_reasons: Vec<String>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Default)]
struct CandidateSeed {
    categories: BTreeSet<String>,
    leaderboard_hits: usize,
}

#[derive(Debug, Clone)]
struct RecentTrade {
    condition_id: String,
    asset: String,
    title: String,
    slug: String,
    event_slug: String,
    timestamp: DateTime<Utc>,
    size: f64,
    price: f64,
    side: String,
}

#[derive(Debug, Clone)]
pub struct CandidateScout {
    config: ScoutConfig,
    http: reqwest::Client,
    history: WalletHistoryFetcher,
    books: BookFetcher,
    metadata: MarketMetadataFetcher,
    runtime_config: WalletCopyTradingConfig,
}

impl CandidateScout {
    pub fn new(config: ScoutConfig) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            config,
            http: reqwest::Client::builder()
                .user_agent("polypulse-copy-wallet-scout")
                .build()
                .context("failed to build scout HTTP client")?,
            history: WalletHistoryFetcher::new()?,
            books: BookFetcher::new()?,
            metadata: MarketMetadataFetcher::new()?,
            runtime_config: WalletCopyTradingConfig::from_env(),
        })
    }

    pub async fn run(&self) -> Result<ScoutReport> {
        let now = Utc::now();
        let (mut seeds, mut partial_failures) = self.discover().await;
        for address in &self.config.explicit_wallets {
            let normalized = normalize_address(address)?;
            seeds
                .entry(normalized)
                .or_default()
                .categories
                .insert("EXPLICIT".into());
        }

        let discovered = seeds.len();
        let mut ranked: Vec<_> = seeds.into_iter().collect();
        ranked.sort_by(|a, b| {
            b.1.leaderboard_hits
                .cmp(&a.1.leaderboard_hits)
                .then(a.0.cmp(&b.0))
        });
        let ranked = ranked
            .into_iter()
            .skip(self.config.candidate_offset)
            .take(self.config.max_candidates)
            .collect::<Vec<_>>();

        let mut candidates = Vec::new();
        let evaluation_count = ranked.len();
        for (index, (address, seed)) in ranked.into_iter().enumerate() {
            info!(wallet = %address, current = index + 1, total = evaluation_count, offset = self.config.candidate_offset, "Evaluating copy candidate");
            match self.evaluate(&address, &seed, now).await {
                Ok(candidate) => candidates.push(candidate),
                Err(error) => partial_failures.push(format!("{address}: {error:#}")),
            }
        }
        candidates.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal));
        let accepted = candidates.iter().filter(|c| c.accepted).count();
        let report = ScoutReport {
            generated_at: now,
            taxonomy_version: TAXONOMY_VERSION.into(),
            profile: self.config.profile,
            candidates_discovered: discovered,
            candidates_evaluated: candidates.len(),
            candidates_accepted: accepted,
            partial_failures,
            candidates,
        };
        self.write_outputs(&report)?;
        Ok(report)
    }

    async fn discover(&self) -> (HashMap<String, CandidateSeed>, Vec<String>) {
        let mut seeds = HashMap::<String, CandidateSeed>::new();
        let mut failures = Vec::new();
        for category in &self.config.categories {
            for period in &self.config.periods {
                for order in &self.config.order_by {
                    let url = format!(
                        "{DATA_API_URL}/v1/leaderboard?category={}&timePeriod={}&orderBy={}&limit={}",
                        category.to_ascii_uppercase(), period.to_ascii_uppercase(),
                        order.to_ascii_uppercase(), self.config.leaderboard_limit
                    );
                    match self.http.get(&url).send().await {
                        Ok(response) => match response.error_for_status() {
                            Ok(response) => match response.json::<Value>().await {
                                Ok(Value::Array(rows)) => {
                                    for row in rows {
                                        if let Some(raw) =
                                            row.get("proxyWallet").and_then(Value::as_str)
                                        {
                                            if let Ok(address) = normalize_address(raw) {
                                                let seed = seeds.entry(address).or_default();
                                                seed.categories
                                                    .insert(category.to_ascii_uppercase());
                                                seed.leaderboard_hits += 1;
                                            }
                                        }
                                    }
                                }
                                Ok(_) => failures.push(format!(
                                    "{category}/{period}/{order}: unexpected response shape"
                                )),
                                Err(error) => {
                                    failures.push(format!("{category}/{period}/{order}: {error}"))
                                }
                            },
                            Err(error) => {
                                failures.push(format!("{category}/{period}/{order}: {error}"))
                            }
                        },
                        Err(error) => {
                            failures.push(format!("{category}/{period}/{order}: {error}"))
                        }
                    }
                }
            }
        }
        (seeds, failures)
    }

    async fn evaluate(
        &self,
        address: &str,
        seed: &CandidateSeed,
        now: DateTime<Utc>,
    ) -> Result<CandidateEvaluation> {
        let records = self.history.fetch_records(address).await?;
        let runtime_evaluation = WhitelistManager::new(self.runtime_config.clone())
            .evaluate_wallet(address, &records, now);
        let (trades, trade_history_truncated) = self.fetch_recent_trades(address).await?;
        let category_filter: HashSet<_> = self
            .config
            .categories
            .iter()
            .map(|s| s.to_ascii_uppercase())
            .collect();
        let sub_filter: HashSet<_> = self
            .config
            .subcategories
            .iter()
            .map(|s| normalize_label(s))
            .collect();

        let classified: Vec<_> = trades
            .iter()
            .map(|t| {
                let text = format!("{} {} {}", t.title, t.slug, t.event_slug);
                classify_market(&text)
            })
            .collect();
        let primary_category = select_evaluation_category(&classified, &category_filter, seed);
        let primary_subcategory = mode(
            classified
                .iter()
                .filter(|x| x.0 == primary_category)
                .map(|x| x.1.clone()),
        )
        .unwrap_or_else(|| "unknown".into());
        let matching = classified
            .iter()
            .filter(|x| x.0 == primary_category && x.1 == primary_subcategory)
            .count();
        let category_observations = classified
            .iter()
            .filter(|x| x.0 == primary_category)
            .count();
        let classification_confidence = ratio(matching, category_observations);

        let cutoff = now - Duration::days(self.config.recent_days);
        let recent: Vec<_> = trades
            .iter()
            .zip(classified.iter())
            .filter(|(trade, class)| {
                trade.timestamp >= cutoff
                    && class.0 == primary_category
                    && (sub_filter.is_empty() || sub_filter.contains(&normalize_label(&class.1)))
            })
            .map(|(trade, _)| trade)
            .collect();
        let recent_active_days = recent
            .iter()
            .map(|t| t.timestamp.date_naive())
            .collect::<HashSet<NaiveDate>>()
            .len();
        let mut buy_notionals = recent
            .iter()
            .filter(|trade| trade.side.eq_ignore_ascii_case("BUY"))
            .map(|trade| trade.size * trade.price)
            .filter(|notional| notional.is_finite() && *notional > 0.0)
            .collect::<Vec<_>>();
        buy_notionals.sort_by(|left, right| left.partial_cmp(right).unwrap_or(Ordering::Equal));
        let median_buy_notional_usdc = median(&buy_notionals);
        let runtime_min_signal = self
            .runtime_config
            .min_signal_notional_usdc
            .to_f64()
            .unwrap_or(100.0);
        let actionable_buy_signals = buy_notionals
            .iter()
            .filter(|notional| **notional >= runtime_min_signal)
            .count();
        let actionable_signal_ratio = ratio(actionable_buy_signals, buy_notionals.len());
        let last_trade_at = trades
            .iter()
            .zip(classified.iter())
            .filter(|(_, class)| {
                class.0 == primary_category
                    && (sub_filter.is_empty() || sub_filter.contains(&normalize_label(&class.1)))
            })
            .map(|(trade, _)| trade.timestamp)
            .max();
        let last_trade_age_hours =
            last_trade_at.map(|ts| now.signed_duration_since(ts).num_hours().max(0));

        let settled: Vec<_> = records
            .iter()
            .filter(|r| {
                r.settled_at.is_some() && record_matches_category(&r.domain, &primary_category)
            })
            .collect();
        let resolved_markets = settled
            .iter()
            .map(|r| r.market_id.as_str())
            .collect::<HashSet<_>>()
            .len();
        let all_closed_like = records
            .iter()
            .filter(|r| {
                r.exit_price.is_some() && record_matches_category(&r.domain, &primary_category)
            })
            .count();
        let data_coverage = ratio(settled.len(), all_closed_like);
        let lookback_cutoff = now - Duration::days(self.config.lookback_days);
        let lookback: Vec<_> = settled
            .iter()
            .copied()
            .filter(|r| r.opened_at >= lookback_cutoff)
            .collect();
        let stats_records = if lookback.is_empty() {
            settled.clone()
        } else {
            lookback
        };
        let stats = performance_stats(&stats_records, now);
        let (live_execution_fit, live_book_coverage) = self.live_execution_fit(&recent).await;
        let (compatible_market_ratio, market_metadata_coverage) =
            self.market_compatibility(&recent).await;

        let activity_score = mean(&[
            bounded_ratio(recent.len(), self.config.min_recent_trades),
            bounded_ratio(recent_active_days, self.config.min_recent_active_days),
            last_trade_age_hours
                .map(|h| {
                    (1.0 - h as f64 / self.config.max_last_trade_hours.max(1) as f64)
                        .clamp(0.0, 1.0)
                })
                .unwrap_or(0.0),
        ]);
        let skill_score = mean(&[
            scale_positive(stats.net_roi, self.config.min_net_roi, 0.50),
            scale_positive(
                stats.profit_factor.min(5.0),
                self.config.min_profit_factor,
                3.0,
            ),
            stats.win_rate,
        ]);
        let consistency_score = (stats.profitable_windows as f64 / 4.0).clamp(0.0, 1.0);
        let risk_score = mean(&[
            (1.0 - stats.max_drawdown / self.config.max_drawdown.max(0.001)).clamp(0.0, 1.0),
            (1.0 - stats.max_single_profit_ratio / self.config.max_single_profit_ratio.max(0.001))
                .clamp(0.0, 1.0),
        ]);
        let execution_score = live_execution_fit;
        let (score, mut warnings) = if let Some(execution) = execution_score {
            (
                100.0
                    * (0.30 * skill_score
                        + 0.25 * execution
                        + 0.20 * consistency_score
                        + 0.15 * risk_score
                        + 0.10 * activity_score),
                Vec::new(),
            )
        } else {
            (100.0 * (0.40 * skill_score + 0.27 * consistency_score + 0.20 * risk_score + 0.13 * activity_score),
             vec!["live_execution_fit_not_measured: no active sampled books; score weights redistributed".into()])
        };
        if trade_history_truncated {
            warnings.push("trade_history_truncated_at_1000: recent trade and active-day counts are lower bounds".into());
        }
        if !sub_filter.is_empty() {
            warnings.push("subcategory_profit_metrics_use_primary_category_history: closed-position records do not expose a stable tag taxonomy".into());
        }
        if compatible_market_ratio.is_none() {
            warnings.push("market_compatibility_not_measured: no Gamma metadata available".into());
        }

        let mut rejection_reasons = Vec::new();
        gate(
            &mut rejection_reasons,
            last_trade_age_hours.is_some_and(|h| h <= self.config.max_last_trade_hours),
            "inactive_wallet",
            last_trade_age_hours
                .map(|value| value.to_string())
                .unwrap_or_else(|| "missing".into()),
            self.config.max_last_trade_hours,
        );
        gate(
            &mut rejection_reasons,
            recent.len() >= self.config.min_recent_trades,
            "insufficient_recent_trades",
            recent.len(),
            self.config.min_recent_trades,
        );
        gate(
            &mut rejection_reasons,
            recent_active_days >= self.config.min_recent_active_days,
            "insufficient_recent_active_days",
            recent_active_days,
            self.config.min_recent_active_days,
        );
        gate(
            &mut rejection_reasons,
            resolved_markets >= self.config.min_resolved_markets,
            "insufficient_resolved_markets",
            resolved_markets,
            self.config.min_resolved_markets,
        );
        gate(
            &mut rejection_reasons,
            data_coverage >= self.config.min_data_coverage,
            "insufficient_data_coverage",
            round4(data_coverage),
            self.config.min_data_coverage,
        );
        gate(
            &mut rejection_reasons,
            stats.net_roi >= self.config.min_net_roi,
            "net_roi_below_threshold",
            round4(stats.net_roi),
            self.config.min_net_roi,
        );
        gate(
            &mut rejection_reasons,
            stats.profit_factor >= self.config.min_profit_factor,
            "profit_factor_below_threshold",
            round4(stats.profit_factor),
            self.config.min_profit_factor,
        );
        gate(
            &mut rejection_reasons,
            stats.max_drawdown <= self.config.max_drawdown,
            "max_drawdown_exceeded",
            round4(stats.max_drawdown),
            self.config.max_drawdown,
        );
        gate(
            &mut rejection_reasons,
            stats.max_single_profit_ratio <= self.config.max_single_profit_ratio,
            "profit_concentration_too_high",
            round4(stats.max_single_profit_ratio),
            self.config.max_single_profit_ratio,
        );
        gate(
            &mut rejection_reasons,
            stats.profitable_windows >= self.config.min_profitable_windows,
            "insufficient_profitable_windows",
            stats.profitable_windows,
            self.config.min_profitable_windows,
        );
        if !category_filter.is_empty() && !category_filter.contains(&primary_category) {
            rejection_reasons.push(format!("category_mismatch: actual={primary_category}"));
        }
        if !sub_filter.is_empty() && !sub_filter.contains(&normalize_label(&primary_subcategory)) {
            rejection_reasons.push(format!(
                "subcategory_mismatch: actual={primary_subcategory}"
            ));
        }
        if let Some(fit) = live_execution_fit {
            gate(
                &mut rejection_reasons,
                fit >= self.config.min_live_execution_fit,
                "live_execution_fit_below_threshold",
                round4(fit),
                self.config.min_live_execution_fit,
            );
        } else if self.config.profile == ScoutProfile::Conservative {
            rejection_reasons.push("live_execution_fit_unavailable".into());
        }
        if let Some(ratio) = compatible_market_ratio {
            gate(
                &mut rejection_reasons,
                ratio >= self.config.min_compatible_market_ratio,
                "runtime_market_compatibility_ratio_below_threshold",
                round4(ratio),
                self.config.min_compatible_market_ratio,
            );
        } else if self.config.profile == ScoutProfile::Conservative {
            rejection_reasons.push("market_compatibility_unavailable".into());
        }
        gate(
            &mut rejection_reasons,
            actionable_signal_ratio >= self.config.min_actionable_signal_ratio,
            "actionable_signal_ratio_below_threshold",
            round4(actionable_signal_ratio),
            self.config.min_actionable_signal_ratio,
        );
        if !runtime_evaluation.is_accepted {
            rejection_reasons.extend(
                runtime_evaluation
                    .failure_reasons
                    .iter()
                    .map(|reason| format!("runtime_funnel: {reason}")),
            );
        }
        let accepted = rejection_reasons.is_empty();
        let tier = if !accepted {
            "REJECT"
        } else if score >= 80.0 {
            "A"
        } else if score >= 65.0 {
            "B"
        } else {
            "RESEARCH"
        };

        Ok(CandidateEvaluation {
            address: address.into(),
            accepted,
            tier: tier.into(),
            score: round2(score),
            leaderboard_categories: seed.categories.iter().cloned().collect(),
            primary_category,
            primary_subcategory,
            classification_confidence: round4(classification_confidence),
            last_trade_at,
            last_trade_age_hours,
            recent_trades: recent.len(),
            recent_active_days,
            resolved_markets,
            data_coverage: round4(data_coverage),
            net_roi: round4(stats.net_roi),
            total_pnl_usdc: round2(stats.total_pnl),
            profit_factor: round4(stats.profit_factor),
            win_rate: round4(stats.win_rate),
            max_drawdown: round4(stats.max_drawdown),
            max_single_profit_ratio: round4(stats.max_single_profit_ratio),
            profitable_30d_windows: stats.profitable_windows,
            live_execution_fit: live_execution_fit.map(round4),
            live_book_coverage: round4(live_book_coverage),
            risk_score: round4(risk_score),
            skill_score: round4(skill_score),
            consistency_score: round4(consistency_score),
            activity_score: round4(activity_score),
            execution_score: execution_score.map(round4),
            compatible_market_ratio: compatible_market_ratio.map(round4),
            market_metadata_coverage: round4(market_metadata_coverage),
            buy_signals_sampled: buy_notionals.len(),
            median_buy_notional_usdc: median_buy_notional_usdc.map(round2),
            actionable_signal_ratio: round4(actionable_signal_ratio),
            runtime_compatible: runtime_evaluation.is_accepted,
            runtime_tier: format!("{:?}", runtime_evaluation.tier),
            runtime_avg_entry_price: round4(
                runtime_evaluation
                    .profile
                    .avg_entry_price
                    .to_f64()
                    .unwrap_or_default(),
            ),
            runtime_brier_score: round4(
                runtime_evaluation
                    .profile
                    .brier_score
                    .to_f64()
                    .unwrap_or_default(),
            ),
            runtime_active_span_days: runtime_evaluation.profile.active_span_days,
            runtime_failure_reasons: runtime_evaluation.failure_reasons,
            rejection_reasons,
            warnings,
        })
    }

    async fn fetch_recent_trades(&self, address: &str) -> Result<(Vec<RecentTrade>, bool)> {
        const PAGE: usize = 100;
        const CAP: usize = 1000;
        let mut out = Vec::new();
        let mut truncated = false;
        for offset in (0..CAP).step_by(PAGE) {
            let url = format!(
                "{DATA_API_URL}/trades?user={address}&limit={PAGE}&offset={offset}&takerOnly=false"
            );
            let rows = self
                .http
                .get(&url)
                .send()
                .await?
                .error_for_status()?
                .json::<Value>()
                .await?;
            let rows = rows.as_array().context("trades response is not an array")?;
            for row in rows {
                if let Some(trade) = parse_recent_trade(row) {
                    out.push(trade);
                }
            }
            if rows.len() < PAGE {
                break;
            }
            if offset + PAGE == CAP {
                truncated = true;
            }
        }
        out.sort_by_key(|t| std::cmp::Reverse(t.timestamp));
        let mut seen = HashSet::new();
        out.retain(|t| {
            seen.insert(format!(
                "{}:{}:{}:{}",
                t.condition_id,
                t.asset,
                t.timestamp.timestamp(),
                t.size
            ))
        });
        Ok((out, truncated))
    }

    async fn live_execution_fit(&self, recent: &[&RecentTrade]) -> (Option<f64>, f64) {
        if self.config.live_book_sample == 0 {
            return (None, 0.0);
        }
        let mut seen = HashSet::new();
        let mut selected = Vec::new();
        for trade in recent {
            if seen.insert(trade.asset.clone()) {
                selected.push(*trade);
            }
            if selected.len() >= self.config.live_book_sample {
                break;
            }
        }
        if selected.is_empty() {
            return (None, 0.0);
        }
        let attempted = selected.len();
        let mut measured = 0usize;
        let mut passed = 0usize;
        for trade in selected {
            match self.books.fetch(&trade.asset, &trade.condition_id).await {
                Ok(book) => {
                    if let Some(spread) = book.relative_spread().and_then(|v| v.to_f64()) {
                        measured += 1;
                        let depth = book.top_n_ask_depth_notional(5).to_f64().unwrap_or(0.0);
                        if spread <= self.config.max_live_spread
                            && depth >= self.config.copy_notional_usdc
                        {
                            passed += 1;
                        }
                    }
                }
                Err(error) => {
                    warn!(token = %trade.asset, error = %error, "Could not sample live book for candidate")
                }
            }
        }
        let coverage = ratio(measured, attempted);
        ((measured > 0).then(|| ratio(passed, measured)), coverage)
    }

    async fn market_compatibility(&self, recent: &[&RecentTrade]) -> (Option<f64>, f64) {
        let sample_size = self.config.live_book_sample.max(1);
        let mut seen = HashSet::new();
        let mut selected = Vec::new();
        for trade in recent {
            if seen.insert(trade.condition_id.clone()) {
                selected.push(*trade);
            }
            if selected.len() >= sample_size {
                break;
            }
        }
        if selected.is_empty() {
            return (None, 0.0);
        }
        let attempted = selected.len();
        let mut measured = 0usize;
        let mut compatible = 0usize;
        for trade in selected {
            match self.metadata.fetch_by_condition(&trade.condition_id).await {
                Ok(Some(market)) => {
                    measured += 1;
                    let hours_to_expiry = market
                        .settlement_time
                        .signed_duration_since(trade.timestamp)
                        .num_seconds() as f64
                        / 3600.0;
                    if market.outcome_count == 2
                        && (!self.runtime_config.isolate_multi_outcome || !market.is_neg_risk)
                        && hours_to_expiry
                            >= self
                                .runtime_config
                                .min_hours_to_expiry
                                .to_f64()
                                .unwrap_or_default()
                        && market.volume_24h_usdc >= self.runtime_config.min_24h_volume_usdc
                    {
                        compatible += 1;
                    }
                }
                Ok(None) => {}
                Err(error) => warn!(
                    market = %trade.condition_id,
                    error = %error,
                    "Could not sample market compatibility for candidate"
                ),
            }
        }
        let coverage = ratio(measured, attempted);
        (
            (measured > 0).then(|| ratio(compatible, measured)),
            coverage,
        )
    }

    fn write_outputs(&self, report: &ScoutReport) -> Result<()> {
        std::fs::create_dir_all(&self.config.output_dir)?;
        let details = self.config.output_dir.join("candidate-details.ndjson");
        let rejections = self.config.output_dir.join("rejections.ndjson");
        let csv = self.config.output_dir.join("candidates.csv");
        let env = self.config.output_dir.join("recommended-paper.env");
        let snapshot = self.config.output_dir.join("report.json");

        let detail_lines = report
            .candidates
            .iter()
            .map(serde_json::to_string)
            .collect::<std::result::Result<Vec<_>, _>>()?
            .join("\n");
        atomic_write(&details, &(detail_lines + "\n"))?;
        let rejection_lines = report
            .candidates
            .iter()
            .filter(|c| !c.accepted)
            .map(serde_json::to_string)
            .collect::<std::result::Result<Vec<_>, _>>()?
            .join("\n");
        atomic_write(&rejections, &(rejection_lines + "\n"))?;

        let mut csv_text = "rank,address,accepted,tier,score,category,subcategory,last_trade_age_hours,recent_trades,active_days,resolved_markets,net_roi,profit_factor,max_drawdown,live_execution_fit,compatible_market_ratio,market_metadata_coverage,median_buy_notional_usdc,actionable_signal_ratio,runtime_compatible,runtime_tier,runtime_avg_entry_price,runtime_brier_score,runtime_active_span_days,runtime_failure_reasons,rejection_reasons\n".to_string();
        for (rank, c) in report.candidates.iter().enumerate() {
            csv_text.push_str(&format!(
                "{},{},{},{},{:.2},{},{},{},{},{},{},{:.4},{:.4},{:.4},{},{},{:.4},{},{:.4},{},{},{:.4},{:.4},{},{},{}\n",
                rank + 1,
                c.address,
                c.accepted,
                c.tier,
                c.score,
                csv_escape(&c.primary_category),
                csv_escape(&c.primary_subcategory),
                c.last_trade_age_hours
                    .map(|v| v.to_string())
                    .unwrap_or_default(),
                c.recent_trades,
                c.recent_active_days,
                c.resolved_markets,
                c.net_roi,
                c.profit_factor,
                c.max_drawdown,
                c.live_execution_fit
                    .map(|v| format!("{v:.4}"))
                    .unwrap_or_default(),
                c.compatible_market_ratio
                    .map(|v| format!("{v:.4}"))
                    .unwrap_or_default(),
                c.market_metadata_coverage,
                c.median_buy_notional_usdc
                    .map(|v| format!("{v:.2}"))
                    .unwrap_or_default(),
                c.actionable_signal_ratio,
                c.runtime_compatible,
                csv_escape(&c.runtime_tier),
                c.runtime_avg_entry_price,
                c.runtime_brier_score,
                c.runtime_active_span_days,
                csv_escape(&c.runtime_failure_reasons.join(";")),
                csv_escape(&c.rejection_reasons.join(";"))
            ))
        }
        atomic_write(&csv, &csv_text)?;

        let addresses = report
            .candidates
            .iter()
            .filter(|c| c.accepted)
            .take(self.config.top)
            .map(|c| c.address.as_str())
            .collect::<Vec<_>>()
            .join(",");
        let env_text = format!(
            "# Generated {} by wallet-copy-scout; review before copying into .env.\n# This file is PAPER-only and never enables live execution.\nWALLET_COPY_TRADING_DRY_RUN=true\nWALLET_COPY_TRADING_TARGET_WALLETS={}\n",
            report.generated_at.to_rfc3339(), addresses
        );
        atomic_write(&env, &env_text)?;
        atomic_write(&snapshot, &serde_json::to_string_pretty(report)?)?;
        Ok(())
    }
}

#[derive(Default)]
struct PerformanceStats {
    total_pnl: f64,
    net_roi: f64,
    profit_factor: f64,
    win_rate: f64,
    max_drawdown: f64,
    max_single_profit_ratio: f64,
    profitable_windows: usize,
}

fn performance_stats(records: &[&WalletTradeRecord], now: DateTime<Utc>) -> PerformanceStats {
    if records.is_empty() {
        return PerformanceStats::default();
    }
    let mut ordered = records.to_vec();
    ordered.sort_by_key(|r| r.opened_at);
    let pnl: Vec<f64> = ordered
        .iter()
        .map(|r| r.pnl_usdc.to_f64().unwrap_or(0.0))
        .collect();
    let cost: f64 = ordered
        .iter()
        .map(|r| r.notional_usdc.to_f64().unwrap_or(0.0).max(0.0))
        .sum();
    let total_pnl: f64 = pnl.iter().sum();
    let gains: f64 = pnl.iter().copied().filter(|v| *v > 0.0).sum();
    let losses: f64 = -pnl.iter().copied().filter(|v| *v < 0.0).sum::<f64>();
    let wins = pnl.iter().filter(|v| **v > 0.0).count();
    let mut equity = 0.0_f64;
    let mut peak = 0.0_f64;
    let mut max_drawdown = 0.0_f64;
    for value in &pnl {
        equity += value;
        peak = peak.max(equity);
        let denominator = cost.max(1.0);
        max_drawdown = max_drawdown.max((peak - equity) / denominator);
    }
    let max_profit = pnl.iter().copied().fold(0.0_f64, f64::max);
    let mut profitable_windows = 0;
    for index in 0..4 {
        let end = now - Duration::days(index * 30);
        let start = end - Duration::days(30);
        let window_pnl: f64 = ordered
            .iter()
            .filter(|r| r.opened_at >= start && r.opened_at < end)
            .map(|r| r.pnl_usdc.to_f64().unwrap_or(0.0))
            .sum();
        if window_pnl > 0.0 {
            profitable_windows += 1;
        }
    }
    PerformanceStats {
        total_pnl,
        net_roi: if cost > 0.0 { total_pnl / cost } else { 0.0 },
        profit_factor: if losses > 0.0 {
            gains / losses
        } else if gains > 0.0 {
            10.0
        } else {
            0.0
        },
        win_rate: ratio(wins, pnl.len()),
        max_drawdown,
        max_single_profit_ratio: if gains > 0.0 { max_profit / gains } else { 1.0 },
        profitable_windows,
    }
}

fn parse_recent_trade(row: &Value) -> Option<RecentTrade> {
    let timestamp = row.get("timestamp")?.as_i64()?;
    Some(RecentTrade {
        condition_id: row.get("conditionId")?.as_str()?.to_string(),
        asset: value_string(row.get("asset")?)?,
        title: row
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        slug: row
            .get("slug")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        event_slug: row
            .get("eventSlug")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        timestamp: DateTime::from_timestamp(timestamp, 0)?,
        size: value_f64(row.get("size")?)?,
        price: value_f64(row.get("price")?)?,
        side: row
            .get("side")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_ascii_uppercase(),
    })
}

fn value_string(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_string)
        .or_else(|| value.as_u64().map(|v| v.to_string()))
}
fn value_f64(value: &Value) -> Option<f64> {
    value.as_f64().or_else(|| value.as_str()?.parse().ok())
}

fn classify_market(text: &str) -> (String, String) {
    let t = text.to_ascii_lowercase();
    let any = |words: &[&str]| words.iter().any(|word| t.contains(word));
    if any(&["hurricane", "cyclone", "typhoon", "storm"]) {
        return ("WEATHER".into(), "storm-hurricane".into());
    }
    if any(&["rain", "rainfall", "precipitation"]) {
        return ("WEATHER".into(), "precipitation".into());
    }
    if any(&["snow", "snowfall"]) {
        return ("WEATHER".into(), "snow".into());
    }
    if any(&["temperature", "degrees", "weather"]) {
        return ("WEATHER".into(), "temperature".into());
    }
    if any(&[
        "election",
        "electoral",
        "primary",
        "nominee",
        "candidate",
        "president",
        "governor",
        "mayor",
    ]) {
        return ("POLITICS".into(), "elections".into());
    }
    if any(&[
        "war",
        "ceasefire",
        "invasion",
        "nato",
        "ukraine",
        "gaza",
        "iran",
        "taiwan",
        "sanction",
    ]) {
        return ("POLITICS".into(), "geopolitics".into());
    }
    if any(&["court", "supreme court", "lawsuit", "convict", "indict"]) {
        return ("POLITICS".into(), "courts-legal".into());
    }
    if any(&["approval rating", "polling", "poll "]) {
        return ("POLITICS".into(), "polling-approval".into());
    }
    if any(&["congress", "senate", "bill", "legislation", "law", "policy"]) {
        return ("POLITICS".into(), "legislation-policy".into());
    }
    if any(&[
        "bitcoin", "btc", "ethereum", "eth", "solana", "xrp", "crypto",
    ]) {
        let sub = if any(&["etf", "sec", "regulation"]) {
            "regulation-etf"
        } else if any(&[
            "up or down",
            "higher than",
            "price",
            "above",
            "below",
            "reach",
        ]) {
            "price-direction"
        } else {
            "protocol-ecosystem"
        };
        return ("CRYPTO".into(), sub.into());
    }
    if any(&["fed", "fomc", "interest rate", "rate cut", "rate hike"]) {
        return ("ECONOMICS".into(), "interest-rates".into());
    }
    if any(&["inflation", "cpi", "ppi"]) {
        return ("ECONOMICS".into(), "inflation".into());
    }
    if any(&["gdp", "unemployment", "jobs report", "payroll"]) {
        return ("ECONOMICS".into(), "macro-data".into());
    }
    if any(&["earnings", "stock", "shares", "s&p", "nasdaq", "dow jones"]) {
        return ("FINANCE".into(), "equities-earnings".into());
    }
    if any(&["oil", "gold", "commodity", "commodities"]) {
        return ("FINANCE".into(), "commodities".into());
    }
    if any(&[
        "nba", "nfl", "mlb", "nhl", "soccer", "football", "tennis", "match", "game",
    ]) {
        return ("SPORTS".into(), "match-event".into());
    }
    if any(&[
        "openai",
        "google",
        "apple",
        "nvidia",
        "artificial intelligence",
        " ai ",
    ]) {
        return ("TECH".into(), "companies-ai".into());
    }
    if any(&[
        "movie",
        "album",
        "oscar",
        "grammy",
        "box office",
        "celebrity",
    ]) {
        return ("CULTURE".into(), "entertainment".into());
    }
    let domain = super::feed::DomainClassifier::classify(text);
    match domain {
        DomainTag::Elections => ("POLITICS".into(), "elections".into()),
        DomainTag::Geopolitics => ("POLITICS".into(), "geopolitics".into()),
        DomainTag::MacroInterestRate => ("ECONOMICS".into(), "macro".into()),
        DomainTag::Crypto => ("CRYPTO".into(), "general".into()),
        DomainTag::ScienceClimate => ("WEATHER".into(), "climate-seasonal".into()),
        DomainTag::PopCultureSports => ("CULTURE".into(), "general".into()),
        DomainTag::TechRegulation => ("TECH".into(), "regulation".into()),
        _ => ("UNKNOWN".into(), "unknown".into()),
    }
}

fn select_evaluation_category(
    classified: &[(String, String)],
    requested: &HashSet<String>,
    seed: &CandidateSeed,
) -> String {
    if requested.len() == 1 {
        return requested.iter().next().cloned().unwrap_or_default();
    }
    let eligible: HashSet<String> = if requested.is_empty() {
        seed.categories
            .iter()
            .filter(|category| category.as_str() != "EXPLICIT")
            .cloned()
            .collect()
    } else {
        requested.clone()
    };
    mode(
        classified
            .iter()
            .filter(|class| eligible.is_empty() || eligible.contains(&class.0))
            .map(|class| class.0.clone()),
    )
    .or_else(|| eligible.iter().min().cloned())
    .or_else(|| mode(classified.iter().map(|class| class.0.clone())))
    .unwrap_or_else(|| "UNKNOWN".into())
}

fn record_matches_category(domain: &DomainTag, category: &str) -> bool {
    match domain {
        DomainTag::MacroInterestRate => category == "ECONOMICS" || category == "FINANCE",
        DomainTag::Geopolitics | DomainTag::Elections => category == "POLITICS",
        DomainTag::TechRegulation => category == "TECH",
        DomainTag::Crypto => category == "CRYPTO",
        DomainTag::ScienceClimate => category == "WEATHER",
        DomainTag::PopCultureSports => category == "SPORTS" || category == "CULTURE",
        DomainTag::General | DomainTag::Custom(_) => category == "UNKNOWN",
    }
}

fn normalize_address(value: &str) -> Result<String> {
    let value = value.trim().to_ascii_lowercase();
    if value.len() == 42
        && value.starts_with("0x")
        && value[2..].chars().all(|c| c.is_ascii_hexdigit())
    {
        Ok(value)
    } else {
        anyhow::bail!("invalid EVM wallet address '{value}'")
    }
}
fn normalize_label(value: &str) -> String {
    value.trim().to_ascii_lowercase().replace(['_', ' '], "-")
}
fn mode(values: impl Iterator<Item = String>) -> Option<String> {
    let mut counts = BTreeMap::<String, usize>::new();
    for value in values {
        *counts.entry(value).or_default() += 1;
    }
    counts
        .into_iter()
        .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(&a.0)))
        .map(|x| x.0)
}
fn ratio(n: usize, d: usize) -> f64 {
    if d == 0 {
        0.0
    } else {
        n as f64 / d as f64
    }
}
fn bounded_ratio(n: usize, target: usize) -> f64 {
    if target == 0 {
        1.0
    } else {
        (n as f64 / target as f64).clamp(0.0, 1.0)
    }
}
fn mean(values: &[f64]) -> f64 {
    if values.is_empty() {
        0.0
    } else {
        values.iter().sum::<f64>() / values.len() as f64
    }
}
fn median(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        None
    } else if values.len() % 2 == 1 {
        Some(values[values.len() / 2])
    } else {
        Some((values[values.len() / 2 - 1] + values[values.len() / 2]) / 2.0)
    }
}
fn scale_positive(value: f64, floor: f64, strong: f64) -> f64 {
    ((value - floor) / (strong - floor).max(0.001)).clamp(0.0, 1.0)
}
fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}
fn round4(value: f64) -> f64 {
    (value * 10_000.0).round() / 10_000.0
}
fn gate<T: fmt::Display, U: fmt::Display>(
    out: &mut Vec<String>,
    passed: bool,
    code: &str,
    actual: T,
    threshold: U,
) {
    if !passed {
        out.push(format!("{code}: actual={actual}, threshold={threshold}"));
    }
}
fn csv_escape(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}
fn atomic_write(path: &Path, content: &str) -> Result<()> {
    let tmp = path.with_extension(format!(
        "{}.tmp",
        path.extension().and_then(|x| x.to_str()).unwrap_or("out")
    ));
    std::fs::write(&tmp, content).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replace {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn taxonomy_identifies_secondary_categories() {
        assert_eq!(
            classify_market("Will a hurricane hit Florida?"),
            ("WEATHER".into(), "storm-hurricane".into())
        );
        assert_eq!(
            classify_market("Will the Fed cut interest rates?"),
            ("ECONOMICS".into(), "interest-rates".into())
        );
        assert_eq!(
            classify_market("Who wins the presidential election?"),
            ("POLITICS".into(), "elections".into())
        );
    }

    #[test]
    fn profiles_have_distinct_hard_gates() {
        let conservative = ScoutConfig::conservative();
        let research = ScoutConfig::research();
        assert!(conservative.min_resolved_markets > research.min_resolved_markets);
        assert!(conservative.min_recent_trades > research.min_recent_trades);
        assert!(conservative.validate().is_ok());
        assert!(research.validate().is_ok());
    }

    #[test]
    fn address_validation_is_strict() {
        assert!(normalize_address("0x1111111111111111111111111111111111111111").is_ok());
        assert!(normalize_address("0x1234").is_err());
    }
}
