//! Window-end winner-candidate buy strategy.
//!
//! Near the end of an up/down window, a high ask on one side and a cheap or
//! absent opposite ask can identify a directional candidate. This is a
//! single-leg trade with loss risk, not a YES+NO arbitrage; a fill remains
//! pending until the market is verifiably closed and resolved.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::late_resolution_winner_model::ModelEvaluation;

/// Check freshness against the time of the check, not against a timestamp
/// captured before an asynchronous REST request began.
pub fn received_at_is_fresh(received_at_ms: i64, checked_at_ms: i64, max_age_ms: i64) -> bool {
    checked_at_ms
        .checked_sub(received_at_ms)
        .is_some_and(|age| age >= 0 && age <= max_age_ms)
}

/// Refresh missing books unconditionally. For complete but stale WS books,
/// REST is only needed when the cached pair still indicates a possible winner.
/// A no-signal book cannot become a candidate without a WS update while the
/// market stream is healthy, so this avoids refreshing both legs on every scan.
pub fn winner_books_need_rest(
    stream_healthy: bool,
    yes_present: bool,
    no_present: bool,
    yes_fresh: bool,
    no_fresh: bool,
    preliminary_signal: bool,
) -> bool {
    if !stream_healthy {
        return true;
    }
    if yes_fresh && no_fresh {
        return false;
    }
    if !yes_present || !no_present {
        return true;
    }
    preliminary_signal
}

#[derive(Debug, Clone, Copy)]
struct SettlementRetry {
    attempts: u32,
    next_query_at_ms: i64,
}

/// Per-market exponential retry schedule for Gamma settlement visibility.
/// The state is deliberately in-memory: journal replay still remains the
/// source of truth after a process restart.
#[derive(Debug, Default)]
pub struct SettlementBackoff {
    retries: HashMap<String, SettlementRetry>,
}

impl SettlementBackoff {
    pub fn should_query(&self, slug: &str, now_ms: i64) -> bool {
        self.retries
            .get(slug)
            .is_none_or(|retry| now_ms >= retry.next_query_at_ms)
    }

    pub fn record_retry(&mut self, slug: &str, now_ms: i64) -> u64 {
        let attempts = self
            .retries
            .get(slug)
            .map(|retry| retry.attempts.saturating_add(1))
            .unwrap_or(1);
        let delay_secs = 10_u64
            .saturating_mul(1_u64 << attempts.saturating_sub(1).min(3))
            .min(60);
        self.retries.insert(
            slug.to_string(),
            SettlementRetry {
                attempts,
                next_query_at_ms: now_ms
                    .saturating_add(i64::try_from(delay_secs * 1_000).unwrap_or(60_000)),
            },
        );
        delay_secs
    }

    pub fn clear(&mut self, slug: &str) {
        self.retries.remove(slug);
    }
}

/// Result of a winner-direction scan for one market.
#[derive(Debug, Clone, PartialEq)]
pub struct WinnerSignal {
    /// True = buy YES side, false = buy NO side.
    pub buy_yes: bool,
    /// Best ask of the winning side, used as the limit price.
    pub price: Decimal,
}

/// A paper or live late-resolution-winner-trading position awaiting Polymarket settlement.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PendingLateResolutionWinnerTrading {
    /// Stable idempotency key for persistence and settlement.
    pub position_id: String,
    /// Links the actual order lifecycle to its counterfactual model evaluation.
    #[serde(default)]
    pub evaluation_id: String,
    /// `production` or `hybrid_paper`; empty for legacy journal records.
    #[serde(default)]
    pub candidate_stage: String,
    /// Asset bucket used by the independent per-asset loss circuit breaker.
    #[serde(default)]
    pub asset: String,
    /// Market slug used to re-query gamma for the settlement result.
    pub slug: String,
    /// CTF condition required for an eventual on-chain redemption.
    #[serde(default)]
    pub condition_id: String,
    /// Whether this market uses the NegRisk redemption path.
    #[serde(default)]
    pub neg_risk: bool,
    /// Bought token, retained so wind-down can protect this hold-to-settlement position.
    pub token_id: String,
    /// True if the bought token is the YES (Up) side.
    pub token_is_yes: bool,
    /// Requested shares and pre-trade limit retained for order-attempt audit.
    #[serde(default)]
    pub requested: Decimal,
    #[serde(default)]
    pub limit_price: Decimal,
    #[serde(default)]
    pub planned_cost: Decimal,
    /// Independent Chainlink model inputs/outputs captured at order admission.
    #[serde(default)]
    pub opening_twap: Decimal,
    #[serde(default)]
    pub current_twap: Decimal,
    #[serde(default)]
    pub model_observed_at_ms: i64,
    #[serde(default)]
    pub model_probability: Decimal,
    #[serde(default)]
    pub model_net_ev: Decimal,
    #[serde(default)]
    pub directional_margin_bps: Decimal,
    #[serde(default)]
    pub reversal_boundary_bps: Decimal,
    #[serde(default)]
    pub book_spread: Decimal,
    #[serde(default)]
    pub book_depth_usdc: Decimal,
    pub filled: Decimal,
    pub price: Decimal,
    /// Fee included in the effective entry price when known/estimated.
    #[serde(default)]
    pub fee: Decimal,
    /// Scheduled market end. A live market must never be treated as resolved.
    pub end_date: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WinnerEvaluation {
    pub evaluation_id: String,
    pub asset: String,
    pub slug: String,
    pub end_date: DateTime<Utc>,
    pub evaluated_at: DateTime<Utc>,
    pub window_start_ms: i64,
    pub seconds_remaining: i64,
    pub buy_yes: bool,
    pub bid_price: Decimal,
    pub ask_price: Decimal,
    pub ask_size: Decimal,
    pub fee_per_share: Decimal,
    pub rejection: Option<String>,
    #[serde(default)]
    pub failed_checks: Vec<String>,
    pub opening_twap: Option<Decimal>,
    pub current_twap: Option<Decimal>,
    pub model_observed_at_ms: Option<i64>,
    pub model_probability: Decimal,
    pub model_net_ev: Decimal,
    #[serde(default)]
    pub effective_entry_price: Decimal,
    #[serde(default)]
    pub probability_edge_before_risk: Decimal,
    #[serde(default)]
    pub required_probability: Decimal,
    #[serde(default)]
    pub reversal_margin_ratio: f64,
    #[serde(default)]
    pub ask_price_bucket: String,
    #[serde(default)]
    pub seconds_remaining_bucket: String,
    #[serde(default)]
    pub book_source: String,
    #[serde(default)]
    pub book_age_ms: i64,
    /// Overlapping research cohorts. They never affect production admission.
    #[serde(default)]
    pub research_cohorts: Vec<String>,
    pub directional_margin_bps: f64,
    pub reversal_boundary_bps: f64,
    pub volatility_bps_per_sqrt_second: f64,
    pub spread: Decimal,
    pub depth_usdc: Decimal,
}

/// Delayed, read-only execution result for a rejected Late-Resolution Winner Trading candidate.
/// This is research data only: it never reserves paper cash or creates an
/// order/position lifecycle entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WinnerShadowExecution {
    pub evaluation_id: String,
    pub slug: String,
    pub simulated_at: DateTime<Utc>,
    pub delay_ms: u64,
    /// `rejected_confirmed` for new research records; older `rejected` and
    /// `awaiting_confirmation` records remain readable.
    #[serde(default = "default_shadow_candidate_stage")]
    pub candidate_stage: String,
    /// Cohorts copied from the originating evaluation for market-level reports.
    #[serde(default)]
    pub research_cohorts: Vec<String>,
    pub outcome: String,
    pub limit_price: Decimal,
    pub requested: Decimal,
    pub filled: Decimal,
    /// Fee-inclusive cost and average price, matching normal paper Late-Resolution Winner Trading.
    pub cost: Decimal,
    pub average_price: Decimal,
    pub estimated_fee: Decimal,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

