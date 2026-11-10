//! Independent, funded PAPER comparison. Never uses the real order API or the
//! production ledger. The only arm-specific admission condition is ETH ask<=.97.
use crate::config::Config;
use crate::late_resolution_winner_model::WinnerConfirmationTracker;
use crate::late_resolution_winner_trading::{
    fee_inclusive_depth_capped_size, DurableNdjsonWriter, WinnerEvaluation, WinnerShadowExecution,
};
use crate::trading::TradingExecutor;
use crate::winner_dashboard::control::{self, Action, Command, Reply, Settings, Status};
use anyhow::{bail, ensure, Context, Result};
use chrono::{DateTime, Utc};
use polymarket_client_sdk::types::U256;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::sync::{Arc, Mutex};

const STUDY_ID: &str = "eth-ask-cap-097-v1";
pub(crate) const LOSS_BOOST_STUDY: &str = "winner-loss-boost-1h-v1";
const CONTROL: &str = "control_096_098";
const CANDIDATE: &str = "eth_cap_097";
const ARMS: [&str; 2] = [CONTROL, CANDIDATE];

#[derive(Clone, Debug)]
pub(crate) struct LossBoost {
    pub(crate) base: Decimal,
    pub(crate) capital: Decimal,
    last_loss: [Option<DateTime<Utc>>; 2],
}

impl LossBoost {
    fn asset(asset: &str) -> Result<usize> {
        match asset {
            "btc" => Ok(0),
            "eth" => Ok(1),
            _ => bail!("unsupported loss-boost asset"),
        }
    }

    pub(crate) fn from_configuration(c: &Value) -> Result<Self> {
        let amount = |key: &str| -> Result<Decimal> {
            Ok(c[key]
                .as_str()
                .context("missing loss-boost amount")?
                .parse()?)
        };
        let base = amount("base_order_usdc")?;
        let capital = amount("initial_cash_per_arm")?;
        control::valid_budget(base)?;
        control::valid_deposit(capital)?;
        ensure!(
            capital >= base * dec!(10)
                && amount("max_order_usdc")? == base * dec!(10)
                && c["study_id"] == LOSS_BOOST_STUDY
                && c["paper_only"] == true
                && c["automatic_promotion"] == false
                && c["boost_multiplier"] == 10
                && c["boost_duration_secs"] == 3600
                && c["arms"] == json!({(CANDIDATE): {"btc_max_ask":"0.98","eth_max_ask":"0.97"}}),
            "invalid loss-boost configuration"
        );
        Ok(Self {
            base,
            capital,
            last_loss: [None; 2],
        })
    }

    pub(crate) fn record_loss(&mut self, asset: usize, at: DateTime<Utc>) -> Result<()> {
        ensure!(
            at.checked_add_signed(chrono::TimeDelta::hours(1)).is_some(),
            "loss-boost expiry out of range"
        );
        self.last_loss[asset] = self.last_loss[asset].max(Some(at));
        Ok(())
    }

    pub(crate) fn active_until(&self, asset: usize, at: DateTime<Utc>) -> Option<DateTime<Utc>> {
        self.last_loss[asset]
            .filter(|start| *start <= at)
            .and_then(|start| start.checked_add_signed(chrono::TimeDelta::hours(1)))
            .filter(|end| at < *end)
    }

    pub(crate) fn budget(&self, asset: usize, at: DateTime<Utc>) -> Decimal {
        if self.active_until(asset, at).is_some() {
            self.base * dec!(10)
        } else {
            self.base
        }
    }
}

/// Offline capability check used by the deployment script (no credentials/API).
pub fn specification() -> Value {
    json!({"study_id": STUDY_ID, "schema_version": 1, "paper_only": true,
        "control_max_ask": "0.98", "candidate_eth_max_ask": "0.97",
        "candidate_btc_max_ask": "0.98", "max_order_usdc": "5"})
}