fn default_shadow_candidate_stage() -> String {
    "rejected".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WinnerConfirmationObservation {
    pub evaluation_id: String,
    pub slug: String,
    pub recorded_at: DateTime<Utc>,
    pub model_observed_at_ms: i64,
    pub confirmations: u32,
    pub required: u32,
    pub confirmed: bool,
    /// `production`, `hybrid_paper`, or `rejected_shadow`; legacy records deserialize empty.
    #[serde(default)]
    pub candidate_stage: String,
}

impl WinnerEvaluation {
    #[allow(clippy::too_many_arguments)]
    pub fn from_model(
        evaluation_id: String,
        asset: String,
        slug: String,
        end_date: DateTime<Utc>,
        evaluated_at: DateTime<Utc>,
        window_start_ms: i64,
        seconds_remaining: i64,
        buy_yes: bool,
        bid_price: Decimal,
        ask_price: Decimal,
        ask_size: Decimal,
        fee_per_share: Decimal,
        book_source: String,
        book_age_ms: i64,
        model: &ModelEvaluation,
    ) -> Self {
        Self {
            evaluation_id,
            asset,
            slug,
            end_date,
            evaluated_at,
            window_start_ms,
            seconds_remaining,
            buy_yes,
            bid_price,
            ask_price,
            ask_size,
            fee_per_share,
            rejection: model.rejection.map(|reason| format!("{reason:?}")),
            failed_checks: model
                .failed_checks
                .iter()
                .map(|reason| format!("{reason:?}"))
                .collect(),
            opening_twap: model.opening_twap,
            current_twap: model.current_twap,
            model_observed_at_ms: model.observed_at_ms,
            model_probability: model.probability,
            model_net_ev: model.net_ev,
            effective_entry_price: model.effective_price,
            probability_edge_before_risk: model.probability_edge_before_risk,
            required_probability: model.required_probability,
            reversal_margin_ratio: if model.reversal_boundary_bps > 0.0 {
                model.directional_margin_bps / model.reversal_boundary_bps
            } else {
                0.0
            },
            ask_price_bucket: ask_price.normalize().to_string(),
            seconds_remaining_bucket: seconds_remaining_bucket(seconds_remaining).to_string(),
            book_source,
            book_age_ms,
            research_cohorts: Vec::new(),
            directional_margin_bps: model.directional_margin_bps,
            reversal_boundary_bps: model.reversal_boundary_bps,
            volatility_bps_per_sqrt_second: model.volatility_bps_per_sqrt_second,
            spread: model.spread,
            depth_usdc: model.depth_notional,
        }
    }
}

/// Classify an evaluation into overlapping, read-only research cohorts. The
/// hybrid cohort treats Chainlink as a directional veto instead of using its
/// diffusion probability as the absolute source of truth.
pub fn winner_research_cohorts(
    evaluation: &WinnerEvaluation,
    max_spread: Decimal,
    min_depth_usdc: Decimal,
) -> Vec<String> {
    let mut cohorts = vec!["all_candidates".to_string()];
    let asset = evaluation.asset.trim().to_ascii_lowercase();
    cohorts.push(format!("asset_{asset}"));
    cohorts.push(format!(
        "price_{}",
        evaluation
            .ask_price
            .normalize()
            .to_string()
            .replace('.', "_")
    ));
    let preferred_price = (dec!(0.96)..=dec!(0.98)).contains(&evaluation.ask_price);
    if preferred_price {
        cohorts.push("price_096_098".to_string());
    }
    let has_chainlink_model = evaluation.opening_twap.is_some()
        && evaluation.current_twap.is_some()
        && evaluation.model_observed_at_ms.is_some();
    let direction_agrees = has_chainlink_model && evaluation.directional_margin_bps > 0.0;
    if direction_agrees {
        cohorts.push("chainlink_direction_agrees".to_string());
    }
    let positive_win_payoff =
        evaluation.effective_entry_price < dec!(1) && evaluation.fee_per_share >= dec!(0);
    if preferred_price
        && direction_agrees
        && positive_win_payoff
        && evaluation.spread <= max_spread
        && evaluation.depth_usdc >= min_depth_usdc
    {
        cohorts.push("hybrid_chainlink_veto".to_string());
    }
    if evaluation.rejection.is_none() {
        cohorts.push("production_model".to_string());
    }
    cohorts
}

fn seconds_remaining_bucket(seconds: i64) -> &'static str {
    match seconds {
        ..=20 => "00-20s",
        21..=40 => "21-40s",
        41..=60 => "41-60s",
        _ => "61s+",
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum WinnerEvaluationEvent {
    Evaluated {
        version: u8,
        evaluation: Box<WinnerEvaluation>,
    },
    Resolved {
        version: u8,
        evaluation_id: String,
        slug: String,
        won: bool,
        counterfactual_pnl_per_share: Decimal,
        resolved_at: DateTime<Utc>,
    },
    /// One record per slug, so profitability reports do not accidentally
    /// overweight markets that were evaluated more than once.
    MarketResolved {
        version: u8,
        slug: String,
        evaluation_count: usize,
        selected_evaluation_id: String,
        buy_yes: bool,
        won: bool,
        counterfactual_pnl_per_share: Decimal,
        resolved_at: DateTime<Utc>,
    },
    ShadowExecuted {
        version: u8,
        execution: Box<WinnerShadowExecution>,
    },
    ConfirmationObserved {
        version: u8,
        observation: Box<WinnerConfirmationObservation>,
    },
}

/// Append-only counterfactual dataset. Unlike the position journal, it keeps
/// rejected/no-fill candidates so thresholds can later be calibrated without
/// survivorship bias.
pub struct WinnerEvaluationJournal {
    path: PathBuf,
    writer: Mutex<File>,
    unresolved: Mutex<HashMap<String, WinnerEvaluation>>,
}

impl WinnerEvaluationJournal {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)?;
        }
        let mut unresolved = HashMap::new();
        if path.exists() {
            let lines = BufReader::new(File::open(&path)?)
                .lines()
                .collect::<std::io::Result<Vec<_>>>()?;
            let last_record = lines.iter().rposition(|line| !line.trim().is_empty());
            for (index, line) in lines.iter().enumerate() {
                if line.trim().is_empty() {
                    continue;
                }
                let event: WinnerEvaluationEvent = match serde_json::from_str(line) {
                    Ok(event) => event,
                    Err(error) if Some(index) == last_record => {
                        quarantine_incomplete_tail(&path, &lines, index)?;
                        tracing::warn!(path = %path.display(), line = index + 1, %error, "Quarantined incomplete Late-Resolution Winner Trading evaluation journal tail");
                        break;
                    }
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!(
                                "invalid Late-Resolution Winner Trading evaluation record at {}:{}",
                                path.display(),
                                index + 1
                            )
                        });
                    }
                };
                match event {
                    WinnerEvaluationEvent::Evaluated {
                        version: 1,
                        evaluation,
                    } => {
                        unresolved.insert(evaluation.evaluation_id.clone(), *evaluation);
                    }
                    WinnerEvaluationEvent::Resolved {
                        version: 1,
                        evaluation_id,
                        ..
                    } => {
                        unresolved.remove(&evaluation_id);
                    }
                    WinnerEvaluationEvent::MarketResolved { version: 1, .. } => {}
                    WinnerEvaluationEvent::ShadowExecuted { version: 1, .. } => {}
                    WinnerEvaluationEvent::ConfirmationObserved { version: 1, .. } => {}
                    _ => anyhow::bail!(
                        "unsupported Late-Resolution Winner Trading evaluation journal version"
                    ),
                }
            }
        }
        let writer = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Self {
            path,
            writer: Mutex::new(writer),
            unresolved: Mutex::new(unresolved),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn record(&self, evaluation: WinnerEvaluation) -> Result<()> {
        self.append(&WinnerEvaluationEvent::Evaluated {
            version: 1,
            evaluation: Box::new(evaluation.clone()),
        })?;
        self.unresolved
            .lock()
            .expect("Late-Resolution Winner Trading evaluation lock poisoned")
            .insert(evaluation.evaluation_id.clone(), evaluation);
        Ok(())
    }

    pub fn record_shadow_execution(&self, execution: WinnerShadowExecution) -> Result<()> {
        self.append(&WinnerEvaluationEvent::ShadowExecuted {
            version: 1,
            execution: Box::new(execution),
        })
    }

    pub fn record_confirmation_observation(
        &self,
        observation: WinnerConfirmationObservation,
    ) -> Result<()> {
        self.append(&WinnerEvaluationEvent::ConfirmationObserved {
            version: 1,
            observation: Box::new(observation),
        })
    }

    pub fn due_slugs(&self, now: DateTime<Utc>) -> Vec<String> {
        let mut slugs: Vec<_> = self
            .unresolved
            .lock()
            .expect("Late-Resolution Winner Trading evaluation lock poisoned")
            .values()
            .filter(|evaluation| now >= evaluation.end_date)
            .map(|evaluation| evaluation.slug.clone())
            .collect();
        slugs.sort();
        slugs.dedup();
        slugs
    }

    pub fn record_resolution(
        &self,
        slug: &str,
        now: DateTime<Utc>,
        market_closed: bool,
        outcome_prices: &(Decimal, Decimal),
    ) -> Result<usize> {
        const TERMINAL_HIGH: Decimal = dec!(0.999);
        const TERMINAL_LOW: Decimal = dec!(0.001);
        if !market_closed {
            return Ok(0);
        }
        let yes_won = outcome_prices.0 >= TERMINAL_HIGH && outcome_prices.1 <= TERMINAL_LOW;
        let no_won = outcome_prices.1 >= TERMINAL_HIGH && outcome_prices.0 <= TERMINAL_LOW;
        if yes_won == no_won {
            return Ok(0);
        }
        let matches: Vec<_> = self
            .unresolved
            .lock()
            .expect("Late-Resolution Winner Trading evaluation lock poisoned")
            .values()
            .filter(|evaluation| evaluation.slug == slug && now >= evaluation.end_date)
            .cloned()
            .collect();
        for evaluation in &matches {
            let won = if evaluation.buy_yes { yes_won } else { no_won };
            let effective_price = evaluation.ask_price + evaluation.fee_per_share;
            let pnl = if won {
                dec!(1) - effective_price
            } else {
                -effective_price
            };
            self.append(&WinnerEvaluationEvent::Resolved {
                version: 1,
                evaluation_id: evaluation.evaluation_id.clone(),
                slug: slug.to_string(),
                won,
                counterfactual_pnl_per_share: pnl,
                resolved_at: now,
            })?;
        }
        if let Some(selected) = matches
            .iter()
            .max_by_key(|evaluation| evaluation.evaluated_at)
        {
            let won = if selected.buy_yes { yes_won } else { no_won };
            let effective_price = selected.ask_price + selected.fee_per_share;
            let pnl = if won {
                dec!(1) - effective_price
            } else {
                -effective_price
            };
            self.append(&WinnerEvaluationEvent::MarketResolved {
                version: 1,
                slug: slug.to_string(),
                evaluation_count: matches.len(),
                selected_evaluation_id: selected.evaluation_id.clone(),
                buy_yes: selected.buy_yes,
                won,
                counterfactual_pnl_per_share: pnl,
                resolved_at: now,
            })?;
        }
        let mut unresolved = self
            .unresolved
            .lock()
            .expect("Late-Resolution Winner Trading evaluation lock poisoned");
        for evaluation in &matches {
            unresolved.remove(&evaluation.evaluation_id);
        }
        Ok(matches.len())
    }

    fn append(&self, event: &WinnerEvaluationEvent) -> Result<()> {
        let encoded = serde_json::to_string(event)?;
        let mut writer = self
            .writer
            .lock()
            .expect("Late-Resolution Winner Trading evaluation journal lock poisoned");
        writeln!(writer, "{encoded}")?;
        writer.flush()?;
        writer.sync_data()?;
        Ok(())
    }
}

impl PendingLateResolutionWinnerTrading {
    pub fn asset_key(&self) -> String {
        if !self.asset.trim().is_empty() {
            return self.asset.trim().to_ascii_lowercase();
        }
        self.slug
            .split('-')
            .next()
            .unwrap_or("unknown")
            .to_ascii_lowercase()
    }
}

/// Settle a late-resolution-winner-trading position against the market's outcome prices.
/// Returns the realized PnL (positive = win, negative = loss).
/// `outcome_prices` is the (YES, NO) settlement pair from gamma: the winner
/// side is ~1.0 and the loser ~0.0 after the market closes.
pub fn settle_late_resolution_winner_trading(
    buy: &PendingLateResolutionWinnerTrading,
    now: DateTime<Utc>,
    market_closed: bool,
    outcome_prices: &(Decimal, Decimal),
) -> Option<Decimal> {
    const TERMINAL_HIGH: Decimal = dec!(0.999);
    const TERMINAL_LOW: Decimal = dec!(0.001);

    if now < buy.end_date || !market_closed {
        return None;
    }

    let yes_won = outcome_prices.0 >= TERMINAL_HIGH && outcome_prices.1 <= TERMINAL_LOW;
    let no_won = outcome_prices.1 >= TERMINAL_HIGH && outcome_prices.0 <= TERMINAL_LOW;
    if yes_won == no_won {
        return None;
    }

    let won = if buy.token_is_yes { yes_won } else { no_won };
    Some(if won {
        buy.filled * (dec!(1) - buy.price)
    } else {
        -buy.filled * buy.price
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum WinnerJournalAction {
    Attempted,
    NoFill,
    Failed,
    Pending,
    Settled,
    RedeemSubmitted,
    Redeemed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WinnerJournalEvent {
    version: u8,
    action: WinnerJournalAction,
    position: PendingLateResolutionWinnerTrading,
    #[serde(default)]
    realized_pnl: Option<Decimal>,
    #[serde(default)]
    reference: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WinnerLifecycleState {
    Attempted,
    NoFill,
    Failed,
    Pending,
    Settled,
    RedeemSubmitted,
    Redeemed,
}

#[derive(Debug, Clone, Copy)]
pub struct LateResolutionWinnerTradingRiskPolicy {
    pub max_strategy_loss: Decimal,
    pub max_asset_loss: Decimal,
    pub max_consecutive_losses: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LateResolutionWinnerTradingRiskSnapshot {
    pub strategy_loss: Decimal,
    pub asset_loss: HashMap<String, Decimal>,
    pub consecutive_losses: u32,
}

#[derive(Default)]
struct LateResolutionWinnerTradingRiskState {
    strategy_loss: Decimal,
    asset_loss: HashMap<String, Decimal>,
    consecutive_losses: u32,
}

/// Append-only state for hold-to-settlement positions. The in-memory pending
/// map is rebuilt by replaying the journal on startup.
pub struct LateResolutionWinnerTradingJournal {
    path: PathBuf,
    writer: Mutex<File>,
    pending: Mutex<HashMap<String, PendingLateResolutionWinnerTrading>>,
    awaiting_redeem: Mutex<HashMap<String, PendingLateResolutionWinnerTrading>>,
    states: Mutex<HashMap<String, WinnerLifecycleState>>,
    total_spent: Mutex<Decimal>,
    realized_pnl: Mutex<Decimal>,
    risk: Mutex<LateResolutionWinnerTradingRiskState>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LateResolutionWinnerTradingPaperSummary {
    pub pending_cost: Decimal,
    pub total_spent: Decimal,
    pub realized_pnl: Decimal,
    pub open_positions: u64,
}

struct WinnerJournalReplay {
    pending: HashMap<String, PendingLateResolutionWinnerTrading>,
    awaiting_redeem: HashMap<String, PendingLateResolutionWinnerTrading>,
    states: HashMap<String, WinnerLifecycleState>,
    total_spent: Decimal,
    realized_pnl: Decimal,
    risk: LateResolutionWinnerTradingRiskState,
}

impl LateResolutionWinnerTradingJournal {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed to create late-resolution-winner-trading journal directory {}",
                    parent.display()
                )
            })?;
        }
        let replay = replay_journal(&path)?;
        let writer = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| {
                format!(
                    "failed to open late-resolution-winner-trading journal {}",
                    path.display()
                )
            })?;
        Ok(Self {
            path,
            writer: Mutex::new(writer),
            pending: Mutex::new(replay.pending),
            awaiting_redeem: Mutex::new(replay.awaiting_redeem),
            states: Mutex::new(replay.states),
            total_spent: Mutex::new(replay.total_spent),
            realized_pnl: Mutex::new(replay.realized_pnl),
            risk: Mutex::new(replay.risk),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn pending(&self) -> Vec<PendingLateResolutionWinnerTrading> {
        self.pending
            .lock()
            .expect("late-resolution-winner-trading pending lock poisoned")
            .values()
            .cloned()
            .collect()
    }

    pub fn awaiting_redeem(&self) -> Vec<PendingLateResolutionWinnerTrading> {
        self.awaiting_redeem
            .lock()
            .expect("late-resolution-winner-trading redeem lock poisoned")
            .values()
            .cloned()
            .collect()
    }

    pub fn uncertain_redeem_count(&self) -> usize {
        self.states
            .lock()
            .expect("late-resolution-winner-trading state lock poisoned")
            .values()
            .filter(|state| **state == WinnerLifecycleState::RedeemSubmitted)
            .count()
    }

    pub fn risk_snapshot(&self) -> LateResolutionWinnerTradingRiskSnapshot {
        let risk = self
            .risk
            .lock()
            .expect("late-resolution-winner-trading risk lock poisoned");
        LateResolutionWinnerTradingRiskSnapshot {
            strategy_loss: risk.strategy_loss,
            asset_loss: risk.asset_loss.clone(),
            consecutive_losses: risk.consecutive_losses,
        }
    }

    pub fn check_risk(
        &self,
        asset: &str,
        policy: LateResolutionWinnerTradingRiskPolicy,
    ) -> std::result::Result<(), String> {
        let asset = asset.trim().to_ascii_lowercase();
        let risk = self
            .risk
            .lock()
            .expect("late-resolution-winner-trading risk lock poisoned");
        if risk.strategy_loss >= policy.max_strategy_loss {
            return Err(format!(
                "late-resolution-winner-trading strategy loss {} reached {}",
                risk.strategy_loss, policy.max_strategy_loss
            ));
        }
        let asset_loss = risk.asset_loss.get(&asset).copied().unwrap_or(dec!(0));
        if asset_loss >= policy.max_asset_loss {
            return Err(format!(
                "late-resolution-winner-trading {asset} loss {asset_loss} reached {}",
                policy.max_asset_loss
            ));
        }
        if policy.max_consecutive_losses > 0
            && risk.consecutive_losses >= policy.max_consecutive_losses
        {
            return Err(format!(
                "late-resolution-winner-trading consecutive losses {} reached {}",
                risk.consecutive_losses, policy.max_consecutive_losses
            ));
        }
        Ok(())
    }

    pub fn paper_summary(&self) -> LateResolutionWinnerTradingPaperSummary {
        let pending = self
            .pending
            .lock()
            .expect("late-resolution-winner-trading pending lock poisoned");
        LateResolutionWinnerTradingPaperSummary {
            pending_cost: pending.values().map(|buy| buy.filled * buy.price).sum(),
            total_spent: *self
                .total_spent
                .lock()
                .expect("late-resolution-winner-trading spent lock poisoned"),
            realized_pnl: *self
                .realized_pnl
                .lock()
                .expect("late-resolution-winner-trading pnl lock poisoned"),
            open_positions: pending.len() as u64,
        }
    }

    pub fn record_pending(&self, position: PendingLateResolutionWinnerTrading) -> Result<bool> {
        if matches!(
            self.states
                .lock()
                .expect("late-resolution-winner-trading state lock poisoned")
                .get(&position.position_id),
            Some(
                WinnerLifecycleState::NoFill
                    | WinnerLifecycleState::Failed
                    | WinnerLifecycleState::Settled
                    | WinnerLifecycleState::RedeemSubmitted
                    | WinnerLifecycleState::Redeemed
            )
        ) {
            return Ok(false);
        }
        if self
            .pending
            .lock()
            .expect("late-resolution-winner-trading pending lock poisoned")
            .contains_key(&position.position_id)
        {
            return Ok(false);
        }
        self.append(&WinnerJournalEvent {
            version: 1,
            action: WinnerJournalAction::Pending,
            position: position.clone(),
            realized_pnl: None,
            reference: None,
        })?;
        let position_id = position.position_id.clone();
        let cost = position.filled * position.price;
        self.pending
            .lock()
            .expect("late-resolution-winner-trading pending lock poisoned")
            .insert(position_id.clone(), position);
        *self
            .total_spent
            .lock()
            .expect("late-resolution-winner-trading spent lock poisoned") += cost;
        self.states
            .lock()
            .expect("late-resolution-winner-trading state lock poisoned")
            .insert(position_id, WinnerLifecycleState::Pending);
        Ok(true)
    }

    pub fn record_attempt(&self, position: PendingLateResolutionWinnerTrading) -> Result<bool> {
        let mut states = self
            .states
            .lock()
            .expect("late-resolution-winner-trading state lock poisoned");
        if states.contains_key(&position.position_id) {
            return Ok(false);
        }
        self.append(&WinnerJournalEvent {
            version: 1,
            action: WinnerJournalAction::Attempted,
            position: position.clone(),
            realized_pnl: None,
            reference: None,
        })?;
        states.insert(
            position.position_id.clone(),
            WinnerLifecycleState::Attempted,
        );
        Ok(true)
    }

    pub fn record_no_fill(&self, position: &PendingLateResolutionWinnerTrading) -> Result<bool> {
        self.record_terminal_attempt(
            position,
            WinnerJournalAction::NoFill,
            WinnerLifecycleState::NoFill,
            None,
        )
    }

    pub fn record_failed(
        &self,
        position: &PendingLateResolutionWinnerTrading,
        error: &str,
    ) -> Result<bool> {
        self.record_terminal_attempt(
            position,
            WinnerJournalAction::Failed,
            WinnerLifecycleState::Failed,
            Some(error.to_string()),
        )
    }

    fn record_terminal_attempt(
        &self,
        position: &PendingLateResolutionWinnerTrading,
        action: WinnerJournalAction,
        target: WinnerLifecycleState,
        reference: Option<String>,
    ) -> Result<bool> {
        let mut states = self
            .states
            .lock()
            .expect("late-resolution-winner-trading state lock poisoned");
        if states.get(&position.position_id) != Some(&WinnerLifecycleState::Attempted) {
            return Ok(false);
        }
        self.append(&WinnerJournalEvent {
            version: 1,
            action,
            position: position.clone(),
            realized_pnl: None,
            reference,
        })?;
        states.insert(position.position_id.clone(), target);
        Ok(true)
    }

    /// Persist the terminal event before removing the pending position. A
    /// repeated settlement response therefore cannot be counted twice.
    pub fn record_settled(
        &self,
        position: &PendingLateResolutionWinnerTrading,
        pnl: Decimal,
    ) -> Result<bool> {
        if !self
            .pending
            .lock()
            .expect("late-resolution-winner-trading pending lock poisoned")
            .contains_key(&position.position_id)
        {
            return Ok(false);
        }
        self.append(&WinnerJournalEvent {
            version: 1,
            action: WinnerJournalAction::Settled,
            position: position.clone(),
            realized_pnl: Some(pnl),
            reference: None,
        })?;
        self.pending
            .lock()
            .expect("late-resolution-winner-trading pending lock poisoned")
            .remove(&position.position_id);
        *self
            .realized_pnl
            .lock()
            .expect("late-resolution-winner-trading pnl lock poisoned") += pnl;
        if pnl > dec!(0) {
            self.awaiting_redeem
                .lock()
                .expect("late-resolution-winner-trading redeem lock poisoned")
                .insert(position.position_id.clone(), position.clone());
        }
        self.states
            .lock()
            .expect("late-resolution-winner-trading state lock poisoned")
            .insert(position.position_id.clone(), WinnerLifecycleState::Settled);
        record_risk_result(
            &mut self
                .risk
                .lock()
                .expect("late-resolution-winner-trading risk lock poisoned"),
            position,
            pnl,
        );
        Ok(true)
    }

    pub fn record_redeem_submitted(
        &self,
        position: &PendingLateResolutionWinnerTrading,
    ) -> Result<bool> {
        let mut states = self
            .states
            .lock()
            .expect("late-resolution-winner-trading state lock poisoned");
        if states.get(&position.position_id) != Some(&WinnerLifecycleState::Settled) {
            return Ok(false);
        }
        self.append(&WinnerJournalEvent {
            version: 1,
            action: WinnerJournalAction::RedeemSubmitted,
            position: position.clone(),
            realized_pnl: None,
            reference: None,
        })?;
        states.insert(
            position.position_id.clone(),
            WinnerLifecycleState::RedeemSubmitted,
        );
        Ok(true)
    }

    pub fn record_redeemed(
        &self,
        position: &PendingLateResolutionWinnerTrading,
        reference: String,
    ) -> Result<bool> {
        let mut states = self
            .states
            .lock()
            .expect("late-resolution-winner-trading state lock poisoned");
        if !matches!(
            states.get(&position.position_id),
            Some(WinnerLifecycleState::Settled | WinnerLifecycleState::RedeemSubmitted)
        ) {
            return Ok(false);
        }
        self.append(&WinnerJournalEvent {
            version: 1,
            action: WinnerJournalAction::Redeemed,
            position: position.clone(),
            realized_pnl: None,
            reference: Some(reference),
        })?;
        self.awaiting_redeem
            .lock()
            .expect("late-resolution-winner-trading redeem lock poisoned")
            .remove(&position.position_id);
        states.insert(position.position_id.clone(), WinnerLifecycleState::Redeemed);
        Ok(true)
    }

    fn append(&self, event: &WinnerJournalEvent) -> Result<()> {
        let encoded = serde_json::to_string(event)?;
        let mut writer = self
            .writer
            .lock()
            .expect("late-resolution-winner-trading journal lock poisoned");
        writeln!(writer, "{encoded}")?;
        writer.flush()?;
        writer.sync_data()?;
        Ok(())
    }
}

fn replay_journal(path: &Path) -> Result<WinnerJournalReplay> {
    if !path.exists() {
        return Ok(WinnerJournalReplay {
            pending: HashMap::new(),
            awaiting_redeem: HashMap::new(),
            states: HashMap::new(),
            total_spent: dec!(0),
            realized_pnl: dec!(0),
            risk: LateResolutionWinnerTradingRiskState::default(),
        });
    }
    let lines = BufReader::new(File::open(path)?)
        .lines()
        .collect::<std::io::Result<Vec<_>>>()?;
    let last_record = lines.iter().rposition(|line| !line.trim().is_empty());
    let mut pending = HashMap::new();
    let mut awaiting_redeem = HashMap::new();
    let mut states = HashMap::new();
    let mut seen_positions = HashSet::new();
    let mut settled_positions = HashSet::new();
    let mut total_spent = dec!(0);
    let mut realized_pnl = dec!(0);
    let mut risk = LateResolutionWinnerTradingRiskState::default();
    for (index, line) in lines.iter().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let event: WinnerJournalEvent = match serde_json::from_str(line) {
            Ok(event) => event,
            Err(error) if Some(index) == last_record => {
                quarantine_incomplete_tail(path, &lines, index)?;
                tracing::warn!(
                    path = %path.display(),
                    line = index + 1,
                    %error,
                    "Quarantined incomplete late-resolution-winner-trading journal tail"
                );
                break;
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "invalid late-resolution-winner-trading journal record at {}:{}",
                        path.display(),
                        index + 1
                    )
                });
            }
        };
        if event.version != 1 {
            anyhow::bail!(
                "unsupported late-resolution-winner-trading journal version {}",
                event.version
            );
        }
        match event.action {
            WinnerJournalAction::Attempted => {
                states.insert(
                    event.position.position_id.clone(),
                    WinnerLifecycleState::Attempted,
                );
            }
            WinnerJournalAction::NoFill => {
                states.insert(
                    event.position.position_id.clone(),
                    WinnerLifecycleState::NoFill,
                );
            }
            WinnerJournalAction::Failed => {
                states.insert(
                    event.position.position_id.clone(),
                    WinnerLifecycleState::Failed,
                );
            }
            WinnerJournalAction::Pending => {
                if seen_positions.insert(event.position.position_id.clone()) {
                    total_spent += event.position.filled * event.position.price;
                }
                states.insert(
                    event.position.position_id.clone(),
                    WinnerLifecycleState::Pending,
                );
                pending.insert(event.position.position_id.clone(), event.position);
            }
            WinnerJournalAction::Settled => {
                pending.remove(&event.position.position_id);
                if settled_positions.insert(event.position.position_id.clone()) {
                    let pnl = event.realized_pnl.unwrap_or(dec!(0));
                    realized_pnl += pnl;
                    record_risk_result(&mut risk, &event.position, pnl);
                    if pnl > dec!(0) {
                        awaiting_redeem
                            .insert(event.position.position_id.clone(), event.position.clone());
                    }
                }
                states.insert(
                    event.position.position_id.clone(),
                    WinnerLifecycleState::Settled,
                );
            }
            WinnerJournalAction::RedeemSubmitted => {
                states.insert(
                    event.position.position_id.clone(),
                    WinnerLifecycleState::RedeemSubmitted,
                );
            }
            WinnerJournalAction::Redeemed => {
                awaiting_redeem.remove(&event.position.position_id);
                states.insert(
                    event.position.position_id.clone(),
                    WinnerLifecycleState::Redeemed,
                );
            }
        }
    }
    Ok(WinnerJournalReplay {
        pending,
        awaiting_redeem,
        states,
        total_spent,
        realized_pnl,
        risk,
    })
}