pub fn loss_boost_specification() -> Value {
    json!({"study_id": LOSS_BOOST_STUDY, "schema_version": 3, "paper_only": true,
        "multiplier": 10, "duration_secs": 3600, "scope": "per_asset",
        "trigger": "durably_confirmed_negative_pnl",
        "retrigger": "restart_timer_without_stacking", "default_base_usdc": "5",
        "default_capital_usdc": "100"})
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Attempt {
    attempt_id: String,
    arm: String,
    evaluation: WinnerEvaluation,
    requested: Decimal,
    reserved_cost: Decimal,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parameter_revision: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    budget_limit: Option<Decimal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    entry_epoch: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Decision {
    arm: String,
    reason: String,
    confirmations: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum Event {
    OperatorFunded {
        command: Command,
        amount: Decimal,
        revision: u64,
        funded_at: DateTime<Utc>,
    },
    OperatorChanged {
        command: Option<Command>,
        settings: Settings,
        changed_at: DateTime<Utc>,
    },
    Registered {
        version: u32,
        study_id: String,
        registered_at: DateTime<Utc>,
        forward_window_start_ms: i64,
        configuration: Value,
    },
    RunStarted {
        run_id: String,
        started_at: DateTime<Utc>,
    },
    Observed {
        evaluation: Box<WinnerEvaluation>,
        decisions: Vec<Decision>,
    },
    Attempted {
        attempt: Box<Attempt>,
    },
    Executed {
        attempt_id: String,
        execution: Box<WinnerShadowExecution>,
    },
    Settled {
        attempt_id: String,
        won: bool,
        payout: Decimal,
        pnl: Decimal,
        resolved_at: DateTime<Utc>,
    },
}

struct Position {
    attempt: Attempt,
    execution: Option<WinnerShadowExecution>,
    settled: bool,
}

struct State {
    confirmations: [WinnerConfirmationTracker; 2],
    attempted: HashSet<(String, String)>,
    positions: HashMap<String, Position>,
    cash: [Decimal; 2],
    failed: bool,
    operator: Option<Settings>,
    commands: HashMap<String, Command>,
    deposits: Decimal,
    boost: Option<LossBoost>,
}

impl State {
    fn new() -> Self {
        Self {
            confirmations: Default::default(),
            attempted: HashSet::new(),
            positions: HashMap::new(),
            cash: [dec!(100); 2],
            failed: false,
            operator: None,
            commands: HashMap::new(),
            deposits: dec!(0),
            boost: None,
        }
    }

    fn apply(&mut self, event: &Event) -> Result<()> {
        match event {
            Event::OperatorFunded {
                command,
                amount,
                revision,
                ..
            } => {
                let settings = self
                    .operator
                    .as_ref()
                    .context("funding in frozen journal")?;
                control::validate_funding(settings, command, *amount, *revision)?;
                ensure!(
                    !self.commands.contains_key(&command.id),
                    "duplicate PAPER funding command"
                );
                let cash = self.cash[1]
                    .checked_add(*amount)
                    .context("PAPER cash overflow")?;
                let deposits = self
                    .deposits
                    .checked_add(*amount)
                    .context("PAPER deposits overflow")?;
                control::initial_capital()
                    .checked_add(deposits)
                    .context("PAPER capital overflow")?;
                self.cash[1] = cash;
                self.deposits = deposits;
                self.operator.as_mut().expect("checked operator").revision = *revision;
                self.commands.insert(command.id.clone(), command.clone());
            }
            Event::OperatorChanged {
                command,
                settings,
                changed_at,
            } => {
                let old = self
                    .operator
                    .as_ref()
                    .context("operator event in frozen journal")?;
                ensure!(
                    settings.revision == old.revision + 1,
                    "operator revision discontinuity"
                );
                control::valid_budget(settings.budget)?;
                if let Some(value) = settings.pending_budget {
                    control::valid_budget(value)?;
                }
                ensure!(
                    settings.pending_budget.is_some() == settings.effective_window_ms.is_some(),
                    "incomplete pending budget"
                );
                let mut expected = old.clone();
                expected.revision += 1;
                if let Some(c) = command {
                    ensure!(
                        c.expected_revision == old.revision && uuid::Uuid::parse_str(&c.id).is_ok(),
                        "invalid persisted operator command"
                    );
                    match c.action {
                        Action::Pause => {
                            expected.paused = true;
                            expected.entry_epoch += 1;
                        }
                        Action::Resume => {
                            expected.paused = false;
                            expected.entry_epoch += 1;
                        }
                        Action::Deposit { .. } => {
                            bail!("funding requires an operator_funded event")
                        }
                        Action::SetBudget { amount } => {
                            expected.pending_budget = Some(amount);
                            expected.effective_window_ms = Some(
                                (changed_at.timestamp_millis().div_euclid(300_000) + 1) * 300_000,
                            );
                        }
                    }
                    ensure!(
                        !self.commands.contains_key(&c.id),
                        "duplicate operator command"
                    );
                    self.commands.insert(c.id.clone(), c.clone());
                } else {
                    ensure!(
                        old.effective_window_ms
                            .is_some_and(|t| t <= changed_at.timestamp_millis()),
                        "premature budget activation"
                    );
                    expected.budget = expected
                        .pending_budget
                        .take()
                        .context("activation without pending budget")?;
                    expected.effective_window_ms = None;
                }
                ensure!(
                    expected == *settings,
                    "operator state does not match command"
                );
                if old.entry_epoch != settings.entry_epoch || old.budget != settings.budget {
                    for tracker in &mut self.confirmations {
                        tracker.clear();
                    }
                }
                self.operator = Some(settings.clone());
            }
            Event::Attempted { attempt } => {
                let index = arm_index(&attempt.arm)?;
                let cap = if let Some(boost) = &self.boost {
                    let e = &attempt.evaluation;
                    let cap = boost.budget(LossBoost::asset(&e.asset)?, e.evaluated_at);
                    ensure!(
                        index == 1
                            && attempt.budget_limit == Some(cap)
                            && attempt.parameter_revision.is_none()
                            && attempt.entry_epoch.is_none(),
                        "loss-boost attempt budget does not match prior settlements"
                    );
                    cap
                } else if self.operator.is_some() {
                    dec!(100)
                } else {
                    dec!(5)
                };
                ensure!(
                    !self.positions.contains_key(&attempt.attempt_id),
                    "duplicate study attempt ID"
                );
                ensure!(
                    self.attempted
                        .insert((attempt.arm.clone(), attempt.evaluation.slug.clone())),
                    "duplicate study arm/market attempt"
                );
                ensure!(
                    attempt.requested > dec!(0)
                        && attempt.reserved_cost > dec!(0)
                        && attempt.reserved_cost <= cap
                        && self.cash[index] >= attempt.reserved_cost,
                    "invalid study reservation"
                );
                if let Some(c) = &self.operator {
                    ensure!(
                        !c.paused
                            && attempt.arm == CANDIDATE
                            && attempt.parameter_revision == Some(c.revision)
                            && attempt.entry_epoch == Some(c.entry_epoch)
                            && attempt.budget_limit == Some(c.budget)
                            && attempt.reserved_cost <= c.budget,
                        "operator attempt metadata mismatch"
                    );
                }
                self.cash[index] -= attempt.reserved_cost;
                self.positions.insert(
                    attempt.attempt_id.clone(),
                    Position {
                        attempt: (**attempt).clone(),
                        execution: None,
                        settled: false,
                    },
                );
            }
            Event::Executed {
                attempt_id,
                execution,
            } => {
                let position = self
                    .positions
                    .get_mut(attempt_id)
                    .context("execution missing study attempt")?;
                ensure!(position.execution.is_none(), "duplicate study execution");
                validate_execution(&position.attempt, execution)?;
                if execution.filled > dec!(0) {
                    if let Some(c) = &self.operator {
                        ensure!(
                            !c.paused && position.attempt.entry_epoch == Some(c.entry_epoch),
                            "operator fill after pause"
                        );
                    }
                }
                let index = arm_index(&position.attempt.arm)?;
                self.cash[index] += position.attempt.reserved_cost - execution.cost;
                position.execution = Some((**execution).clone());
            }
            Event::Settled {
                attempt_id,
                won,
                payout,
                pnl,
                resolved_at,
            } => {
                let position = self
                    .positions
                    .get_mut(attempt_id)
                    .context("settlement missing study attempt")?;
                ensure!(!position.settled, "duplicate study settlement");
                let execution = position
                    .execution
                    .as_ref()
                    .context("settlement missing study execution")?;
                ensure!(
                    *resolved_at >= position.attempt.evaluation.end_date,
                    "premature study settlement"
                );
                ensure!(
                    *payout == if *won { execution.filled } else { dec!(0) }
                        && *pnl == *payout - execution.cost,
                    "invalid study payout"
                );
                self.cash[arm_index(&position.attempt.arm)?] += *payout;
                position.settled = true;
                if *pnl < dec!(0) {
                    if let Some(boost) = &mut self.boost {
                        boost.record_loss(
                            LossBoost::asset(&position.attempt.evaluation.asset)?,
                            *resolved_at,
                        )?;
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
}

fn arm_index(arm: &str) -> Result<usize> {
    ARMS.iter()
        .position(|name| *name == arm)
        .context("unknown price-cap study arm")
}

fn validate_execution(attempt: &Attempt, execution: &WinnerShadowExecution) -> Result<()> {
    ensure!(
        execution.evaluation_id == attempt.evaluation.evaluation_id
            && execution.slug == attempt.evaluation.slug
            && execution.run_id == attempt.evaluation.run_id
            && execution.requested == attempt.requested
            && execution.limit_price == attempt.evaluation.ask_price
            && execution.simulated_at >= attempt.evaluation.evaluated_at,
        "study execution/attempt mismatch"
    );
    ensure!(
        execution.filled >= dec!(0)
            && execution.filled <= attempt.requested
            && execution.cost >= dec!(0)
            && execution.cost <= attempt.reserved_cost,
        "study execution exceeds reservation"
    );
    match execution.outcome.as_str() {
        "filled" => ensure!(
            execution.filled > dec!(0)
                && execution.cost > dec!(0)
                && execution.simulated_at < attempt.evaluation.end_date,
            "invalid/expired filled study execution"
        ),
        "no_fill" | "error" => ensure!(
            execution.filled == dec!(0) && execution.cost == dec!(0),
            "unfilled study execution has exposure"
        ),
        _ => bail!("unknown study execution outcome"),
    }
    Ok(())
}

pub(crate) struct PriceCapStudy {
    writer: DurableNdjsonWriter,
    state: Mutex<State>,
    forward_window_start_ms: i64,
    run_id: String,
    delay_ms: u64,
    max_feed_age_ms: i64,
    max_book_age_ms: i64,
    max_spread: Decimal,
    min_depth: Decimal,
    configuration: Value,
    path: std::path::PathBuf,
    operator: bool,
    loss_boost: bool,
}

impl PriceCapStudy {
    pub(crate) fn from_env(config: &Config, run_id: &str) -> Result<Option<Arc<Self>>> {
        let enabled = std::env::var("LATE_RESOLUTION_WINNER_PRICE_CAP_STUDY_ENABLED")
            .unwrap_or_else(|_| "false".into())
            .parse::<bool>()
            .context("invalid LATE_RESOLUTION_WINNER_PRICE_CAP_STUDY_ENABLED")?;
        if !enabled {
            return Ok(None);
        }
        ensure!(
            config.dry_run && config.late_resolution_winner_trading_enabled,
            "price-cap study requires enabled Late Resolution Winner dry-run"
        );
        ensure!(
            config.market_period_minutes == 5
                && config.late_resolution_winner_trading_before_end_secs == 60
                && config.late_resolution_winner_trading_min_price == 0.96
                && config.late_resolution_winner_trading_max_price == 0.98
                && config.late_resolution_winner_trading_max_order_usdc == 5.0
                && config.late_resolution_winner_trading_confirmation_observations == 2,
            "price-cap v1 requires 5min, last60s, .96-.98, $5 and two TWAP confirmations"
        );
        let path = std::env::var("LATE_RESOLUTION_WINNER_PRICE_CAP_STUDY_FILE")
            .unwrap_or_else(|_| "results/late-resolution-winner-trading/server-low-latency-007-5m/price-cap-study.ndjson".into());
        ensure!(
            path != config.late_resolution_winner_trading_evaluation_journal_file
                && path != config.late_resolution_winner_trading_state_journal_file,
            "price-cap study must use its own journal"
        );
        let mut configuration = json!({
            "study_id": STUDY_ID, "paper_only": true, "automatic_promotion": false,
            "arms": { (CONTROL): {"btc_max_ask": "0.98", "eth_max_ask": "0.98"},
                (CANDIDATE): {"btc_max_ask": "0.98", "eth_max_ask": "0.97"} },
            "initial_cash_per_arm": "100", "max_order_usdc": "5", "max_attempts_per_market_per_arm": 1,
            "before_end_secs": 60, "min_ask": "0.96", "confirmation_observations": 2,
            "reference_gate": "valid_data_and_market_quality_ignore_boundary_and_ev_only",
            "delay_ms": config.paper_late_resolution_winner_trading_delay_ms,
            "liquidity_haircut": config.paper_liquidity_haircut,
            "order_type": format!("{:?}", config.arbitrage_order_type),
            "max_book_age_ms": config.late_resolution_winner_trading_max_book_age_ms,
            "max_rest_latency_ms": config.late_resolution_winner_trading_max_rest_latency_ms,
            "max_feed_age_ms": config.late_resolution_winner_trading_feed_max_age_ms,
            "opening_max_skew_ms": config.late_resolution_winner_trading_opening_max_skew_ms,
            "volatility_lookback_secs": config.late_resolution_winner_trading_volatility_lookback_secs,
            "min_observations": config.late_resolution_winner_trading_min_observations,
            "max_spread": config.late_resolution_winner_trading_max_spread,
            "min_depth_usdc": config.late_resolution_winner_trading_min_depth_usdc,
            "min_margin_bps": config.late_resolution_winner_trading_min_margin_bps,
            "reversal_sigma": config.late_resolution_winner_trading_reversal_sigma,
            "min_net_ev": config.late_resolution_winner_trading_min_net_ev,
            "btc_floor": config.late_resolution_winner_trading_btc_min_volatility_bps,
            "eth_floor": config.late_resolution_winner_trading_eth_min_volatility_bps,
            "btc_risk_buffer": config.late_resolution_winner_trading_btc_model_risk_buffer,
            "eth_risk_buffer": config.late_resolution_winner_trading_eth_model_risk_buffer,
            "reference_url": config.late_resolution_winner_trading_rtds_url,
            "crypto_symbols": config.crypto_symbols,
        });
        if std::env::var("LATE_RESOLUTION_WINNER_LOSS_BOOST_ENABLED")
            .unwrap_or_else(|_| "false".into())
            .parse::<bool>()
            .context("invalid loss-boost enable flag")?
        {
            ensure!(
                Path::new(&path)
                    .file_name()
                    .is_some_and(|n| n == "loss-boost-study.ndjson"),
                "loss-boost requires an isolated loss-boost-study.ndjson journal"
            );
            let base = std::env::var("LATE_RESOLUTION_WINNER_LOSS_BOOST_BASE_USDC")
                .unwrap_or_else(|_| "5".into())
                .parse::<Decimal>()
                .context("invalid loss-boost base")?;
            control::valid_budget(base)?;
            let capital = std::env::var("LATE_RESOLUTION_WINNER_LOSS_BOOST_CAPITAL_USDC")
                .unwrap_or_else(|_| "100".into())
                .parse::<Decimal>()
                .context("invalid loss-boost capital")?;
            configuration = Self::boost_configuration(configuration, base, capital);
        }
        Self::open(
            Path::new(&path),
            config.late_resolution_winner_trading_journal_min_free_bytes,
            configuration,
            run_id,
            Utc::now(),
        )
        .map(|study| Some(Arc::new(study)))
    }

    fn boost_configuration(mut c: Value, base: Decimal, capital: Decimal) -> Value {
        c["study_id"] = json!(LOSS_BOOST_STUDY);
        c["base_order_usdc"] = json!(base.normalize().to_string());
        c["initial_cash_per_arm"] = json!(capital.normalize().to_string());
        c["max_order_usdc"] = json!((base * dec!(10)).normalize().to_string());
        c["boost_multiplier"] = json!(10);
        c["boost_duration_secs"] = json!(3600);
        c["arms"] = json!({(CANDIDATE): {"btc_max_ask":"0.98","eth_max_ask":"0.97"}});
        c
    }

    pub(crate) fn is_loss_boost(&self) -> bool {
        self.loss_boost
    }

    pub(crate) fn operator_from(
        &self,
        path: &Path,
        reserve: u64,
        run_id: &str,
    ) -> Result<Arc<Self>> {
        let mut c = self.configuration.clone();
        c["study_id"] = json!(control::STUDY);
        c["max_order_usdc"] = json!("100");
        c["initial_budget"] = json!("5");
        c["initial_paused"] = json!(true);
        c["arms"]
            .as_object_mut()
            .context("study arms")?
            .remove(CONTROL);
        Ok(Arc::new(Self::open(path, reserve, c, run_id, Utc::now())?))
    }

    pub(crate) fn operator_status(&self) -> Option<Status> {
        let s = self.state.lock().expect("study lock");
        s.operator.clone().map(|settings| Status {
            journal: crate::winner_dashboard::live::canonical(&self.path)
                .to_string_lossy()
                .into_owned(),
            settings,
            failed: s.failed,
            inflight: s
                .positions
                .values()
                .filter(|p| p.execution.is_none())
                .count(),
            pending_fills: s
                .positions
                .values()
                .filter(|p| !p.settled && p.execution.as_ref().is_some_and(|x| x.filled > dec!(0)))
                .count(),
            cash: s.cash[1],
            capital: control::initial_capital() + s.deposits,
            deposits: s.deposits,
            funding_supported: true,
        })
    }

    fn activate_locked(&self, s: &mut State, now: DateTime<Utc>) -> Result<()> {
        if let Some(old) = &s.operator {
            if old
                .effective_window_ms
                .is_some_and(|t| t <= now.timestamp_millis())
            {
                let mut settings = old.clone();
                settings.budget = settings
                    .pending_budget
                    .take()
                    .context("pending budget missing")?;
                settings.effective_window_ms = None;
                settings.revision += 1;
                self.append(
                    s,
                    &Event::OperatorChanged {
                        command: None,
                        settings,
                        changed_at: now,
                    },
                )?;
            }
        }
        Ok(())
    }
    pub(crate) fn activate_operator(&self, now: DateTime<Utc>) -> Result<()> {
        let mut s = self.state.lock().expect("study lock");
        ensure!(!s.failed, "operator journal failed");
        self.activate_locked(&mut s, now)
    }
    pub(crate) fn command(&self, c: Command) -> Reply {
        let result = (|| -> Result<()> {
            ensure!(
                self.operator && c.run_id == self.run_id,
                "wrong engine or non-operator target"
            );
            ensure!(
                c.journal == crate::winner_dashboard::live::canonical(&self.path).to_string_lossy(),
                "wrong operator journal"
            );
            ensure!(uuid::Uuid::parse_str(&c.id).is_ok(), "invalid command ID");
            let mut s = self.state.lock().expect("study lock");
            ensure!(!s.failed, "operator journal failed");
            if let Some(previous) = s.commands.get(&c.id) {
                ensure!(
                    serde_json::to_value(previous)? == serde_json::to_value(&c)?,
                    "command ID reused with different payload"
                );
                return Ok(());
            }
            let now = Utc::now();
            self.activate_locked(&mut s, now)?;
            let mut settings = s.operator.clone().context("operator unavailable")?;
            ensure!(
                c.expected_revision == settings.revision,
                "stale revision; refresh and confirm again"
            );
            if let Action::Deposit { amount } = c.action {
                control::valid_deposit(amount)?;
                let revision = settings
                    .revision
                    .checked_add(1)
                    .context("operator revision overflow")?;
                s.cash[1]
                    .checked_add(amount)
                    .context("PAPER cash overflow")?;
                let deposits = s
                    .deposits
                    .checked_add(amount)
                    .context("PAPER deposits overflow")?;
                control::initial_capital()
                    .checked_add(deposits)
                    .context("PAPER capital overflow")?;
                return self.append(
                    &mut s,
                    &Event::OperatorFunded {
                        command: c.clone(),
                        amount,
                        revision,
                        funded_at: now,
                    },
                );
            }
            match c.action {
                Action::Deposit { .. } => unreachable!("deposit handled separately"),
                Action::Pause => {
                    settings.paused = true;
                    settings.entry_epoch += 1;
                }
                Action::Resume => {
                    settings.paused = false;
                    settings.entry_epoch += 1;
                }
                Action::SetBudget { amount } => {
                    control::valid_budget(amount)?;
                    settings.pending_budget = Some(amount);
                    settings.effective_window_ms =
                        Some((now.timestamp_millis().div_euclid(300_000) + 1) * 300_000);
                }
            }
            settings.revision += 1;
            self.append(
                &mut s,
                &Event::OperatorChanged {
                    command: Some(c.clone()),
                    settings,
                    changed_at: now,
                },
            )
        })();
        Reply {
            id: c.id,
            accepted: result.is_ok(),
            message: result.err().map_or_else(
                || "Durably acknowledged; controls affect operator PAPER only".into(),
                |e| format!("{e:#}"),
            ),
            status: self.operator_status(),
        }
    }

    fn open(
        path: &Path,
        reserve: u64,
        configuration: Value,
        run_id: &str,
        now: DateTime<Utc>,
    ) -> Result<Self> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        // Acquire the exclusive writer lock BEFORE restoring the ledger.
        let writer = DurableNdjsonWriter::open(path, reserve)?;
        let raw = std::fs::read(path)?;
        ensure!(
            raw.is_empty() || raw.ends_with(b"\n"),
            "incomplete price-cap journal tail; repair before restarting"
        );
        let operator = configuration["study_id"] == control::STUDY;
        let loss_boost = configuration["study_id"] == LOSS_BOOST_STUDY;
        let (version, study_id) = if loss_boost {
            (3, LOSS_BOOST_STUDY)
        } else if operator {
            (2, control::STUDY)
        } else {
            (1, STUDY_ID)
        };
        let mut state = State::new();
        if operator {
            state.operator = Some(Settings::default());
        }
        if loss_boost {
            let boost = LossBoost::from_configuration(&configuration)?;
            state.cash[1] = boost.capital;
            state.boost = Some(boost);
        }
        let mut boundary = None;
        for (index, line) in BufReader::new(raw.as_slice()).lines().enumerate() {
            let line = line?;
            let event: Event = serde_json::from_str(&line)
                .with_context(|| format!("invalid price-cap journal line {}", index + 1))?;
            if let Event::Registered {
                version: prior_version,
                study_id: prior_study_id,
                configuration: old,
                forward_window_start_ms,
                ..
            } = &event
            {
                ensure!(
                    index == 0
                        && *prior_version == version
                        && prior_study_id == study_id
                        && *old == configuration,
                    "price-cap study configuration changed; use a new batch"
                );
                boundary = Some(*forward_window_start_ms);
            } else {
                ensure!(boundary.is_some(), "price-cap journal missing registration");
            }
            state.apply(&event)?;
        }
        let forward_window_start_ms = match boundary {
            Some(value) => value,
            None => {
                let value = (now.timestamp_millis().div_euclid(300_000) + 1) * 300_000;
                writer.append(&Event::Registered {
                    version,
                    study_id: study_id.into(),
                    registered_at: now,
                    forward_window_start_ms: value,
                    configuration: configuration.clone(),
                })?;
                value
            }
        };
        writer.append(&Event::RunStarted {
            run_id: run_id.into(),
            started_at: now,
        })?;
        let number = |name: &str| {
            configuration[name]
                .as_i64()
                .context("missing study integer configuration")
        };
        if loss_boost {
            tracing::info!(target: "polypulse::console::late_resolution_winner_trading",
                file = %path.display(), %forward_window_start_ms, cash = %state.cash[1],
                "Independent BTC/ETH loss-boost PAPER ready");
        } else if operator {
            tracing::info!(target: "polypulse::console::late_resolution_winner_trading",
                file = %path.display(), %forward_window_start_ms, settings = ?state.operator, cash = %state.cash[1],
                "Operator PAPER ready: independent adjustable candidate only");
        } else {
            tracing::info!(target: "polypulse::console::late_resolution_winner_trading",
                file = %path.display(), %forward_window_start_ms, cash = ?state.cash,
                "Price-cap independent PAPER study ready: ETH .97 vs .98, BTC .98 in both arms");
        }
        Ok(Self {
            writer,
            operator,
            loss_boost,
            configuration: configuration.clone(),
            path: path.into(),
            state: Mutex::new(state),
            forward_window_start_ms,
            run_id: run_id.into(),
            delay_ms: number("delay_ms")? as u64,
            max_feed_age_ms: number("max_feed_age_ms")?,
            max_book_age_ms: number("max_book_age_ms")?,
            max_spread: Decimal::try_from(
                configuration["max_spread"]
                    .as_f64()
                    .context("study max spread")?,
            )?,
            min_depth: Decimal::try_from(
                configuration["min_depth_usdc"]
                    .as_f64()
                    .context("study min depth")?,
            )?,
        })
    }

    fn append(&self, state: &mut State, event: &Event) -> Result<()> {
        ensure!(
            !state.failed,
            "price-cap study is fail-closed after a persistence error"
        );
        if let Err(error) = self.writer.append(event).and_then(|_| state.apply(event)) {
            state.failed = true;
            return Err(error);
        }
        Ok(())
    }

    pub(crate) fn needs_market(&self, slug: &str, window_start_ms: i64) -> bool {
        if window_start_ms < self.forward_window_start_ms {
            return false;
        }
        let state = self.state.lock().expect("study state lock");
        if self.operator {
            return true;
        }
        if self.loss_boost {
            return !state.attempted.contains(&(CANDIDATE.into(), slug.into()));
        }
        ARMS.iter().any(|arm| {
            !state
                .attempted
                .contains(&(arm.to_string(), slug.to_string()))
        })
    }

    pub(crate) fn reject(&self, slug: &str) {
        let mut state = self.state.lock().expect("study state lock");
        for tracker in &mut state.confirmations {
            tracker.reject(slug);
        }
    }

    pub(crate) fn clear_confirmations(&self) {
        let mut state = self.state.lock().expect("study state lock");
        for tracker in &mut state.confirmations {
            tracker.clear();
        }
    }

    /// Runs before production admission and independently of production attempts.
    /// Attempts confirmed in the same scan share ONE delayed book read, so the
    /// price-cap arm is not given a different simulated fill merely by scheduling.
    pub(crate) fn observe(&self, evaluation: &WinnerEvaluation) -> Result<Vec<String>> {
        let mut state = self.state.lock().expect("study state lock");
        ensure!(!state.failed, "price-cap study persistence failed");
        if evaluation.window_start_ms < self.forward_window_start_ms {
            return Ok(vec![]);
        }
        let mut evaluation = evaluation.clone();
        evaluation.run_id = self.run_id.clone();
        evaluation.evaluation_id = uuid::Uuid::new_v4().to_string();
        evaluation.candidate_stage = if self.loss_boost {
            "loss_boost_paper"
        } else if self.operator {
            "operator_paper"
        } else {
            "independent_price_cap_paper"
        }
        .into();
        let mut decisions = Vec::new();
        let mut attempts = Vec::new();
        self.activate_locked(&mut state, evaluation.evaluated_at)?;
        for (index, arm) in ARMS.iter().enumerate() {
            if (self.operator || self.loss_boost) && index == 0 {
                continue;
            }
            let mut reason = admission_reason(
                &evaluation,
                arm,
                self.max_feed_age_ms,
                self.max_book_age_ms,
                self.max_spread,
                self.min_depth,
            )
            .to_string();
            let mut confirmations = 0;
            if state
                .attempted
                .contains(&(arm.to_string(), evaluation.slug.clone()))
            {
                reason = "already_attempted".into();
            } else if state.operator.as_ref().is_some_and(|c| c.paused) {
                reason = "operator_paused".into();
                state.confirmations[index].reject(&evaluation.slug);
            } else if reason == "eligible" {
                let progress = state.confirmations[index].observe(
                    &evaluation.slug,
                    evaluation.buy_yes,
                    evaluation.model_observed_at_ms.expect("reference gate"),
                    2,
                    self.max_feed_age_ms.saturating_mul(3),
                );
                confirmations = progress.confirmations;
                if progress.confirmed {
                    let budget = if let Some(boost) = &state.boost {
                        boost.budget(
                            LossBoost::asset(&evaluation.asset)?,
                            evaluation.evaluated_at,
                        )
                    } else {
                        state.operator.as_ref().map_or(dec!(5), |c| c.budget)
                    };
                    let size = fee_inclusive_depth_capped_size(
                        evaluation.ask_price,
                        evaluation.fee_per_share,
                        budget,
                        evaluation.ask_size,
                    );
                    let reserved_cost = size * (evaluation.ask_price + evaluation.fee_per_share);
                    if size <= dec!(0) {
                        reason = "zero_size".into();
                    } else if state.cash[index] < reserved_cost {
                        reason = "insufficient_paper_cash".into();
                    } else {
                        reason = "attempt".into();
                        attempts.push(Attempt {
                            attempt_id: uuid::Uuid::new_v4().to_string(),
                            arm: arm.to_string(),
                            evaluation: evaluation.clone(),
                            requested: size,
                            reserved_cost,
                            parameter_revision: state.operator.as_ref().map(|c| c.revision),
                            budget_limit: (self.operator || self.loss_boost).then_some(budget),
                            entry_epoch: state.operator.as_ref().map(|c| c.entry_epoch),
                        });
                    }
                } else {
                    reason = "awaiting_confirmation".into();
                }
            } else {
                state.confirmations[index].reject(&evaluation.slug);
            }
            decisions.push(Decision {
                arm: arm.to_string(),
                reason,
                confirmations,
            });
        }
        self.append(
            &mut state,
            &Event::Observed {
                evaluation: Box::new(evaluation),
                decisions,
            },
        )?;
        let mut ids = Vec::new();
        for attempt in attempts {
            ids.push(attempt.attempt_id.clone());
            self.append(
                &mut state,
                &Event::Attempted {
                    attempt: Box::new(attempt),
                },
            )?;
        }
        Ok(ids)
    }

    pub(crate) async fn execute(
        &self,
        executor: Arc<TradingExecutor>,
        token: U256,
        ids: Vec<String>,
    ) {
        if ids.is_empty() {
            return;
        }
        let first = {
            let state = self.state.lock().expect("study state lock");
            state.positions[&ids[0]].attempt.clone()
        };
        let e = &first.evaluation;
        let result = executor
            .shadow_buy_winner(token, e.ask_price, first.requested, e.fee_per_share)
            .await;
        let mut execution = match result {
            Ok(fill) => WinnerShadowExecution {
                request_latency_ms: Some(fill.request_latency_ms),
                total_latency_ms: Some(fill.total_latency_ms),
                run_id: e.run_id.clone(),
                evaluation_id: e.evaluation_id.clone(),
                slug: e.slug.clone(),
                simulated_at: Utc::now(),
                delay_ms: self.delay_ms,
                candidate_stage: "independent_price_cap_paper".into(),
                research_cohorts: vec![STUDY_ID.into()],
                outcome: if fill.filled > dec!(0) {
                    "filled"
                } else {
                    "no_fill"
                }
                .into(),
                limit_price: e.ask_price,
                requested: first.requested,
                filled: fill.filled,
                cost: fill.cost,
                average_price: fill.average_price,
                estimated_fee: fill.estimated_fee,
                error: None,
            },
            Err(error) => WinnerShadowExecution {
                request_latency_ms: None,
                total_latency_ms: None,
                run_id: e.run_id.clone(),
                evaluation_id: e.evaluation_id.clone(),
                slug: e.slug.clone(),
                simulated_at: Utc::now(),
                delay_ms: self.delay_ms,
                candidate_stage: "independent_price_cap_paper".into(),
                research_cohorts: vec![STUDY_ID.into()],
                outcome: "error".into(),
                limit_price: e.ask_price,
                requested: first.requested,
                filled: dec!(0),
                cost: dec!(0),
                average_price: dec!(0),
                estimated_fee: dec!(0),
                error: Some(error.to_string()),
            },
        };
        if self.loss_boost {
            execution.candidate_stage = "loss_boost_paper".into();
            execution.research_cohorts = vec![LOSS_BOOST_STUDY.into()];
        } else if self.operator {
            execution.candidate_stage = "operator_paper".into();
            execution.research_cohorts = vec![control::STUDY.into()];
        }
        // A read completed after market expiry is not evidence of an executable
        // pre-expiry fill. Preserve the raw result in the diagnostic, release the
        // simulated reservation, and exclude this market from matched performance.
        if execution.simulated_at >= e.end_date {
            let raw = serde_json::to_string(&execution).expect("serializable shadow execution");
            execution.outcome = "error".into();
            execution.filled = dec!(0);
            execution.cost = dec!(0);
            execution.average_price = dec!(0);
            execution.estimated_fee = dec!(0);
            execution.error = Some(format!("post_end_simulation_excluded: {raw}"));
        }
        self.finish_execution(ids, execution);
    }

    fn finish_execution(&self, ids: Vec<String>, mut execution: WinnerShadowExecution) {
        let mut state = self.state.lock().expect("study state lock");
        let first = &state.positions[&ids[0]].attempt;
        if state
            .operator
            .as_ref()
            .is_some_and(|c| c.paused || first.entry_epoch != Some(c.entry_epoch))
        {
            execution.outcome = "error".into();
            execution.filled = dec!(0);
            execution.cost = dec!(0);
            execution.average_price = dec!(0);
            execution.estimated_fee = dec!(0);
            execution.error = Some("operator_pause_cancelled_uncommitted_paper_fill".into());
        }
        for id in ids {
            let attempt = &state.positions[&id].attempt;
            let checked = validate_execution(attempt, &execution);
            if let Err(error) = checked.and_then(|_| {
                self.append(
                    &mut state,
                    &Event::Executed {
                        attempt_id: id,
                        execution: Box::new(execution.clone()),
                    },
                )
            }) {
                state.failed = true;
                tracing::error!(target: "polypulse::strategy::late_resolution_winner_trading", %error,
                    "Price-cap study execution persistence failed; fail-closed");
                break;
            }
        }
    }

    pub(crate) fn due_slugs(&self, now: DateTime<Utc>) -> Result<Vec<String>> {
        let state = self.state.lock().expect("study state lock");
        ensure!(!state.failed, "price-cap study persistence failed");
        let mut slugs: Vec<_> = state
            .positions
            .values()
            .filter(|p| p.execution.is_some() && !p.settled && p.attempt.evaluation.end_date <= now)
            .map(|p| p.attempt.evaluation.slug.clone())
            .collect();
        slugs.sort();
        slugs.dedup();
        Ok(slugs)
    }

    pub(crate) fn resolve(
        &self,
        slug: &str,
        now: DateTime<Utc>,
        closed: bool,
        prices: &(Decimal, Decimal),
    ) -> Result<usize> {
        let yes_won = prices.0 >= dec!(0.999) && prices.1 <= dec!(0.001);
        let no_won = prices.1 >= dec!(0.999) && prices.0 <= dec!(0.001);
        if !closed || yes_won == no_won {
            return Ok(0);
        }
        let mut state = self.state.lock().expect("study state lock");
        let ids: Vec<_> = state
            .positions
            .iter()
            .filter(|(_, p)| {
                p.attempt.evaluation.slug == slug
                    && p.attempt.evaluation.end_date <= now
                    && p.execution.is_some()
                    && !p.settled
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in &ids {
            let p = &state.positions[id];
            let execution = p.execution.as_ref().expect("selected executed position");
            let won = p.attempt.evaluation.buy_yes == yes_won;
            let payout = if won { execution.filled } else { dec!(0) };
            let pnl = payout - execution.cost;
            let arm = p.attempt.arm.clone();
            self.append(
                &mut state,
                &Event::Settled {
                    attempt_id: id.clone(),
                    won,
                    payout,
                    pnl,
                    resolved_at: now,
                },
            )?;
            tracing::info!(target: "polypulse::console::late_resolution_winner_trading", %slug, %arm, %won,
                %pnl, cash = %state.cash[arm_index(&arm)?], "Price-cap independent PAPER settled");
        }
        Ok(ids.len())
    }
}

fn admission_reason(
    e: &WinnerEvaluation,
    arm: &str,
    feed_age: i64,
    book_age: i64,
    spread: Decimal,
    depth: Decimal,
) -> &'static str {
    if !matches!(e.asset.to_ascii_lowercase().as_str(), "btc" | "eth") {
        return "unsupported_asset";
    }
    if e.seconds_remaining <= 0 || e.seconds_remaining > 60 {
        return "outside_entry_window";
    }
    if e.ask_price < dec!(0.96) || e.ask_price > dec!(0.98) {
        return "outside_control_quote_range";
    }
    if e.model_observed_at_ms.is_none_or(|t| t <= 0)
        || e.opening_twap.is_none()
        || e.current_twap.is_none()
        || e.model_audit
            .as_ref()
            .is_none_or(|a| a.current_age_ms > feed_age)
        || e.failed_checks
            .iter()
            .any(|r| r != "ReversalBoundary" && r != "NetEvTooLow")
    {
        return "invalid_reference_or_market";
    }
    if e.book_age_ms < 0
        || e.book_age_ms > book_age
        || e.bid_price <= dec!(0)
        || e.spread < dec!(0)
        || e.spread > spread
        || e.depth_usdc < depth
    {
        return "invalid_book_quality";
    }
    if arm == CANDIDATE && e.asset.eq_ignore_ascii_case("eth") && e.ask_price > dec!(0.97) {
        return "eth_ask_above_cap";
    }
    "eligible"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn configuration() -> Value {
        json!({"delay_ms":250,"max_feed_age_ms":5000,"max_book_age_ms":1000,
            "max_spread":0.03,"min_depth_usdc":10.0,
            "paper_only":true,"automatic_promotion":false,"initial_cash_per_arm":"100","max_order_usdc":"5",
            "arms":{(CONTROL):{"btc_max_ask":"0.98","eth_max_ask":"0.98"},
                    (CANDIDATE):{"btc_max_ask":"0.98","eth_max_ask":"0.97"}}})
    }
    fn now() -> DateTime<Utc> {
        "2026-10-26T00:00:01Z".parse().unwrap()
    }
    fn path() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("winner-price-cap-{}.ndjson", uuid::Uuid::new_v4()))
    }
    fn study(path: &Path) -> PriceCapStudy {
        PriceCapStudy::open(path, 0, configuration(), "run-007", now()).unwrap()
    }
    fn evaluation(asset: &str, ask: Decimal, tick: i64) -> WinnerEvaluation {
        let start = now().timestamp_millis().div_euclid(300_000) * 300_000 + 300_000;
        let observed = start + 240_000 + tick * 1000;
        serde_json::from_value(json!({
            "model_audit":{"version":1,"opening_observed_at_ms":start,"opening_skew_ms":0,
                "current_age_ms":200,"observation_count":120,"observed_volatility_bps":0.1,
                "minimum_volatility_bps":1.2,"volatility_floor_active":true,
                "probability_without_volatility_floor":0.99,"reference_topic":"crypto_prices_twap_sixty",
                "horizon_volatility":[]},
            "run_id":"run-007","evaluation_id":"original-main-evaluation",
            "candidate_stage":"rejected_shadow","asset":asset,
            "slug":format!("{}-updown-5m-{}",asset,start/1000),
            "end_date":DateTime::from_timestamp_millis(start+300_000).unwrap(),
            "evaluated_at":DateTime::from_timestamp_millis(observed+200).unwrap(),
            "window_start_ms":start,"seconds_remaining":60-tick,"buy_yes":true,
            "bid_price":ask-dec!(0.01),"ask_price":ask,"ask_size":"100",
            "fee_per_share":"0.001","rejection":"ReversalBoundary",
            "failed_checks":["ReversalBoundary","NetEvTooLow"],
            "opening_twap":"1000","current_twap":"1000.1","model_observed_at_ms":observed,
            "model_probability":"0.51","model_net_ev":"-0.48",
            "directional_margin_bps":1.0,"reversal_boundary_bps":20.0,
            "volatility_bps_per_sqrt_second":1.2,"spread":"0.01","depth_usdc":"97",
            "book_age_ms":10
        })).unwrap()
    }
    fn finish(study: &PriceCapStudy, ids: &[String], filled: bool) {
        let mut state = study.state.lock().unwrap();
        for id in ids {
            let a = &state.positions[id].attempt;
            let e = &a.evaluation;
            let execution = WinnerShadowExecution {
                request_latency_ms: Some(20),
                total_latency_ms: Some(270),
                run_id: e.run_id.clone(),
                evaluation_id: e.evaluation_id.clone(),
                slug: e.slug.clone(),
                simulated_at: e.evaluated_at,
                delay_ms: 250,
                candidate_stage: "independent_price_cap_paper".into(),
                research_cohorts: vec![],
                outcome: if filled { "filled" } else { "no_fill" }.into(),
                limit_price: e.ask_price,
                requested: a.requested,
                filled: if filled { a.requested } else { dec!(0) },
                cost: if filled { a.reserved_cost } else { dec!(0) },
                average_price: if filled {
                    e.ask_price + e.fee_per_share
                } else {
                    dec!(0)
                },
                estimated_fee: if filled {
                    a.requested * e.fee_per_share
                } else {
                    dec!(0)
                },
                error: None,
            };
            study
                .append(
                    &mut state,
                    &Event::Executed {
                        attempt_id: id.clone(),
                        execution: Box::new(execution),
                    },
                )
                .unwrap();
        }
    }

    #[test]
    fn cap_can_enter_later_after_control_has_already_traded() {
        let path = path();
        let s = study(&path);
        assert!(s
            .observe(&evaluation("eth", dec!(0.98), 1))
            .unwrap()
            .is_empty());
        let control = s.observe(&evaluation("eth", dec!(0.98), 2)).unwrap();
        assert_eq!(control.len(), 1);
        finish(&s, &control, true);
        assert!(s.needs_market(
            &evaluation("eth", dec!(0.97), 3).slug,
            evaluation("eth", dec!(0.97), 3).window_start_ms
        ));
        assert!(s
            .observe(&evaluation("eth", dec!(0.97), 3))
            .unwrap()
            .is_empty());
        let cap = s.observe(&evaluation("eth", dec!(0.97), 4)).unwrap();
        assert_eq!(cap.len(), 1);
        finish(&s, &cap, true);
        let state = s.state.lock().unwrap();
        assert_eq!(state.positions[&control[0]].attempt.arm, CONTROL);
        assert_eq!(state.positions[&cap[0]].attempt.arm, CANDIDATE);
        assert!(
            state.positions[&control[0]].attempt.evaluation.evaluated_at
                < state.positions[&cap[0]].attempt.evaluation.evaluated_at
        );
        drop(state);
        drop(s);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn duplicate_twap_and_rejected_price_do_not_supply_confirmations() {
        let path = path();
        let s = study(&path);
        let e = evaluation("eth", dec!(0.97), 1);
        assert!(s.observe(&e).unwrap().is_empty());
        assert!(s.observe(&e).unwrap().is_empty());
        let control = s.observe(&evaluation("eth", dec!(0.98), 2)).unwrap();
        assert_eq!(control.len(), 1);
        assert!(s
            .observe(&evaluation("eth", dec!(0.97), 3))
            .unwrap()
            .is_empty());
        assert_eq!(
            s.observe(&evaluation("eth", dec!(0.97), 4)).unwrap().len(),
            1
        );
        drop(s);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn btc_098_uses_same_observation_and_no_fill_consumes_both_attempts() {
        let path = path();
        let s = study(&path);
        s.observe(&evaluation("btc", dec!(0.98), 1)).unwrap();
        let ids = s.observe(&evaluation("btc", dec!(0.98), 2)).unwrap();
        assert_eq!(ids.len(), 2);
        {
            let st = s.state.lock().unwrap();
            assert_eq!(
                st.positions[&ids[0]].attempt.evaluation.evaluation_id,
                st.positions[&ids[1]].attempt.evaluation.evaluation_id
            );
        }
        finish(&s, &ids, false);
        assert!(s
            .observe(&evaluation("btc", dec!(0.98), 3))
            .unwrap()
            .is_empty());
        assert_eq!(s.state.lock().unwrap().cash, [dec!(100); 2]);
        drop(s);
        let s = study(&path);
        assert!(s
            .observe(&evaluation("btc", dec!(0.98), 4))
            .unwrap()
            .is_empty());
        drop(s);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn cash_pending_settlement_and_restart_are_conserved() {
        let path = path();
        let s = study(&path);
        let e = evaluation("eth", dec!(0.97), 1);
        s.observe(&e).unwrap();
        let ids = s.observe(&evaluation("eth", dec!(0.97), 2)).unwrap();
        assert_eq!(ids.len(), 2);
        finish(&s, &ids, true);
        let pending_cash = s.state.lock().unwrap().cash;
        assert!(pending_cash
            .iter()
            .all(|c| *c >= dec!(95) && *c < dec!(100)));
        assert_eq!(
            s.resolve(
                &e.slug,
                e.end_date - chrono::Duration::seconds(1),
                true,
                &(dec!(1), dec!(0))
            )
            .unwrap(),
            0
        );
        assert_eq!(
            s.resolve(&e.slug, e.end_date, false, &(dec!(1), dec!(0)))
                .unwrap(),
            0
        );
        assert_eq!(
            s.resolve(&e.slug, e.end_date, true, &(dec!(0.5), dec!(0.5)))
                .unwrap(),
            0
        );
        drop(s);
        let s = study(&path);
        assert_eq!(s.state.lock().unwrap().cash, pending_cash);
        assert_eq!(s.due_slugs(e.end_date).unwrap(), vec![e.slug.clone()]);
        assert_eq!(
            s.resolve(&e.slug, e.end_date, true, &(dec!(1), dec!(0)))
                .unwrap(),
            2
        );
        assert_eq!(
            s.resolve(&e.slug, e.end_date, true, &(dec!(1), dec!(0)))
                .unwrap(),
            0
        );
        let cash = s.state.lock().unwrap().cash;
        assert!(cash.iter().all(|c| *c > dec!(100)));
        drop(s);
        let s = study(&path);
        assert_eq!(s.state.lock().unwrap().cash, cash);
        assert!(s.due_slugs(e.end_date).unwrap().is_empty());
        drop(s);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn invalid_feed_or_book_rejects_both_arms_without_relaxing_quality() {
        let mut e = evaluation("eth", dec!(0.97), 1);
        assert_eq!(
            admission_reason(&e, CANDIDATE, 5000, 1000, dec!(0.03), dec!(10)),
            "eligible"
        );
        e.failed_checks.push("StalePrice".into());
        assert_eq!(
            admission_reason(&e, CONTROL, 5000, 1000, dec!(0.03), dec!(10)),
            "invalid_reference_or_market"
        );
        e.failed_checks.pop();
        e.book_age_ms = 1001;
        assert_eq!(
            admission_reason(&e, CANDIDATE, 5000, 1000, dec!(0.03), dec!(10)),
            "invalid_book_quality"
        );
        e.book_age_ms = 0;
        e.depth_usdc = dec!(9);
        assert_eq!(
            admission_reason(&e, CANDIDATE, 5000, 1000, dec!(0.03), dec!(10)),
            "invalid_book_quality"
        );
    }

    #[test]
    fn first_partial_window_configuration_change_and_corrupt_tail_fail_closed() {
        let path = path();
        let s = study(&path);
        let mut e = evaluation("eth", dec!(0.97), 1);
        e.window_start_ms -= 300_000;
        assert!(s.observe(&e).unwrap().is_empty());
        drop(s);
        let mut changed = configuration();
        changed["delay_ms"] = json!(500);
        assert!(PriceCapStudy::open(&path, 0, changed, "new-run", now()).is_err());
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{incomplete")
            .unwrap();
        assert!(PriceCapStudy::open(&path, 0, configuration(), "new-run", now()).is_err());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn arm_cash_and_connection_resets_are_independent() {
        let path = path();
        let s = study(&path);
        s.observe(&evaluation("eth", dec!(0.97), 1)).unwrap();
        s.reject(&evaluation("eth", dec!(0.97), 1).slug);
        assert!(s
            .observe(&evaluation("eth", dec!(0.97), 2))
            .unwrap()
            .is_empty());
        s.state.lock().unwrap().cash[0] = dec!(0);
        let ids = s.observe(&evaluation("eth", dec!(0.97), 3)).unwrap();
        assert_eq!(ids.len(), 1);
        assert_eq!(
            s.state.lock().unwrap().positions[&ids[0]].attempt.arm,
            CANDIDATE
        );
        drop(s);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn interrupted_execution_keeps_cash_reserved_and_cannot_repeat_after_restart() {
        let path = path();
        let s = study(&path);
        s.observe(&evaluation("eth", dec!(0.97), 1)).unwrap();
        let ids = s.observe(&evaluation("eth", dec!(0.97), 2)).unwrap();
        assert_eq!(ids.len(), 2);
        let cash = s.state.lock().unwrap().cash;
        drop(s);
        let s = study(&path);
        assert_eq!(s.state.lock().unwrap().cash, cash);
        assert!(s
            .observe(&evaluation("eth", dec!(0.97), 3))
            .unwrap()
            .is_empty());
        assert!(s
            .due_slugs(evaluation("eth", dec!(0.97), 3).end_date)
            .unwrap()
            .is_empty());
        drop(s);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn exclusive_lock_and_expired_fill_validation_protect_the_ledger() {
        let path = path();
        let s = study(&path);
        assert!(PriceCapStudy::open(&path, 0, configuration(), "other-run", now()).is_err());
        s.observe(&evaluation("eth", dec!(0.97), 1)).unwrap();
        let ids = s.observe(&evaluation("eth", dec!(0.97), 2)).unwrap();
        finish(&s, &ids, true);
        let state = s.state.lock().unwrap();
        let position = &state.positions[&ids[0]];
        let mut expired = position.execution.clone().unwrap();
        expired.simulated_at = position.attempt.evaluation.end_date;
        assert!(validate_execution(&position.attempt, &expired).is_err());
        drop(state);
        drop(s);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    #[ignore = "cross-language integration requires Python 3"]
    fn rust_journal_is_audited_by_python_report() {
        let path = path();
        let s = study(&path);
        let e = evaluation("eth", dec!(0.97), 1);
        s.observe(&e).unwrap();
        let ids = s.observe(&evaluation("eth", dec!(0.97), 2)).unwrap();
        finish(&s, &ids, true);
        s.resolve(&e.slug, e.end_date, true, &(dec!(1), dec!(0)))
            .unwrap();
        let cash = s.state.lock().unwrap().cash;
        drop(s);
        let output_dir = path.with_extension("report");
        let output = std::process::Command::new("python3")
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/scripts/late_resolution_winner_price_cap_report.py"
            ))
            .arg("--input")
            .arg(&path)
            .arg("--output-dir")
            .arg(&output_dir)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report: Value =
            serde_json::from_slice(&std::fs::read(output_dir.join("report.json")).unwrap())
                .unwrap();
        for (index, arm) in ARMS.iter().enumerate() {
            assert_eq!(report["arms"][arm]["total"]["settled_fills"], 1);
            assert_eq!(report["arms"][arm]["cash"], cash[index].to_string());
        }
        assert_eq!(
            report["matched_markets"]["eth"]["candidate_minus_control"],
            "0.00000"
        );
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir_all(output_dir).unwrap();
    }
    fn operator_configuration() -> Value {
        let mut c = configuration();
        c["study_id"] = json!(control::STUDY);
        c["max_order_usdc"] = json!("100");
        c["initial_budget"] = json!("5");
        c["initial_paused"] = json!(true);
        c["arms"].as_object_mut().unwrap().remove(CONTROL);
        c
    }
    fn operator(path: &Path) -> PriceCapStudy {
        PriceCapStudy::open(
            path,
            0,
            operator_configuration(),
            &uuid::Uuid::new_v4().to_string(),
            now(),
        )
        .unwrap()
    }
    fn command(s: &PriceCapStudy, action: Action) -> Command {
        let status = s.operator_status().unwrap();
        Command {
            id: uuid::Uuid::new_v4().to_string(),
            run_id: s.run_id.clone(),
            journal: status.journal,
            expected_revision: status.settings.revision,
            action,
        }
    }
    fn apply(s: &PriceCapStudy, action: Action) -> Reply {
        let r = s.command(command(s, action));
        assert!(r.accepted, "{}", r.message);
        r
    }
    fn future_evaluation(asset: &str, start: i64, tick: i64) -> WinnerEvaluation {
        let mut e = evaluation(asset, dec!(0.97), tick);
        e.window_start_ms = start;
        e.slug = format!("{asset}-updown-5m-{}", start / 1000);
        e.evaluated_at = DateTime::from_timestamp_millis(start + 240_200 + tick * 1000).unwrap();
        e.end_date = DateTime::from_timestamp_millis(start + 300_000).unwrap();
        e.model_observed_at_ms = Some(start + 240_000 + tick * 1000);
        e
    }
    fn synthetic_execution(s: &PriceCapStudy, id: &str) -> WinnerShadowExecution {
        let state = s.state.lock().unwrap();
        let a = &state.positions[id].attempt;
        let e = &a.evaluation;
        WinnerShadowExecution {
            request_latency_ms: Some(20),
            total_latency_ms: Some(270),
            run_id: e.run_id.clone(),
            evaluation_id: e.evaluation_id.clone(),
            slug: e.slug.clone(),
            simulated_at: e.evaluated_at,
            delay_ms: 250,
            candidate_stage: "operator_paper".into(),
            research_cohorts: vec![control::STUDY.into()],
            outcome: "filled".into(),
            limit_price: e.ask_price,
            requested: a.requested,
            filled: a.requested,
            cost: a.reserved_cost,
            average_price: e.ask_price + e.fee_per_share,
            estimated_fee: a.requested * e.fee_per_share,
            error: None,
        }
    }
    #[test]
    fn operator_pauses_entries_keeps_observations_and_settles_while_paused() {
        let p = path();
        let s = operator(&p);
        assert!(s.operator_status().unwrap().settings.paused);
        assert!(s
            .observe(&evaluation("eth", dec!(0.97), 1))
            .unwrap()
            .is_empty());
        assert!(s
            .observe(&evaluation("eth", dec!(0.97), 2))
            .unwrap()
            .is_empty());
        assert_eq!(
            std::fs::read_to_string(&p)
                .unwrap()
                .matches("\"action\":\"observed\"")
                .count(),
            2
        );
        apply(&s, Action::Resume);
        assert!(s
            .observe(&evaluation("eth", dec!(0.97), 3))
            .unwrap()
            .is_empty());
        let ids = s.observe(&evaluation("eth", dec!(0.97), 4)).unwrap();
        assert_eq!(ids.len(), 1);
        s.finish_execution(ids.clone(), synthetic_execution(&s, &ids[0]));
        apply(&s, Action::Pause);
        assert_eq!(s.operator_status().unwrap().pending_fills, 1);
        let e = evaluation("eth", dec!(0.97), 4);
        assert_eq!(
            s.resolve(
                &e.slug,
                e.end_date + chrono::Duration::seconds(10),
                true,
                &(dec!(1), dec!(0))
            )
            .unwrap(),
            1
        );
        assert!(s.operator_status().unwrap().cash > dec!(100));
        assert!(s.operator_status().unwrap().settings.paused);
        let mut ledger = crate::winner_dashboard::ledger::Ledger::open(&p).unwrap();
        ledger.refresh().unwrap();
        let view = ledger
            .view(e.evaluated_at.date_naive(), 1, None, 0, 20)
            .unwrap();
        assert_eq!(view.total[2].wins, 1);
        assert_eq!(view.total[2].pending, 0);
        drop(ledger);
        drop(s);
        std::fs::remove_file(p).unwrap();
    }
    #[test]
    fn operator_budget_activates_at_boundary_and_survives_restart_with_metadata() {
        let p = path();
        let s = operator(&p);
        let c = command(&s, Action::SetBudget { amount: dec!(10) });
        assert!(s.command(c.clone()).accepted);
        let pending = s.operator_status().unwrap().settings;
        let boundary = pending.effective_window_ms.unwrap();
        s.activate_operator(DateTime::from_timestamp_millis(boundary - 1).unwrap())
            .unwrap();
        assert_eq!(s.operator_status().unwrap().settings.budget, dec!(5));
        drop(s);
        let s = operator(&p);
        assert_eq!(s.operator_status().unwrap().settings, pending);
        s.activate_operator(DateTime::from_timestamp_millis(boundary).unwrap())
            .unwrap();
        assert_eq!(s.operator_status().unwrap().settings.budget, dec!(10));
        let revision = s.operator_status().unwrap().settings.revision;
        assert!(!s.command(c).accepted); // a previous engine session cannot control a restarted engine
        assert_eq!(s.operator_status().unwrap().settings.revision, revision);
        apply(&s, Action::Resume);
        s.observe(&future_evaluation("btc", boundary, 1)).unwrap();
        let ids = s.observe(&future_evaluation("btc", boundary, 2)).unwrap();
        let x = synthetic_execution(&s, &ids[0]);
        assert!(x.cost > dec!(5) && x.cost <= dec!(10));
        s.finish_execution(ids, x);
        let mut l = crate::winner_dashboard::ledger::Ledger::open(&p).unwrap();
        l.refresh().unwrap();
        let day = future_evaluation("btc", boundary, 2)
            .evaluated_at
            .with_timezone(&chrono::FixedOffset::east_opt(8 * 3600).unwrap())
            .date_naive();
        let view = l.view(day, 1, None, 0, 20).unwrap();
        assert_eq!(view.rows[0].budget_limit, Some(dec!(10)));
        assert!(view.rows[0].parameter_revision.is_some());
        assert_eq!(view.total[2].cost, dec!(0)); // pending cost is exposure, never realized cost/PnL
        assert!(view.total[2].exposure > dec!(5));
        drop(l);
        drop(s);
        let s = operator(&p);
        assert!(!s.operator_status().unwrap().settings.paused);
        assert_eq!(s.operator_status().unwrap().pending_fills, 1);
        drop(s);
        std::fs::remove_file(p).unwrap();
    }
    #[test]
    fn operator_rejects_stale_wrong_target_invalid_and_reused_commands() {
        let p = path();
        let s = operator(&p);
        let c = command(&s, Action::Resume);
        let stale = command(&s, Action::Pause);
        assert!(s.command(c.clone()).accepted);
        assert!(!s.command(stale).accepted);
        assert!(s.command(c.clone()).accepted);
        let mut changed = c;
        changed.action = Action::Pause;
        assert!(!s.command(changed).accepted);
        for amount in [dec!(0), dec!(-5), dec!(101), dec!(1.001)] {
            assert!(
                !s.command(command(&s, Action::SetBudget { amount }))
                    .accepted
            );
        }
        let mut c = command(&s, Action::Pause);
        c.run_id = "old-run".into();
        assert!(!s.command(c).accepted);
        let mut c = command(&s, Action::Pause);
        c.journal.push_str("wrong");
        assert!(!s.command(c).accepted);
        s.state.lock().unwrap().failed = true;
        assert!(!s.command(command(&s, Action::Resume)).accepted);
        drop(s);
        std::fs::remove_file(p).unwrap();
    }
    #[test]
    fn pause_resume_race_cannot_commit_old_paper_result_or_reuse_confirmation() {
        let p = path();
        let s = operator(&p);
        apply(&s, Action::Resume);
        s.observe(&evaluation("eth", dec!(0.97), 1)).unwrap();
        apply(&s, Action::Pause);
        apply(&s, Action::Resume);
        assert!(s
            .observe(&evaluation("eth", dec!(0.97), 2))
            .unwrap()
            .is_empty());
        let ids = s.observe(&evaluation("eth", dec!(0.97), 3)).unwrap();
        let x = synthetic_execution(&s, &ids[0]);
        apply(&s, Action::Pause);
        apply(&s, Action::Resume);
        s.finish_execution(ids.clone(), x);
        let st = s.state.lock().unwrap();
        let x = st.positions[&ids[0]].execution.as_ref().unwrap();
        assert_eq!(x.outcome, "error");
        assert_eq!(x.cost, dec!(0));
        assert_eq!(st.cash[1], dec!(100));
        drop(st);
        assert!(s
            .observe(&evaluation("eth", dec!(0.97), 4))
            .unwrap()
            .is_empty());
        drop(s);
        std::fs::remove_file(p).unwrap();
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn operator_socket_acknowledges_durably_without_touching_frozen_journal() {
        let p = path();
        let baseline = study(&p);
        let frozen = std::fs::read(&p).unwrap();
        let op_path = path();
        let op = Arc::new(operator(&op_path));
        let display = crate::winner_dashboard::live::LiveState::new(&op.run_id, &p);
        display.set_operator(op.clone());
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let socket = crate::winner_dashboard::live::start(
            display,
            Arc::new(crate::late_resolution_winner_model::ChainlinkTwapStore::default()),
            Arc::new(crate::runtime_metrics::RuntimeMetrics::default()),
            stop.clone(),
            5000,
            1000,
            5000,
        )
        .unwrap();
        let c = command(&op, Action::Resume);
        let client_path = socket.clone();
        let ack = tokio::task::spawn_blocking(move || control::send(&client_path, &c))
            .await
            .unwrap()
            .unwrap();
        assert!(ack.accepted);
        assert!(!ack.status.unwrap().settings.paused);
        assert!(std::fs::read_to_string(&op_path)
            .unwrap()
            .contains("operator_changed"));
        let funding = command(&op, Action::Deposit { amount: dec!(500) });
        let client_path = socket.clone();
        let retry = funding.clone();
        let credit = tokio::task::spawn_blocking(move || control::send(&client_path, &funding))
            .await
            .unwrap()
            .unwrap();
        assert!(credit.accepted);
        assert_eq!(credit.status.unwrap().capital, dec!(600));
        let client_path = socket.clone();
        let retry = tokio::task::spawn_blocking(move || control::send(&client_path, &retry))
            .await
            .unwrap()
            .unwrap();
        assert!(retry.accepted);
        assert_eq!(retry.status.unwrap().deposits, dec!(500));
        assert_eq!(
            std::fs::read_to_string(&op_path)
                .unwrap()
                .matches("\"action\":\"operator_funded\"")
                .count(),
            1
        );
        assert_eq!(std::fs::read(&p).unwrap(), frozen);
        assert!(
            !crate::winner_dashboard::live::read(&socket, &p)
                .unwrap()
                .operator
                .unwrap()
                .settings
                .paused
        );
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        for _ in 0..30 {
            if !socket.exists() && !control::socket(&socket).exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        }
        assert!(!socket.exists());
        assert!(!control::socket(&socket).exists());
        drop(baseline);
        drop(op);
        std::fs::remove_file(p).unwrap();
        std::fs::remove_file(op_path).unwrap();
    }
    #[test]
    #[ignore = "cross-language integration requires Python 3"]
    fn operator_rust_journal_is_audited_by_python() {
        let p = path();
        let s = operator(&p);
        apply(&s, Action::Resume);
        s.observe(&evaluation("eth", dec!(0.97), 1)).unwrap();
        let ids = s.observe(&evaluation("eth", dec!(0.97), 2)).unwrap();
        s.finish_execution(ids.clone(), synthetic_execution(&s, &ids[0]));
        let e = evaluation("eth", dec!(0.97), 2);
        s.resolve(&e.slug, e.end_date, true, &(dec!(1), dec!(0)))
            .unwrap();
        apply(&s, Action::SetBudget { amount: dec!(10) });
        let boundary = s
            .operator_status()
            .unwrap()
            .settings
            .effective_window_ms
            .unwrap();
        s.activate_operator(DateTime::from_timestamp_millis(boundary).unwrap())
            .unwrap();
        s.observe(&future_evaluation("btc", boundary, 1)).unwrap();
        let ids = s.observe(&future_evaluation("btc", boundary, 2)).unwrap();
        s.finish_execution(ids.clone(), synthetic_execution(&s, &ids[0]));
        apply(&s, Action::Pause);
        let e = future_evaluation("btc", boundary, 2);
        s.resolve(&e.slug, e.end_date, true, &(dec!(0), dec!(1)))
            .unwrap();
        apply(&s, Action::Deposit { amount: dec!(500) });
        let cash = s.operator_status().unwrap().cash;
        let output = std::process::Command::new("python3")
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/scripts/late_resolution_winner_operator_report.py"
            ))
            .arg("--input")
            .arg(&p)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["total"]["wins"], 1);
        assert_eq!(report["total"]["losses"], 1);
        assert_eq!(report["cash"], cash.to_string());
        assert_eq!(report["capital"], "600");
        assert_eq!(report["deposits"], "500");
        assert_eq!(report["by_revision"].as_object().unwrap().len(), 2);
        drop(s);
        std::fs::remove_file(p).unwrap();
    }
    #[test]
    fn paper_deposit_preserves_trade_pnl_pending_exposure_and_restores_after_restart() {
        let p = path();
        let s = operator(&p);
        apply(&s, Action::Resume);
        s.observe(&evaluation("btc", dec!(0.97), 1)).unwrap();
        let ids = s.observe(&evaluation("btc", dec!(0.97), 2)).unwrap();
        s.finish_execution(ids.clone(), synthetic_execution(&s, &ids[0]));
        let e = evaluation("btc", dec!(0.97), 2);
        s.resolve(&e.slug, e.end_date, true, &(dec!(0), dec!(1)))
            .unwrap();
        s.observe(&evaluation("eth", dec!(0.97), 1)).unwrap();
        let pending = s.observe(&evaluation("eth", dec!(0.97), 2)).unwrap();
        let before = s.operator_status().unwrap();
        let c = command(&s, Action::Deposit { amount: dec!(500) });
        let reply = s.command(c.clone());
        assert!(reply.accepted, "{}", reply.message);
        let after = reply.status.unwrap();
        assert_eq!(after.cash, before.cash + dec!(500));
        assert_eq!(after.capital, dec!(600));
        assert_eq!(after.deposits, dec!(500));
        assert_eq!(after.inflight, before.inflight);
        assert_eq!(after.settings.entry_epoch, before.settings.entry_epoch);
        assert_eq!(after.settings.paused, before.settings.paused);
        assert_eq!(after.settings.budget, before.settings.budget);
        assert!(s.command(c).accepted);
        assert_eq!(s.operator_status().unwrap().deposits, dec!(500));
        let mut l = crate::winner_dashboard::ledger::Ledger::open(&p).unwrap();
        l.refresh().unwrap();
        let day = e
            .evaluated_at
            .with_timezone(&chrono::FixedOffset::east_opt(8 * 3600).unwrap())
            .date_naive();
        let view = l.view(day, 1, None, 0, 20).unwrap();
        assert_eq!(view.capital, dec!(600));
        assert_eq!(view.deposits, dec!(500));
        assert!(view.total[2].pnl < dec!(0));
        assert_eq!(view.total[2].unknown, 1);
        assert_eq!(
            view.capital + view.total[2].pnl - view.total[2].exposure - view.total[2].reserved,
            after.cash
        );
        // Funding does not cancel a previously reserved order or alter its original size.
        let x = synthetic_execution(&s, &pending[0]);
        s.finish_execution(pending, x);
        assert_eq!(s.operator_status().unwrap().pending_fills, 1);
        let cash = s.operator_status().unwrap().cash;
        drop(l);
        drop(s);
        let s = operator(&p);
        assert_eq!(s.operator_status().unwrap().cash, cash);
        assert_eq!(s.operator_status().unwrap().capital, dec!(600));
        assert!(!s.operator_status().unwrap().settings.paused);
        drop(s);
        std::fs::remove_file(p).unwrap();
    }
    #[test]
    fn paper_deposit_is_idempotent_keeps_pause_and_rejects_invalid_or_frozen_targets() {
        let p = path();
        let s = operator(&p);
        for amount in [dec!(0), dec!(-1), dec!(0.001), dec!(1000000.01)] {
            assert!(!s.command(command(&s, Action::Deposit { amount })).accepted);
        }
        let c = command(&s, Action::Deposit { amount: dec!(0.01) });
        let stale = command(&s, Action::Deposit { amount: dec!(500) });
        assert!(s.command(c.clone()).accepted);
        assert!(s.command(c.clone()).accepted);
        assert_eq!(s.operator_status().unwrap().cash, dec!(100.01));
        assert!(s.operator_status().unwrap().settings.paused);
        assert!(!s.command(stale).accepted);
        let mut reuse = c.clone();
        reuse.action = Action::Deposit { amount: dec!(1) };
        assert!(!s.command(reuse).accepted);
        let base_path = path();
        let base = study(&base_path);
        let raw = std::fs::read(&base_path).unwrap();
        let mut target = c;
        target.run_id = base.run_id.clone();
        target.journal = crate::winner_dashboard::live::canonical(&base_path)
            .to_string_lossy()
            .into_owned();
        assert!(!base.command(target).accepted);
        assert_eq!(raw, std::fs::read(&base_path).unwrap());
        drop(base);
        std::fs::remove_file(base_path).unwrap();
        drop(s);
        std::fs::remove_file(p).unwrap();
    }
    #[test]
    fn duplicate_funding_journal_event_fails_closed_in_engine_and_viewer() {
        let p = path();
        let s = operator(&p);
        let c = command(&s, Action::Deposit { amount: dec!(500) });
        assert!(s.command(c.clone()).accepted);
        drop(s);
        // A tampered replay with a fresh revision but the same credit ID must never credit twice.
        let mut repeated = c;
        repeated.expected_revision = 1;
        let event = Event::OperatorFunded {
            command: repeated,
            amount: dec!(500),
            revision: 2,
            funded_at: Utc::now(),
        };
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        writeln!(f, "{}", serde_json::to_string(&event).unwrap()).unwrap();
        drop(f);
        let mut l = crate::winner_dashboard::ledger::Ledger::open(&p).unwrap();
        assert!(l.refresh().is_err());
        drop(l);
        assert!(PriceCapStudy::open(&p, 0, operator_configuration(), "restart", now()).is_err());
        std::fs::remove_file(p).unwrap();
    }

    #[test]
    fn loss_boost_is_per_asset_restarts_on_each_loss_and_survives_replay() {
        let p = path();
        let c = PriceCapStudy::boost_configuration(configuration(), dec!(5), dec!(100));
        let s = PriceCapStudy::open(&p, 0, c.clone(), "boost-run", now()).unwrap();
        let attempt = |s: &PriceCapStudy, asset: &str, windows: i64, expected| {
            let e = |tick| {
                let mut e = evaluation(asset, dec!(0.97), tick);
                let delta = chrono::TimeDelta::minutes(windows * 5);
                e.evaluated_at += delta;
                e.end_date += delta;
                e.window_start_ms += delta.num_milliseconds();
                e.model_observed_at_ms = e
                    .model_observed_at_ms
                    .map(|ms| ms + delta.num_milliseconds());
                e.slug = format!("{asset}-updown-5m-{}", e.window_start_ms / 1000);
                e
            };
            assert!(s.observe(&e(0)).unwrap().is_empty());
            let e = e(1);
            let ids = s.observe(&e).unwrap();
            assert_eq!(ids.len(), 1);
            assert_eq!(
                s.state.lock().unwrap().positions[&ids[0]]
                    .attempt
                    .budget_limit,
                Some(expected)
            );
            (ids, e)
        };
        let (btc, b) = attempt(&s, "btc", 0, dec!(5));
        finish(&s, &btc, true);
        let first_loss = b.end_date + chrono::TimeDelta::seconds(17);
        let loss = (dec!(0), dec!(1));
        assert_eq!(s.resolve(&b.slug, first_loss, true, &loss).unwrap(), 1);
        let (eth, e) = attempt(&s, "eth", 1, dec!(5));
        finish(&s, &eth, false);
        assert_eq!(s.resolve(&e.slug, e.end_date, true, &loss).unwrap(), 1);
        let (btc, b) = attempt(&s, "btc", 1, dec!(50));
        finish(&s, &btc, true);
        let second_loss = b.end_date + chrono::TimeDelta::seconds(9);
        assert_eq!(s.resolve(&b.slug, second_loss, true, &loss).unwrap(), 1);
        assert_eq!(
            s.resolve(
                &b.slug,
                second_loss + chrono::TimeDelta::minutes(1),
                true,
                &loss
            )
            .unwrap(),
            0
        );
        let (eth, e) = attempt(&s, "eth", 2, dec!(5));
        finish(&s, &eth, true);
        let eth_loss = e.end_date + chrono::TimeDelta::seconds(11);
        s.resolve(&e.slug, eth_loss, true, &loss).unwrap();
        drop(s);

        let restart = eth_loss + chrono::TimeDelta::minutes(1);
        let s = PriceCapStudy::open(&p, 0, c, "boost-restart", restart).unwrap();
        let state = s.state.lock().unwrap();
        let boost = state.boost.as_ref().unwrap();
        let hour = chrono::TimeDelta::hours(1);
        let tick = chrono::TimeDelta::milliseconds(1);
        assert_eq!(boost.budget(0, first_loss - tick), dec!(5));
        assert_eq!(boost.budget(0, first_loss + hour), dec!(50));
        assert_eq!(boost.budget(0, second_loss + hour - tick), dec!(50));
        assert_eq!(boost.budget(0, second_loss + hour), dec!(5));
        assert_eq!(boost.budget(1, second_loss + hour), dec!(50));
        assert_eq!(boost.budget(1, eth_loss + hour), dec!(5));
        let mut ledger = crate::winner_dashboard::ledger::Ledger::open(&p).unwrap();
        ledger.refresh().unwrap();
        let v = ledger.view(now().date_naive(), 3, None, 0, 20).unwrap();
        assert_eq!(v.total[2].losses, 3);
        assert_eq!(v.total[2].no_fills, 1);
        assert_eq!(
            v.boost.unwrap().active_until(1, restart),
            Some(eth_loss + hour)
        );
        assert_eq!(v.capital + v.total[2].pnl, state.cash[1]);
        drop(state);
        drop(s);
        std::fs::remove_file(p).unwrap();
    }
}