fn record_risk_result(
    risk: &mut LateResolutionWinnerTradingRiskState,
    position: &PendingLateResolutionWinnerTrading,
    pnl: Decimal,
) {
    if pnl < dec!(0) {
        let loss = -pnl;
        risk.strategy_loss += loss;
        *risk.asset_loss.entry(position.asset_key()).or_default() += loss;
        risk.consecutive_losses = risk.consecutive_losses.saturating_add(1);
    } else {
        risk.consecutive_losses = 0;
    }
}

fn quarantine_incomplete_tail(path: &Path, lines: &[String], index: usize) -> Result<()> {
    let quarantine = PathBuf::from(format!(
        "{}.corrupt-{}",
        path.display(),
        Utc::now().timestamp_millis()
    ));
    std::fs::write(&quarantine, lines[index..].join("\n"))?;
    let mut valid = lines[..index].join("\n");
    if !valid.is_empty() {
        valid.push('\n');
    }
    let mut repaired = OpenOptions::new().write(true).truncate(true).open(path)?;
    repaired.write_all(valid.as_bytes())?;
    repaired.sync_all()?;
    Ok(())
}

/// Decide whether the winner side's ask is buyable, from live REST book asks.
///
/// At window end the decided winner's side shows only high asks (0.95-0.99 —
/// makers selling the winner for small certainty gains) while the losing side
/// carries a full grid down to 0.01 (dumped/panic sells). We buy the side whose
/// best ask is high (`>= min_price`) and whose opponent is cheap (`< 0.1` or
/// absent) — direction inferred purely from Polymarket's own book, so it is
/// consistent with settlement.
pub fn detect_winner_asks(
    yes_ask: Option<Decimal>,
    no_ask: Option<Decimal>,
    min_price: Decimal,
    max_price: Decimal,
) -> Option<WinnerSignal> {
    let min = min_price.clamp(dec!(0.5), dec!(0.999));
    let max = max_price.clamp(min, dec!(0.9999));
    const LOSER_MAX: Decimal = dec!(0.1);

    let high_side = |ask: Option<Decimal>| match ask {
        Some(p) if p >= min && p <= max => Some(p),
        _ => None,
    };
    let low_or_absent = |ask: Option<Decimal>| ask.is_none_or(|p| p <= LOSER_MAX);

    if let Some(yes) = high_side(yes_ask) {
        if low_or_absent(no_ask) {
            return Some(WinnerSignal {
                buy_yes: true,
                price: yes,
            });
        }
    }
    if let Some(no) = high_side(no_ask) {
        if low_or_absent(yes_ask) {
            return Some(WinnerSignal {
                buy_yes: false,
                price: no,
            });
        }
    }
    None
}

/// Notional-capped share size for a buy at `price`, floored to 2 decimals.
pub fn capped_size(price: Decimal, max_order_usdc: Decimal) -> Decimal {
    if price <= dec!(0) {
        return dec!(0);
    }
    let raw = (max_order_usdc / price) * dec!(100);
    (raw.floor() / dec!(100)).max(dec!(0))
}

/// Cash-capped share size including the estimated per-share fee.
pub fn fee_inclusive_capped_size(
    price: Decimal,
    fee_per_share: Decimal,
    max_order_usdc: Decimal,
) -> Decimal {
    capped_size((price + fee_per_share).min(dec!(1)), max_order_usdc)
}

/// Cash-capped Late-Resolution Winner Trading size additionally bounded by the currently visible
/// best-ask quantity. The existing minimum-depth model gate remains in force;
/// this prevents a valid but shallow book from turning a $20 request into an
/// unintended partial fill.
pub fn fee_inclusive_depth_capped_size(
    price: Decimal,
    fee_per_share: Decimal,
    max_order_usdc: Decimal,
    ask_size: Decimal,
) -> Decimal {
    let cash_capped = fee_inclusive_capped_size(price, fee_per_share, max_order_usdc);
    let visible = (ask_size.max(dec!(0)) * dec!(100)).floor() / dec!(100);
    cash_capped.min(visible)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;

    #[test]
    fn freshness_uses_post_request_check_time() {
        assert!(received_at_is_fresh(1_100, 1_100, 2_000));
        assert!(received_at_is_fresh(1_100, 1_500, 2_000));
        assert!(!received_at_is_fresh(1_100, 3_101, 2_000));
        assert!(!received_at_is_fresh(1_101, 1_100, 2_000));
    }

    #[test]
    fn stale_complete_books_only_refresh_for_preliminary_candidates() {
        assert!(!winner_books_need_rest(true, true, true, true, true, false));
        assert!(!winner_books_need_rest(
            true, true, true, false, false, false
        ));
        assert!(winner_books_need_rest(true, true, true, false, false, true));
        assert!(winner_books_need_rest(
            true, false, true, false, true, false
        ));
        assert!(winner_books_need_rest(
            true, true, false, true, false, false
        ));
        assert!(winner_books_need_rest(false, true, true, true, true, false));
    }

    #[test]
    fn evaluation_time_buckets_are_stable() {
        assert_eq!(seconds_remaining_bucket(0), "00-20s");
        assert_eq!(seconds_remaining_bucket(20), "00-20s");
        assert_eq!(seconds_remaining_bucket(21), "21-40s");
        assert_eq!(seconds_remaining_bucket(41), "41-60s");
        assert_eq!(seconds_remaining_bucket(61), "61s+");
    }

    #[test]
    fn settlement_backoff_grows_and_clears() {
        let mut backoff = SettlementBackoff::default();
        assert!(backoff.should_query("market", 1_000));
        assert_eq!(backoff.record_retry("market", 1_000), 10);
        assert!(!backoff.should_query("market", 10_999));
        assert!(backoff.should_query("market", 11_000));
        assert_eq!(backoff.record_retry("market", 11_000), 20);
        assert_eq!(backoff.record_retry("market", 31_000), 40);
        assert_eq!(backoff.record_retry("market", 71_000), 60);
        assert_eq!(backoff.record_retry("market", 131_000), 60);
        backoff.clear("market");
        assert!(backoff.should_query("market", 1_001));
    }

    fn pending_buy() -> PendingLateResolutionWinnerTrading {
        PendingLateResolutionWinnerTrading {
            position_id: "winner-1".into(),
            evaluation_id: "evaluation-1".into(),
            candidate_stage: "production".into(),
            asset: "btc".into(),
            slug: "btc-updown-5m-1".into(),
            condition_id: "0x0000000000000000000000000000000000000000000000000000000000000001"
                .into(),
            neg_risk: false,
            token_id: "42".into(),
            token_is_yes: true,
            requested: dec!(20),
            limit_price: dec!(0.97),
            planned_cost: dec!(19.4),
            opening_twap: dec!(100),
            current_twap: dec!(101),
            model_observed_at_ms: 1,
            model_probability: dec!(0.99),
            model_net_ev: dec!(0.01),
            directional_margin_bps: dec!(100),
            reversal_boundary_bps: dec!(10),
            book_spread: dec!(0.01),
            book_depth_usdc: dec!(20),
            filled: dec!(20),
            price: dec!(0.97),
            fee: dec!(0),
            end_date: Utc::now() - TimeDelta::seconds(1),
        }
    }

    #[test]
    fn buys_yes_when_yes_ask_high_and_no_low() {
        let signal =
            detect_winner_asks(Some(dec!(0.98)), Some(dec!(0.01)), dec!(0.95), dec!(0.99)).unwrap();
        assert!(signal.buy_yes);
        assert_eq!(signal.price, dec!(0.98));
    }

    #[test]
    fn buys_no_when_no_ask_high_and_yes_low() {
        let signal =
            detect_winner_asks(Some(dec!(0.03)), Some(dec!(0.985)), dec!(0.95), dec!(0.99))
                .unwrap();
        assert!(!signal.buy_yes);
        assert_eq!(signal.price, dec!(0.985));
    }

    #[test]
    fn buys_yes_when_no_side_has_no_asks() {
        assert!(detect_winner_asks(Some(dec!(0.98)), None, dec!(0.95), dec!(0.99)).is_some());
    }

    #[test]
    fn rejects_when_both_sides_high() {
        // Direction not decided: both sides carry 0.99 guard asks.
        assert!(
            detect_winner_asks(Some(dec!(0.99)), Some(dec!(0.99)), dec!(0.95), dec!(0.99))
                .is_none()
        );
    }

    #[test]
    fn rejects_when_winner_side_above_max_price() {
        assert!(
            detect_winner_asks(Some(dec!(0.993)), Some(dec!(0.01)), dec!(0.95), dec!(0.99))
                .is_none()
        );
        assert!(
            detect_winner_asks(Some(dec!(0.985)), Some(dec!(0.01)), dec!(0.95), dec!(0.99))
                .is_some()
        );
    }

    #[test]
    fn rejects_when_winner_below_min_price() {
        assert!(
            detect_winner_asks(Some(dec!(0.80)), Some(dec!(0.01)), dec!(0.95), dec!(0.99))
                .is_none()
        );
    }

    #[test]
    fn caps_size_by_notional() {
        assert_eq!(capped_size(dec!(0.98), dec!(50)), dec!(51.02));
        assert_eq!(capped_size(dec!(0.99), dec!(100)), dec!(101.01));
        assert_eq!(capped_size(dec!(0.98), dec!(0)), dec!(0));
    }

    #[test]
    fn fee_inclusive_cap_never_exceeds_cash_limit() {
        let price = dec!(0.98);
        let fee = dec!(0.001372);
        let max = dec!(20);
        let size = fee_inclusive_capped_size(price, fee, max);
        assert!((price + fee) * size <= max);
        assert_eq!(size, dec!(20.37));
    }

    #[test]
    fn depth_cap_preserves_cash_cap_and_visible_top_size() {
        assert_eq!(
            fee_inclusive_depth_capped_size(dec!(0.97), dec!(0.002), dec!(20), dec!(12.345)),
            dec!(12.34)
        );
        assert_eq!(
            fee_inclusive_depth_capped_size(dec!(0.97), dec!(0.002), dec!(20), dec!(50)),
            dec!(20.57)
        );
        assert_eq!(
            fee_inclusive_depth_capped_size(dec!(0.97), dec!(0.002), dec!(20), dec!(-1)),
            dec!(0)
        );
    }

    #[test]
    fn settle_credits_win_and_debits_loss() {
        let buy = pending_buy();
        let now = Utc::now();
        // YES won: +20 * (1 - 0.97)
        assert_eq!(
            settle_late_resolution_winner_trading(&buy, now, true, &(dec!(1.0), dec!(0.0))),
            Some(dec!(0.6))
        );
        // NO won: -20 * 0.97
        assert_eq!(
            settle_late_resolution_winner_trading(&buy, now, true, &(dec!(0.0), dec!(1.0))),
            Some(-dec!(19.4))
        );
        assert_eq!(
            settle_late_resolution_winner_trading(&buy, now, false, &(dec!(1), dec!(0))),
            None
        );
        assert_eq!(
            settle_late_resolution_winner_trading(&buy, now, true, &(dec!(0.98), dec!(0.02))),
            None
        );
        assert_eq!(
            settle_late_resolution_winner_trading(&buy, now, true, &(dec!(0.5), dec!(0.5))),
            None
        );

        let future_buy = PendingLateResolutionWinnerTrading {
            end_date: now + TimeDelta::seconds(1),
            ..buy
        };
        assert_eq!(
            settle_late_resolution_winner_trading(&future_buy, now, true, &(dec!(1), dec!(0))),
            None
        );
    }

    #[test]
    fn journal_restores_pending_and_settlement_is_idempotent() {
        let path = std::env::temp_dir().join(format!(
            "polypulse-late-resolution-winner-trading-{}.ndjson",
            uuid::Uuid::new_v4()
        ));
        let buy = pending_buy();
        {
            let journal = LateResolutionWinnerTradingJournal::open(&path).unwrap();
            assert!(journal.record_pending(buy.clone()).unwrap());
            assert!(!journal.record_pending(buy.clone()).unwrap());
            assert_eq!(journal.pending(), vec![buy.clone()]);
        }
        {
            let journal = LateResolutionWinnerTradingJournal::open(&path).unwrap();
            assert_eq!(journal.pending(), vec![buy.clone()]);
            assert_eq!(
                journal.paper_summary(),
                LateResolutionWinnerTradingPaperSummary {
                    pending_cost: dec!(19.4),
                    total_spent: dec!(19.4),
                    realized_pnl: dec!(0),
                    open_positions: 1,
                }
            );
            assert!(journal.record_settled(&buy, dec!(0.6)).unwrap());
            assert!(!journal.record_settled(&buy, dec!(0.6)).unwrap());
            assert_eq!(journal.awaiting_redeem(), vec![buy.clone()]);
            assert!(journal.record_redeem_submitted(&buy).unwrap());
            assert!(journal.record_redeemed(&buy, "paper:test".into()).unwrap());
        }
        let reloaded = LateResolutionWinnerTradingJournal::open(&path).unwrap();
        assert!(reloaded.pending().is_empty());
        assert!(reloaded.awaiting_redeem().is_empty());
        assert_eq!(reloaded.paper_summary().realized_pnl, dec!(0.6));
        assert_eq!(reloaded.paper_summary().total_spent, dec!(19.4));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn evaluation_journal_resolves_rejected_candidates_without_survivorship_bias() {
        let path = std::env::temp_dir().join(format!(
            "polypulse-winner-evaluations-{}.ndjson",
            uuid::Uuid::new_v4()
        ));
        let now = Utc::now();
        let evaluation = WinnerEvaluation {
            evaluation_id: "eval-1".into(),
            asset: "btc".into(),
            slug: "btc-updown-5m-1".into(),
            end_date: now - TimeDelta::seconds(1),
            evaluated_at: now - TimeDelta::seconds(2),
            window_start_ms: 1,
            seconds_remaining: 1,
            buy_yes: true,
            bid_price: dec!(0.94),
            ask_price: dec!(0.95),
            ask_size: dec!(10),
            fee_per_share: dec!(0.003325),
            rejection: Some("NetEvTooLow".into()),
            failed_checks: vec!["NetEvTooLow".into()],
            opening_twap: Some(dec!(100)),
            current_twap: Some(dec!(101)),
            model_observed_at_ms: Some(2),
            model_probability: dec!(0.94),
            model_net_ev: dec!(-0.01),
            effective_entry_price: dec!(0.953325),
            probability_edge_before_risk: dec!(-0.013325),
            required_probability: dec!(0.965325),
            reversal_margin_ratio: 10.0,
            ask_price_bucket: "0.95".into(),
            seconds_remaining_bucket: "00-20s".into(),
            book_source: "rest".into(),
            book_age_ms: 0,
            research_cohorts: vec![],
            directional_margin_bps: 100.0,
            reversal_boundary_bps: 10.0,
            volatility_bps_per_sqrt_second: 1.0,
            spread: dec!(0.01),
            depth_usdc: dec!(9.5),
        };
        let cohorts = winner_research_cohorts(&evaluation, dec!(0.03), dec!(5));
        assert!(cohorts.iter().any(|cohort| cohort == "all_candidates"));
        assert!(cohorts
            .iter()
            .any(|cohort| cohort == "chainlink_direction_agrees"));
        assert!(!cohorts.iter().any(|cohort| cohort == "price_096_098"));
        assert!(!cohorts.iter().any(|cohort| cohort == "production_model"));
        let mut production_evaluation = evaluation.clone();
        production_evaluation.ask_price = dec!(0.97);
        production_evaluation.rejection = None;
        production_evaluation.failed_checks.clear();
        let production_cohorts =
            winner_research_cohorts(&production_evaluation, dec!(0.03), dec!(5));
        assert!(production_cohorts
            .iter()
            .any(|cohort| cohort == "hybrid_chainlink_veto"));
        assert!(production_cohorts
            .iter()
            .any(|cohort| cohort == "production_model"));

        let mut upper_bound = production_evaluation.clone();
        upper_bound.ask_price = dec!(0.98);
        upper_bound.effective_entry_price = dec!(0.985);
        assert!(winner_research_cohorts(&upper_bound, dec!(0.03), dec!(5))
            .iter()
            .any(|cohort| cohort == "hybrid_chainlink_veto"));
        upper_bound.effective_entry_price = dec!(1);
        assert!(!winner_research_cohorts(&upper_bound, dec!(0.03), dec!(5))
            .iter()
            .any(|cohort| cohort == "hybrid_chainlink_veto"));
        {
            let journal = WinnerEvaluationJournal::open(&path).unwrap();
            journal.record(evaluation).unwrap();
            journal
                .record_confirmation_observation(WinnerConfirmationObservation {
                    evaluation_id: "eval-1".into(),
                    slug: "btc-updown-5m-1".into(),
                    recorded_at: now,
                    model_observed_at_ms: 2,
                    confirmations: 1,
                    required: 2,
                    confirmed: false,
                    candidate_stage: "rejected_shadow".into(),
                })
                .unwrap();
            journal
                .record_shadow_execution(WinnerShadowExecution {
                    evaluation_id: "eval-1".into(),
                    slug: "btc-updown-5m-1".into(),
                    simulated_at: now,
                    delay_ms: 250,
                    candidate_stage: "rejected".into(),
                    research_cohorts: vec!["all_candidates".into()],
                    outcome: "filled".into(),
                    limit_price: dec!(0.95),
                    requested: dec!(20),
                    filled: dec!(7.5),
                    cost: dec!(7.1499375),
                    average_price: dec!(0.953325),
                    estimated_fee: dec!(0.0249375),
                    error: None,
                })
                .unwrap();
            assert_eq!(journal.due_slugs(now), vec!["btc-updown-5m-1"]);
        }
        let journal = WinnerEvaluationJournal::open(&path).unwrap();
        assert_eq!(
            journal
                .record_resolution("btc-updown-5m-1", now, true, &(dec!(1), dec!(0)),)
                .unwrap(),
            1
        );
        assert!(journal.due_slugs(now).is_empty());
        drop(journal);
        assert!(WinnerEvaluationJournal::open(&path)
            .unwrap()
            .due_slugs(now)
            .is_empty());
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("\"action\":\"shadow_executed\""));
        assert!(contents.contains("\"action\":\"confirmation_observed\""));
        assert!(contents.contains("\"candidate_stage\":\"rejected\""));
        assert!(contents.contains("\"delay_ms\":250"));
        assert!(contents.contains("\"outcome\":\"filled\""));
        assert!(contents.contains("\"action\":\"market_resolved\""));
        assert!(contents.contains("\"evaluation_count\":1"));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn journal_audits_no_fill_and_failed_attempts() {
        let path = std::env::temp_dir().join(format!(
            "polypulse-winner-attempt-{}.ndjson",
            uuid::Uuid::new_v4()
        ));
        let first = pending_buy();
        let mut second = pending_buy();
        second.position_id = "winner-2".into();
        {
            let journal = LateResolutionWinnerTradingJournal::open(&path).unwrap();
            assert!(journal.record_attempt(first.clone()).unwrap());
            assert!(journal.record_no_fill(&first).unwrap());
            assert!(journal.record_attempt(second.clone()).unwrap());
            assert!(journal.record_failed(&second, "transport").unwrap());
        }
        let replayed = LateResolutionWinnerTradingJournal::open(&path).unwrap();
        assert!(replayed.pending().is_empty());
        assert!(!replayed.record_no_fill(&first).unwrap());
        assert!(!replayed.record_failed(&second, "again").unwrap());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn persisted_strategy_asset_and_consecutive_loss_breakers_reject_entries() {
        let path = std::env::temp_dir().join(format!(
            "polypulse-winner-risk-{}.ndjson",
            uuid::Uuid::new_v4()
        ));
        let btc = pending_buy();
        let mut eth = pending_buy();
        eth.position_id = "winner-eth".into();
        eth.asset = "eth".into();
        eth.slug = "eth-updown-5m-1".into();
        {
            let journal = LateResolutionWinnerTradingJournal::open(&path).unwrap();
            journal.record_pending(btc.clone()).unwrap();
            journal.record_settled(&btc, dec!(-6)).unwrap();
            journal.record_pending(eth.clone()).unwrap();
            journal.record_settled(&eth, dec!(-5)).unwrap();
        }
        let journal = LateResolutionWinnerTradingJournal::open(&path).unwrap();
        let snapshot = journal.risk_snapshot();
        assert_eq!(snapshot.strategy_loss, dec!(11));
        assert_eq!(snapshot.asset_loss["btc"], dec!(6));
        assert_eq!(snapshot.asset_loss["eth"], dec!(5));
        assert_eq!(snapshot.consecutive_losses, 2);
        assert!(journal
            .check_risk(
                "btc",
                LateResolutionWinnerTradingRiskPolicy {
                    max_strategy_loss: dec!(20),
                    max_asset_loss: dec!(6),
                    max_consecutive_losses: 3,
                },
            )
            .is_err());
        assert!(journal
            .check_risk(
                "sol",
                LateResolutionWinnerTradingRiskPolicy {
                    max_strategy_loss: dec!(11),
                    max_asset_loss: dec!(10),
                    max_consecutive_losses: 3,
                },
            )
            .is_err());
        assert!(journal
            .check_risk(
                "sol",
                LateResolutionWinnerTradingRiskPolicy {
                    max_strategy_loss: dec!(20),
                    max_asset_loss: dec!(10),
                    max_consecutive_losses: 2,
                },
            )
            .is_err());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn submitted_but_unconfirmed_redeem_is_restored_as_uncertain() {
        let path = std::env::temp_dir().join(format!(
            "polypulse-winner-redeem-{}.ndjson",
            uuid::Uuid::new_v4()
        ));
        let buy = pending_buy();
        {
            let journal = LateResolutionWinnerTradingJournal::open(&path).unwrap();
            journal.record_pending(buy.clone()).unwrap();
            journal.record_settled(&buy, dec!(0.6)).unwrap();
            assert!(journal.record_redeem_submitted(&buy).unwrap());
        }
        let journal = LateResolutionWinnerTradingJournal::open(&path).unwrap();
        assert_eq!(journal.uncertain_redeem_count(), 1);
        assert_eq!(journal.awaiting_redeem(), vec![buy]);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn journal_quarantines_an_incomplete_tail_before_future_appends() {
        let path = std::env::temp_dir().join(format!(
            "polypulse-late-resolution-winner-trading-tail-{}.ndjson",
            uuid::Uuid::new_v4()
        ));
        let buy = pending_buy();
        LateResolutionWinnerTradingJournal::open(&path)
            .unwrap()
            .record_pending(buy.clone())
            .unwrap();
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{incomplete")
            .unwrap();

        let journal = LateResolutionWinnerTradingJournal::open(&path).unwrap();
        assert_eq!(journal.pending(), vec![buy.clone()]);
        journal.record_settled(&buy, dec!(0.6)).unwrap();
        assert!(LateResolutionWinnerTradingJournal::open(&path)
            .unwrap()
            .pending()
            .is_empty());
        std::fs::remove_file(path).unwrap();
    }
}
