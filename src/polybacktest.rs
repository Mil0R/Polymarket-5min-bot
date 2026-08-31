//! Deterministic stage-1 replay against the SQLite database produced by
//! `polybacktest_downloader`. No network client or signer is reachable here.

use crate::late_resolution_winner_model::{
    ChainlinkTwapStore, TwapSample, WinnerModelInput, WinnerModelPolicy,
};
use crate::late_resolution_winner_trading::detect_winner_asks;
use anyhow::{bail, Context, Result};
use chrono::Utc;
use rusqlite::{Connection, Row};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Instant;

type AskLevels = Vec<(Decimal, Decimal)>;
type BookCache = HashMap<(usize, bool, bool), Option<AskLevels>>;

#[derive(Debug, Clone)]
pub struct ReplayConfig {
    database_file: PathBuf,
    market_type: String,
    output_dir: PathBuf,
    order_usdc: Decimal,
    execution_delay_ms: i64,
    liquidity_haircut: Decimal,
    fee_rate: Decimal,
    fee_exponent: u64,
    spread_min_edge: Decimal,
    spread_min_net_profit: Decimal,
    spread_execution_spread: Decimal,
    spread_fee_buffer: Decimal,
    spread_dynamic_fee_gate_paper: bool,
    spread_min_ask_depth_usdc: Decimal,
    spread_max_slippage_per_share: Decimal,
    spread_backtest_order_sizes_usdc: Vec<Decimal>,
    spread_backtest_confirmation_ms: Vec<i64>,
    spread_backtest_preflight_delays_ms: Vec<i64>,
    spread_backtest_second_leg_delays_ms: Vec<i64>,
    spread_backtest_liquidity_haircuts: Vec<Decimal>,
    spread_backtest_second_leg_depth_multipliers: Vec<Decimal>,
    spread_backtest_emergency_unwind_delays_ms: Vec<i64>,
    spread_max_one_sided_rate: Decimal,
    winner_before_end_secs: i64,
    winner_min_price: Decimal,
    winner_max_price: Decimal,
    winner_confirmations: u32,
    winner_policy: WinnerModelPolicy,
    winner_btc_min_volatility_bps: f64,
    winner_eth_min_volatility_bps: f64,
    winner_other_min_volatility_bps: f64,
    winner_btc_model_risk_buffer: Decimal,
    winner_eth_model_risk_buffer: Decimal,
    winner_other_model_risk_buffer: Decimal,
    winner_backtest_delays_ms: Vec<i64>,
    winner_backtest_liquidity_haircuts: Vec<Decimal>,
    progress_every_markets: usize,
    market_offset: usize,
    max_markets: usize,
}

impl ReplayConfig {
    pub fn from_env() -> Result<Self> {
        let _ = dotenvy::dotenv();
        let data_dir = std::env::var("POLYBACKTEST_DATA_DIR")
            .unwrap_or_else(|_| "data/polybacktest".to_string());
        let database_file = std::env::var_os("BACKTEST_DATABASE_FILE")
            .or_else(|| std::env::var_os("POLYBACKTEST_DATABASE_FILE"))
            .map(PathBuf::from)
            .unwrap_or_else(|| Path::new(&data_dir).join("polybacktest.sqlite3"));
        let fee_rate = env_decimal("BACKTEST_FEE_RATE", "0.25")?;
        let fee_exponent = env_parse("BACKTEST_FEE_EXPONENT", 2u64)?;
        let winner_fee = dynamic_fee_per_share(fee_rate, fee_exponent, dec!(0.97));
        let market_type = std::env::var("BACKTEST_MARKET_TYPE")
            .or_else(|_| std::env::var("POLYBACKTEST_MARKET_TYPE"))
            .unwrap_or_else(|_| "5m".to_string())
            .trim()
            .to_ascii_lowercase();
        Ok(Self {
            database_file,
            market_type,
            output_dir: std::env::var("BACKTEST_OUTPUT_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("results/backtests")),
            order_usdc: env_decimal("BACKTEST_ORDER_USDC", "5")?,
            execution_delay_ms: env_parse("BACKTEST_EXECUTION_DELAY_MS", 250i64)?,
            liquidity_haircut: env_decimal("BACKTEST_LIQUIDITY_HAIRCUT", "0.75")?,
            fee_rate,
            fee_exponent,
            spread_min_edge: env_decimal("BINARY_COMPLEMENT_ARBITRAGE_BACKTEST_MIN_EDGE", "0.005")?,
            spread_min_net_profit: env_decimal(
                "BINARY_COMPLEMENT_ARBITRAGE_BACKTEST_MIN_NET_PROFIT_USDC",
                "0.01",
            )?,
            spread_execution_spread: env_decimal(
                "BINARY_COMPLEMENT_ARBITRAGE_EXECUTION_SPREAD",
                "0.03",
            )?,
            spread_fee_buffer: env_decimal("BINARY_COMPLEMENT_ARBITRAGE_FEE_BUFFER", "0.002")?,
            spread_dynamic_fee_gate_paper: env_bool(
                "BINARY_COMPLEMENT_ARBITRAGE_DYNAMIC_FEE_GATE_PAPER",
                false,
            )?,
            spread_min_ask_depth_usdc: env_decimal(
                "BINARY_COMPLEMENT_ARBITRAGE_MIN_ASK_DEPTH_USDC",
                "10",
            )?,
            spread_max_slippage_per_share: env_decimal(
                "BINARY_COMPLEMENT_ARBITRAGE_BACKTEST_MAX_SLIPPAGE_PER_SHARE",
                "0.01",
            )?,
            spread_backtest_order_sizes_usdc: env_decimal_list(
                "BINARY_COMPLEMENT_ARBITRAGE_BACKTEST_ORDER_SIZES_USDC",
                &[dec!(2.5), dec!(5), dec!(10)],
            )?,
            spread_backtest_confirmation_ms: env_i64_list(
                "BINARY_COMPLEMENT_ARBITRAGE_BACKTEST_CONFIRMATION_MS",
                &[100, 250, 500, 1_000],
            )?,
            spread_backtest_preflight_delays_ms: env_i64_list(
                "BINARY_COMPLEMENT_ARBITRAGE_BACKTEST_PREFLIGHT_DELAYS_MS",
                &[0, 50, 100, 250],
            )?,
            spread_backtest_second_leg_delays_ms: env_i64_list(
                "BINARY_COMPLEMENT_ARBITRAGE_BACKTEST_SECOND_LEG_DELAYS_MS",
                &[0, 25, 50, 100, 250, 500],
            )?,
            spread_backtest_liquidity_haircuts: env_decimal_list(
                "BINARY_COMPLEMENT_ARBITRAGE_BACKTEST_LIQUIDITY_HAIRCUTS",
                &[dec!(0.50), dec!(0.75), dec!(1.0)],
            )?,
            spread_backtest_second_leg_depth_multipliers: env_decimal_list(
                "BINARY_COMPLEMENT_ARBITRAGE_BACKTEST_SECOND_LEG_DEPTH_MULTIPLIERS",
                &[dec!(1), dec!(2), dec!(3), dec!(5)],
            )?,
            spread_backtest_emergency_unwind_delays_ms: env_i64_list(
                "BINARY_COMPLEMENT_ARBITRAGE_BACKTEST_EMERGENCY_UNWIND_DELAYS_MS",
                &[0, 25, 50, 100],
            )?,
            spread_max_one_sided_rate: env_decimal(
                "BINARY_COMPLEMENT_ARBITRAGE_BACKTEST_MAX_ONE_SIDED_RATE",
                "0.01",
            )?,
            winner_before_end_secs: env_parse(
                "LATE_RESOLUTION_WINNER_TRADING_BEFORE_END_SECS",
                30i64,
            )?,
            winner_min_price: env_decimal("LATE_RESOLUTION_WINNER_TRADING_MIN_PRICE", "0.95")?,
            winner_max_price: env_decimal("LATE_RESOLUTION_WINNER_TRADING_MAX_PRICE", "0.98")?,
            winner_confirmations: env_parse(
                "LATE_RESOLUTION_WINNER_TRADING_CONFIRMATION_OBSERVATIONS",
                3u32,
            )?,
            winner_policy: WinnerModelPolicy {
                max_feed_age_ms: env_parse(
                    "LATE_RESOLUTION_WINNER_TRADING_FEED_MAX_AGE_MS",
                    5000i64,
                )?,
                max_opening_skew_ms: env_parse(
                    "LATE_RESOLUTION_WINNER_TRADING_OPENING_MAX_SKEW_MS",
                    5000i64,
                )?,
                volatility_lookback_secs: env_parse(
                    "LATE_RESOLUTION_WINNER_TRADING_VOLATILITY_LOOKBACK_SECS",
                    120u64,
                )?,
                min_observations: env_parse(
                    "LATE_RESOLUTION_WINNER_TRADING_MIN_OBSERVATIONS",
                    10usize,
                )?,
                min_volatility_bps_per_sqrt_second: env_parse(
                    "LATE_RESOLUTION_WINNER_TRADING_BTC_MIN_VOLATILITY_BPS",
                    0.8f64,
                )?,
                min_margin_bps: env_parse("LATE_RESOLUTION_WINNER_TRADING_MIN_MARGIN_BPS", 3.0f64)?,
                reversal_sigma_multiplier: env_parse(
                    "LATE_RESOLUTION_WINNER_TRADING_REVERSAL_SIGMA",
                    2.0f64,
                )?,
                max_spread: env_decimal("LATE_RESOLUTION_WINNER_TRADING_MAX_SPREAD", "0.03")?,
                min_depth_usdc: env_decimal("LATE_RESOLUTION_WINNER_TRADING_MIN_DEPTH_USDC", "10")?,
                fee_buffer: winner_fee,
                model_risk_buffer: env_decimal(
                    "LATE_RESOLUTION_WINNER_TRADING_BTC_MODEL_RISK_BUFFER",
                    "0.01",
                )?,
                min_net_ev: env_decimal("LATE_RESOLUTION_WINNER_TRADING_MIN_NET_EV", "0.002")?,
            },
            winner_btc_min_volatility_bps: env_parse(
                "LATE_RESOLUTION_WINNER_TRADING_BTC_MIN_VOLATILITY_BPS",
                0.8f64,
            )?,
            winner_eth_min_volatility_bps: env_parse(
                "LATE_RESOLUTION_WINNER_TRADING_ETH_MIN_VOLATILITY_BPS",
                1.2f64,
            )?,
            winner_other_min_volatility_bps: env_parse(
                "LATE_RESOLUTION_WINNER_TRADING_OTHER_MIN_VOLATILITY_BPS",
                1.5f64,
            )?,
            winner_btc_model_risk_buffer: env_decimal(
                "LATE_RESOLUTION_WINNER_TRADING_BTC_MODEL_RISK_BUFFER",
                "0.01",
            )?,
            winner_eth_model_risk_buffer: env_decimal(
                "LATE_RESOLUTION_WINNER_TRADING_ETH_MODEL_RISK_BUFFER",
                "0.015",
            )?,
            winner_other_model_risk_buffer: env_decimal(
                "LATE_RESOLUTION_WINNER_TRADING_OTHER_MODEL_RISK_BUFFER",
                "0.02",
            )?,
            winner_backtest_delays_ms: env_i64_list(
                "LATE_RESOLUTION_WINNER_TRADING_BACKTEST_DELAYS_MS",
                &[100, 250, 500, 1_000],
            )?,
            winner_backtest_liquidity_haircuts: env_decimal_list(
                "LATE_RESOLUTION_WINNER_TRADING_BACKTEST_LIQUIDITY_HAIRCUTS",
                &[dec!(0.50), dec!(0.75), dec!(1.0)],
            )?,
            progress_every_markets: env_parse("BACKTEST_PROGRESS_EVERY_MARKETS", 100usize)?,
            market_offset: env_parse("BACKTEST_MARKET_OFFSET", 0usize)?,
            max_markets: env_parse("BACKTEST_MAX_MARKETS", 0usize)?,
        })
    }

    fn validate(&self) -> Result<()> {
        if !self.database_file.is_file() {
            bail!(
                "PolyBacktest database not found: {}; run polybacktest_downloader sync first",
                self.database_file.display()
            );
        }
        if !matches!(self.market_type.as_str(), "5m" | "15m") {
            bail!("BACKTEST_MARKET_TYPE must be 5m or 15m for these strategy replays");
        }
        let market_window_secs = if self.market_type == "5m" { 300 } else { 900 };
        if self.winner_before_end_secs <= 0 || self.winner_before_end_secs > market_window_secs {
            bail!(
                "LATE_RESOLUTION_WINNER_TRADING_BEFORE_END_SECS must be in [1, {market_window_secs}] for BACKTEST_MARKET_TYPE={}",
                self.market_type
            );
        }
        if self.order_usdc <= Decimal::ZERO {
            bail!("BACKTEST_ORDER_USDC must be greater than zero");
        }
        if self.execution_delay_ms < 0 {
            bail!("BACKTEST_EXECUTION_DELAY_MS cannot be negative");
        }
        if self.spread_min_edge < Decimal::ZERO
            || self.spread_min_net_profit < Decimal::ZERO
            || self.spread_execution_spread < Decimal::ZERO
            || self.spread_fee_buffer < Decimal::ZERO
            || self.spread_min_ask_depth_usdc < Decimal::ZERO
            || self.spread_max_slippage_per_share < Decimal::ZERO
        {
            bail!("Binary Complement Arbitrage backtest thresholds cannot be negative");
        }
        if self.spread_backtest_order_sizes_usdc.is_empty()
            || self
                .spread_backtest_order_sizes_usdc
                .iter()
                .any(|size| *size <= Decimal::ZERO)
        {
            bail!("BINARY_COMPLEMENT_ARBITRAGE_BACKTEST_ORDER_SIZES_USDC must contain positive values");
        }
        validate_non_negative_i64_list(
            "BINARY_COMPLEMENT_ARBITRAGE_BACKTEST_CONFIRMATION_MS",
            &self.spread_backtest_confirmation_ms,
        )?;
        validate_non_negative_i64_list(
            "BINARY_COMPLEMENT_ARBITRAGE_BACKTEST_PREFLIGHT_DELAYS_MS",
            &self.spread_backtest_preflight_delays_ms,
        )?;
        if self.spread_backtest_second_leg_delays_ms.is_empty()
            || self
                .spread_backtest_second_leg_delays_ms
                .iter()
                .any(|delay| *delay < 0)
        {
            bail!(
                "BINARY_COMPLEMENT_ARBITRAGE_BACKTEST_SECOND_LEG_DELAYS_MS must contain non-negative values"
            );
        }
        if self.spread_backtest_liquidity_haircuts.is_empty()
            || self
                .spread_backtest_liquidity_haircuts
                .iter()
                .any(|haircut| *haircut <= Decimal::ZERO || *haircut > Decimal::ONE)
        {
            bail!("BINARY_COMPLEMENT_ARBITRAGE_BACKTEST_LIQUIDITY_HAIRCUTS must be in (0,1]");
        }
        if self.spread_backtest_second_leg_depth_multipliers.is_empty()
            || self
                .spread_backtest_second_leg_depth_multipliers
                .iter()
                .any(|multiplier| *multiplier < Decimal::ONE)
        {
            bail!("BINARY_COMPLEMENT_ARBITRAGE_BACKTEST_SECOND_LEG_DEPTH_MULTIPLIERS must be >= 1");
        }
        validate_non_negative_i64_list(
            "BINARY_COMPLEMENT_ARBITRAGE_BACKTEST_EMERGENCY_UNWIND_DELAYS_MS",
            &self.spread_backtest_emergency_unwind_delays_ms,
        )?;
        if self.spread_max_one_sided_rate < Decimal::ZERO
            || self.spread_max_one_sided_rate > Decimal::ONE
        {
            bail!("BINARY_COMPLEMENT_ARBITRAGE_BACKTEST_MAX_ONE_SIDED_RATE must be in [0,1]");
        }
        if self.liquidity_haircut <= Decimal::ZERO || self.liquidity_haircut > Decimal::ONE {
            bail!("BACKTEST_LIQUIDITY_HAIRCUT must be in (0,1]");
        }
        if self.winner_confirmations == 0 {
            bail!("LATE_RESOLUTION_WINNER_TRADING_CONFIRMATION_OBSERVATIONS must be greater than zero");
        }
        if self.winner_backtest_delays_ms.is_empty()
            || self
                .winner_backtest_delays_ms
                .iter()
                .any(|delay| *delay < 0)
        {
            bail!("LATE_RESOLUTION_WINNER_TRADING_BACKTEST_DELAYS_MS must contain non-negative values");
        }
        if self.winner_backtest_liquidity_haircuts.is_empty()
            || self
                .winner_backtest_liquidity_haircuts
                .iter()
                .any(|haircut| *haircut <= Decimal::ZERO || *haircut > Decimal::ONE)
        {
            bail!("LATE_RESOLUTION_WINNER_TRADING_BACKTEST_LIQUIDITY_HAIRCUTS must be in (0,1]");
        }
        if self.progress_every_markets == 0 {
            bail!("BACKTEST_PROGRESS_EVERY_MARKETS must be greater than zero");
        }
        Ok(())
    }

    fn winner_policy_for_asset(&self, asset: &str) -> WinnerModelPolicy {
        let mut policy = self.winner_policy;
        match winner_asset_bucket(asset) {
            "btc" => {
                policy.min_volatility_bps_per_sqrt_second = self.winner_btc_min_volatility_bps;
                policy.model_risk_buffer = self.winner_btc_model_risk_buffer;
            }
            "eth" => {
                policy.min_volatility_bps_per_sqrt_second = self.winner_eth_min_volatility_bps;
                policy.model_risk_buffer = self.winner_eth_model_risk_buffer;
            }
            _ => {
                policy.min_volatility_bps_per_sqrt_second = self.winner_other_min_volatility_bps;
                policy.model_risk_buffer = self.winner_other_model_risk_buffer;
            }
        }
        policy
    }
}

fn winner_asset_bucket(asset: &str) -> &'static str {
    match asset.trim().to_ascii_lowercase().as_str() {
        "btc" | "bitcoin" => "btc",
        "eth" | "ethereum" => "eth",
        _ => "other",
    }
}

#[derive(Debug, Clone)]
struct Market {
    id: String,
    coin: String,
    start_ms: i64,
    end_ms: i64,
    winner: Option<String>,
}

#[derive(Debug, Clone)]
struct Snapshot {
    time_ms: i64,
    twap: Option<Decimal>,
    twap_observed_at_ms: Option<i64>,
    twap_source: Option<String>,
    up_bid: Option<Decimal>,
    up_ask: Option<Decimal>,
    up_ask_size: Option<Decimal>,
    down_bid: Option<Decimal>,
    down_ask: Option<Decimal>,
    down_ask_size: Option<Decimal>,
    up_book: Option<Vec<u8>>,
    down_book: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Default)]
struct Totals {
    markets: usize,
    snapshots: usize,
    candidates: usize,
    admitted: usize,
    fills: usize,
    wins: usize,
    losses: usize,
    one_sided_fills: usize,
    one_sided_wins: usize,
    one_sided_losses: usize,
    emergency_unwinds: usize,
    unrecovered_one_sided: usize,
    rejections: BTreeMap<String, usize>,
    gross_pnl: Decimal,
    fees: Decimal,
    paired_gross_pnl: Decimal,
    paired_fees: Decimal,
    one_sided_gross_pnl: Decimal,
    one_sided_fees: Decimal,
    emergency_unwind_gross_pnl: Decimal,
    emergency_unwind_fees: Decimal,
}

#[derive(Debug, Clone, Copy, Serialize)]
struct WinnerScenario {
    execution_delay_ms: i64,
    liquidity_haircut: Decimal,
}

impl WinnerScenario {
    fn id(self) -> String {
        format!(
            "delay-{}ms_haircut-{}",
            self.execution_delay_ms, self.liquidity_haircut
        )
    }
}

#[derive(Debug)]
struct WinnerScenarioTotals {
    scenario: WinnerScenario,
    assets: BTreeMap<String, Totals>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
struct SpreadScenario {
    order_usdc: Decimal,
    confirmation_ms: i64,
    preflight_delay_ms: i64,
    second_leg_delay_ms: i64,
    liquidity_haircut: Decimal,
    second_leg_depth_multiplier: Decimal,
    emergency_unwind_delay_ms: i64,
}

impl SpreadScenario {
    fn id(self) -> String {
        format!(
            "order-{}usdc_confirm-{}ms_preflight-{}ms_second-{}ms_haircut-{}_coverage-{}x_unwind-{}ms",
            self.order_usdc,
            self.confirmation_ms,
            self.preflight_delay_ms,
            self.second_leg_delay_ms,
            self.liquidity_haircut,
            self.second_leg_depth_multiplier,
            self.emergency_unwind_delay_ms,
        )
    }
}

#[derive(Debug)]
struct SpreadScenarioTotals {
    scenario: SpreadScenario,
    assets: BTreeMap<String, Totals>,
}

#[derive(Debug, Serialize)]
struct WinnerMetricsReport {
    markets: usize,
    snapshots: usize,
    candidates: usize,
    admitted: usize,
    fills: usize,
    wins: usize,
    losses: usize,
    one_sided_fills: usize,
    one_sided_wins: usize,
    one_sided_losses: usize,
    /// All second-leg failures, including positions recovered by emergency unwind.
    one_sided_rate_pct: String,
    emergency_unwinds: usize,
    unrecovered_one_sided: usize,
    unrecovered_one_sided_rate_pct: String,
    emergency_unwind_net_pnl_usdc: String,
    win_rate_pct: String,
    rejections: BTreeMap<String, usize>,
    gross_pnl_usdc: String,
    fees_usdc: String,
    net_pnl_usdc: String,
    paired_net_pnl_usdc: String,
    one_sided_net_pnl_usdc: String,
}

#[derive(Debug, Serialize)]
struct WinnerScenarioReport {
    id: String,
    execution_delay_ms: i64,
    liquidity_haircut: String,
    overall: WinnerMetricsReport,
    assets: BTreeMap<String, WinnerMetricsReport>,
}

#[derive(Debug, Serialize)]
struct WinnerReplayReport {
    strategy: &'static str,
    generated_at: String,
    database: String,
    market_type: String,
    order_usdc: String,
    fee_rate: String,
    fee_exponent: u64,
    parameters: Value,
    scenarios: Vec<WinnerScenarioReport>,
    positive_scenarios: usize,
    readiness: &'static str,
    report_file: String,
    log_file: String,
    analysis_file: String,
    limitations: Vec<&'static str>,
}

#[derive(Debug, Serialize)]
struct SpreadScenarioReport {
    id: String,
    order_usdc: String,
    confirmation_ms: i64,
    preflight_delay_ms: i64,
    second_leg_delay_ms: i64,
    liquidity_haircut: String,
    second_leg_depth_multiplier: String,
    emergency_unwind_delay_ms: i64,
    overall: WinnerMetricsReport,
    assets: BTreeMap<String, WinnerMetricsReport>,
}

#[derive(Debug, Serialize)]
struct SpreadReplayReport {
    strategy: &'static str,
    generated_at: String,
    database: String,
    market_type: String,
    fee_rate: String,
    fee_exponent: u64,
    parameters: Value,
    scenarios: Vec<SpreadScenarioReport>,
    positive_scenarios: usize,
    readiness: &'static str,
    report_file: String,
    log_file: String,
    analysis_file: String,
    limitations: Vec<&'static str>,
}

struct ReplayRunLog {
    writer: BufWriter<File>,
}

impl ReplayRunLog {
    fn create(path: &Path) -> Result<Self> {
        Ok(Self {
            writer: BufWriter::new(
                File::create(path)
                    .with_context(|| format!("failed to create {}", path.display()))?,
            ),
        })
    }

    fn line(&mut self, message: impl AsRef<str>) -> Result<()> {
        let line = format!(
            "{} {}",
            Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ"),
            message.as_ref()
        );
        println!("{line}");
        writeln!(self.writer, "{line}")?;
        self.writer.flush()?;
        Ok(())
    }
}

pub fn run_binary_complement_replay() -> Result<PathBuf> {
    let config = ReplayConfig::from_env()?;
    config.validate()?;
    let connection = open_database(&config)?;
    let markets = load_markets(&connection, &config.market_type)?;
    if markets.is_empty() {
        bail!(
            "no complete {} markets found in {}; download that period before backtesting",
            config.market_type,
            config.database_file.display()
        );
    }
    let mut markets = markets
        .into_iter()
        .skip(config.market_offset)
        .collect::<Vec<_>>();
    if config.max_markets > 0 {
        markets.truncate(config.max_markets);
    }
    let run_id = format!(
        "run-{}-{}",
        Utc::now().format("%Y%m%dT%H%M%SZ"),
        uuid::Uuid::new_v4()
    );
    let run_dir = config
        .output_dir
        .join("binary-complement-arbitrage")
        .join(&config.market_type)
        .join(run_id);
    fs::create_dir_all(&run_dir)?;
    let report_path = run_dir.join("report.json");
    let log_path = run_dir.join("run.log");
    let analysis_path = run_dir.join("analysis.zh-CN.md");
    let mut logger = ReplayRunLog::create(&log_path)?;
    let scenarios = spread_scenarios(&config);
    let mut scenario_totals = scenarios
        .iter()
        .copied()
        .map(|scenario| SpreadScenarioTotals {
            scenario,
            assets: BTreeMap::new(),
        })
        .collect::<Vec<_>>();
    logger.line(format!(
        "INFO strategy=binary-complement-arbitrage phase=started market_type={} markets={} scenarios={} database={}",
        config.market_type,
        markets.len(),
        scenarios.len(),
        config.database_file.display()
    ))?;
    let started = Instant::now();
    for (index, market) in markets.iter().enumerate() {
        let snapshots = load_snapshots(&connection, &market.id)?;
        for totals in &mut scenario_totals {
            let asset = totals.assets.entry(market.coin.clone()).or_default();
            asset.markets += 1;
            asset.snapshots += snapshots.len();
        }
        replay_spread_market_grid(&config, market, &snapshots, &mut scenario_totals)?;
        let completed = index + 1;
        if completed == markets.len() || completed % config.progress_every_markets == 0 {
            let baseline = scenario_totals
                .first()
                .map(aggregate_spread_scenario_totals)
                .unwrap_or_default();
            logger.line(format!(
                "INFO strategy=binary-complement-arbitrage market_progress={}/{} ({:.1}%) elapsed_secs={} baseline_candidates={} baseline_admitted={} baseline_fills={}",
                completed,
                markets.len(),
                if markets.is_empty() { 100.0 } else { completed as f64 / markets.len() as f64 * 100.0 },
                started.elapsed().as_secs(),
                baseline.candidates,
                baseline.admitted,
                baseline.fills,
            ))?;
        }
    }
    let scenario_reports = scenario_totals
        .iter()
        .map(spread_scenario_report)
        .collect::<Vec<_>>();
    let positive_scenarios = scenario_reports
        .iter()
        .filter(|scenario| decimal_string(&scenario.overall.net_pnl_usdc) > Decimal::ZERO)
        .count();
    let minimum_fills = scenario_reports
        .iter()
        .map(|scenario| scenario.overall.fills)
        .min()
        .unwrap_or_default();
    let has_optimistic_research_sample = scenario_reports.iter().any(|scenario| {
        scenario.preflight_delay_ms == 0
            && scenario.overall.fills >= 100
            && decimal_string(&scenario.overall.paired_net_pnl_usdc) > Decimal::ZERO
    });
    let has_latency_resolved_sample = scenario_reports.iter().any(|scenario| {
        scenario.preflight_delay_ms > 0
            && scenario.overall.fills >= 100
            && decimal_string(&scenario.overall.paired_net_pnl_usdc) > Decimal::ZERO
    });
    let readiness = if scenario_reports
        .iter()
        .all(|scenario| scenario.overall.fills + scenario.overall.one_sided_fills == 0)
    {
        "NOT_READY_NO_FILLS"
    } else if scenario_reports.iter().any(|scenario| {
        decimal_string(&scenario.overall.unrecovered_one_sided_rate_pct)
            > config.spread_max_one_sided_rate * dec!(100)
    }) {
        "NOT_READY_ONE_SIDED_RISK"
    } else if has_optimistic_research_sample && !has_latency_resolved_sample {
        "READY_FOR_PAPER_DRY_RUN_LATENCY_CALIBRATION"
    } else if minimum_fills < 100 {
        "NOT_READY_INSUFFICIENT_SAMPLE"
    } else if positive_scenarios < scenario_reports.len() {
        "NOT_READY_EXECUTION_SENSITIVE"
    } else {
        "READY_FOR_DRY_RUN_REVIEW"
    };
    let report = SpreadReplayReport {
        strategy: "binary-complement-arbitrage",
        generated_at: Utc::now().to_rfc3339(),
        database: config.database_file.display().to_string(),
        market_type: config.market_type.clone(),
        fee_rate: config.fee_rate.to_string(),
        fee_exponent: config.fee_exponent,
        parameters: serde_json::json!({
            "minimum_signal_edge_per_share": config.spread_min_edge,
            "minimum_net_profit_usdc": config.spread_min_net_profit,
            "execution_spread": config.spread_execution_spread,
            "fee_buffer_per_share": config.spread_fee_buffer,
            "dynamic_fee_gate_paper": config.spread_dynamic_fee_gate_paper,
            "minimum_signal_ask_depth_usdc": config.spread_min_ask_depth_usdc,
            "maximum_slippage_per_share": config.spread_max_slippage_per_share,
            "maximum_unrecovered_one_sided_rate": config.spread_max_one_sided_rate,
            "scenario_generation": "baseline_plus_one_factor_and_two_composite_profiles",
        }),
        scenarios: scenario_reports,
        positive_scenarios,
        readiness,
        report_file: report_path.display().to_string(),
        log_file: log_path.display().to_string(),
        analysis_file: analysis_path.display().to_string(),
        limitations: vec![
            "Fee rate/exponent are configured assumptions because the local dataset does not carry historical per-market fee metadata.",
            "Replay uses the first snapshot at or after the configured latency and does not infer fills between snapshots.",
            "Historical snapshots cannot reproduce queue priority, API failures, partial two-leg fills, or venue tick-size changes.",
        ],
    };
    write_json_file(&report_path, &report)?;
    write_spread_analysis(&analysis_path, &report)?;
    for scenario in &report.scenarios {
        logger.line(format!(
            "INFO strategy=binary-complement-arbitrage scenario={} order_usdc={} confirmation_ms={} preflight_delay_ms={} second_leg_delay_ms={} liquidity_haircut={} coverage={} unwind_delay_ms={} candidates={} admitted={} fills={} one_sided={} emergency_unwinds={} net_pnl_usdc={}",
            scenario.id,
            scenario.order_usdc,
            scenario.confirmation_ms,
            scenario.preflight_delay_ms,
            scenario.second_leg_delay_ms,
            scenario.liquidity_haircut,
            scenario.second_leg_depth_multiplier,
            scenario.emergency_unwind_delay_ms,
            scenario.overall.candidates,
            scenario.overall.admitted,
            scenario.overall.fills,
            scenario.overall.one_sided_fills,
            scenario.overall.emergency_unwinds,
            scenario.overall.net_pnl_usdc,
        ))?;
    }
    logger.line(format!(
        "INFO strategy=binary-complement-arbitrage phase=completed elapsed_secs={} report={} analysis={} log={}",
        started.elapsed().as_secs(),
        report_path.display(),
        analysis_path.display(),
        log_path.display(),
    ))?;
    Ok(report_path)
}

pub fn run_late_resolution_winner_replay() -> Result<PathBuf> {
    let config = ReplayConfig::from_env()?;
    config.validate()?;
    let connection = open_database(&config)?;
    let markets = load_markets(&connection, &config.market_type)?;
    if markets.is_empty() {
        bail!(
            "no complete {} markets found in {}; download that period before backtesting",
            config.market_type,
            config.database_file.display()
        );
    }
    let mut markets = markets
        .into_iter()
        .skip(config.market_offset)
        .collect::<Vec<_>>();
    if config.max_markets > 0 {
        markets.truncate(config.max_markets);
    }
    let run_id = format!(
        "run-{}-{}",
        Utc::now().format("%Y%m%dT%H%M%SZ"),
        uuid::Uuid::new_v4()
    );
    let run_dir = config
        .output_dir
        .join("late-resolution-winner-trading")
        .join(&config.market_type)
        .join(run_id);
    fs::create_dir_all(&run_dir)?;
    let report_path = run_dir.join("report.json");
    let log_path = run_dir.join("run.log");
    let analysis_path = run_dir.join("analysis.zh-CN.md");
    let mut logger = ReplayRunLog::create(&log_path)?;
    let scenarios = winner_scenarios(&config);
    let mut scenario_totals = scenarios
        .iter()
        .copied()
        .map(|scenario| WinnerScenarioTotals {
            scenario,
            assets: BTreeMap::new(),
        })
        .collect::<Vec<_>>();
    logger.line(format!(
        "INFO strategy=late-resolution-winner-trading phase=started market_type={} markets={} scenarios={} database={}",
        config.market_type,
        markets.len(),
        scenarios.len(),
        config.database_file.display()
    ))?;
    let started = Instant::now();
    for (index, market) in markets.iter().enumerate() {
        let snapshots = load_snapshots(&connection, &market.id)?;
        for totals in &mut scenario_totals {
            let asset = totals.assets.entry(market.coin.clone()).or_default();
            asset.markets += 1;
            asset.snapshots += snapshots.len();
        }
        replay_winner_market_grid(&config, market, &snapshots, &mut scenario_totals)?;
        let completed = index + 1;
        if completed == markets.len() || completed % config.progress_every_markets == 0 {
            let baseline = scenario_totals
                .first()
                .map(aggregate_scenario_totals)
                .unwrap_or_default();
            logger.line(format!(
                "INFO strategy=late-resolution-winner-trading market_progress={}/{} ({:.1}%) elapsed_secs={} baseline_candidates={} baseline_admitted={} baseline_fills={}",
                completed,
                markets.len(),
                if markets.is_empty() { 100.0 } else { completed as f64 / markets.len() as f64 * 100.0 },
                started.elapsed().as_secs(),
                baseline.candidates,
                baseline.admitted,
                baseline.fills,
            ))?;
        }
    }
    let scenario_reports = scenario_totals
        .iter()
        .map(winner_scenario_report)
        .collect::<Vec<_>>();
    let positive_scenarios = scenario_reports
        .iter()
        .filter(|scenario| decimal_string(&scenario.overall.net_pnl_usdc) > Decimal::ZERO)
        .count();
    let minimum_fills = scenario_reports
        .iter()
        .map(|scenario| scenario.overall.fills)
        .min()
        .unwrap_or_default();
    let readiness = if scenario_reports
        .iter()
        .all(|scenario| scenario.overall.fills == 0)
    {
        "NOT_READY_NO_FILLS"
    } else if minimum_fills < 100 {
        "NOT_READY_INSUFFICIENT_SAMPLE"
    } else if positive_scenarios < scenario_reports.len() {
        "NOT_READY_EXECUTION_SENSITIVE"
    } else {
        "READY_FOR_DRY_RUN_REVIEW"
    };
    let report = WinnerReplayReport {
        strategy: "late-resolution-winner-trading",
        generated_at: Utc::now().to_rfc3339(),
        database: config.database_file.display().to_string(),
        market_type: config.market_type.clone(),
        order_usdc: config.order_usdc.to_string(),
        fee_rate: config.fee_rate.to_string(),
        fee_exponent: config.fee_exponent,
        parameters: serde_json::json!({
            "before_end_secs": config.winner_before_end_secs,
            "price_range": [config.winner_min_price, config.winner_max_price],
            "confirmation_observations": config.winner_confirmations,
            "common": {
                "max_feed_age_ms": config.winner_policy.max_feed_age_ms,
                "max_opening_skew_ms": config.winner_policy.max_opening_skew_ms,
                "volatility_lookback_secs": config.winner_policy.volatility_lookback_secs,
                "min_observations": config.winner_policy.min_observations,
                "min_margin_bps": config.winner_policy.min_margin_bps,
                "reversal_sigma": config.winner_policy.reversal_sigma_multiplier,
                "max_spread": config.winner_policy.max_spread,
                "min_depth_usdc": config.winner_policy.min_depth_usdc,
                "min_net_ev": config.winner_policy.min_net_ev,
            },
            "assets": {
                "btc": {
                    "min_volatility_bps": config.winner_btc_min_volatility_bps,
                    "model_risk_buffer": config.winner_btc_model_risk_buffer,
                },
                "eth": {
                    "min_volatility_bps": config.winner_eth_min_volatility_bps,
                    "model_risk_buffer": config.winner_eth_model_risk_buffer,
                },
                "sol_other": {
                    "min_volatility_bps": config.winner_other_min_volatility_bps,
                    "model_risk_buffer": config.winner_other_model_risk_buffer,
                },
            },
        }),
        scenarios: scenario_reports,
        positive_scenarios,
        readiness,
        report_file: report_path.display().to_string(),
        log_file: log_path.display().to_string(),
        analysis_file: analysis_path.display().to_string(),
        limitations: vec![
            "Only snapshots whose TWAP source is chainlink_twap are model observations.",
            "Historical order-book snapshots cannot prove queue position or fills between snapshots.",
            "Fee rate/exponent are configured assumptions because historical fee metadata is absent.",
        ],
    };
    write_json_file(&report_path, &report)?;
    write_winner_analysis(&analysis_path, &report)?;
    for scenario in &report.scenarios {
        logger.line(format!(
            "INFO strategy=late-resolution-winner-trading scenario={} delay_ms={} liquidity_haircut={} candidates={} admitted={} fills={} wins={} losses={} net_pnl_usdc={}",
            scenario.id,
            scenario.execution_delay_ms,
            scenario.liquidity_haircut,
            scenario.overall.candidates,
            scenario.overall.admitted,
            scenario.overall.fills,
            scenario.overall.wins,
            scenario.overall.losses,
            scenario.overall.net_pnl_usdc,
        ))?;
    }
    logger.line(format!(
        "INFO strategy=late-resolution-winner-trading phase=completed elapsed_secs={} report={} analysis={} log={}",
        started.elapsed().as_secs(),
        report_path.display(),
        analysis_path.display(),
        log_path.display(),
    ))?;
    Ok(report_path)
}

fn open_database(config: &ReplayConfig) -> Result<Connection> {
    let connection = Connection::open_with_flags(
        &config.database_file,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version != 1 {
        bail!("unsupported PolyBacktest schema version {version}; expected 1");
    }
    Ok(connection)
}

fn load_markets(connection: &Connection, market_type: &str) -> Result<Vec<Market>> {
    let mut statement = connection.prepare(
        "SELECT market_id,coin,start_time,end_time,winner FROM markets
         WHERE status='complete' AND market_type=?1
           AND start_time IS NOT NULL AND end_time IS NOT NULL
         ORDER BY start_time,market_id",
    )?;
    let markets = statement
        .query_map([market_type], |row| {
            Ok(Market {
                id: row.get(0)?,
                coin: row.get(1)?,
                start_ms: normalize_ms(row.get(2)?),
                end_ms: normalize_ms(row.get(3)?),
                winner: row.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(markets)
}

fn load_snapshots(connection: &Connection, market_id: &str) -> Result<Vec<Snapshot>> {
    let mut statement = connection.prepare(
        "SELECT time,coin_twap,coin_twap_observed_at,coin_twap_source,
                up_best_bid,up_best_ask,up_best_ask_size,
                down_best_bid,down_best_ask,down_best_ask_size,
                orderbook_up_zstd,orderbook_down_zstd
         FROM snapshots WHERE market_id=?1 ORDER BY time,snapshot_id",
    )?;
    let snapshots = statement
        .query_map([market_id], snapshot_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(snapshots)
}

fn snapshot_from_row(row: &Row<'_>) -> rusqlite::Result<Snapshot> {
    Ok(Snapshot {
        time_ms: normalize_ms(row.get(0)?),
        twap: decimal_column(row, 1)?,
        twap_observed_at_ms: row.get::<_, Option<i64>>(2)?.map(normalize_ms),
        twap_source: row.get(3)?,
        up_bid: decimal_column(row, 4)?,
        up_ask: decimal_column(row, 5)?,
        up_ask_size: decimal_column(row, 6)?,
        down_bid: decimal_column(row, 7)?,
        down_ask: decimal_column(row, 8)?,
        down_ask_size: decimal_column(row, 9)?,
        up_book: row.get(10)?,
        down_book: row.get(11)?,
    })
}

fn replay_spread_market_grid(
    config: &ReplayConfig,
    market: &Market,
    snapshots: &[Snapshot],
    scenario_totals: &mut [SpreadScenarioTotals],
) -> Result<()> {
    let mut completed = vec![false; scenario_totals.len()];
    let mut confirmation_started_at = vec![None; scenario_totals.len()];
    let mut book_cache = BookCache::new();
    for (signal_index, signal) in snapshots.iter().enumerate() {
        let (Some(up_ask), Some(down_ask)) = (signal.up_ask, signal.down_ask) else {
            reset_active_confirmations(&mut confirmation_started_at, &completed);
            continue;
        };
        let signal_sum = up_ask + down_ask;
        if Decimal::ONE - signal_sum < config.spread_min_edge {
            reset_active_confirmations(&mut confirmation_started_at, &completed);
            continue;
        }
        for (index, scenario) in scenario_totals.iter_mut().enumerate() {
            if !completed[index] {
                spread_asset_totals(scenario, &market.coin).candidates += 1;
            }
        }
        let up_signal_depth = cached_asks(&mut book_cache, snapshots, signal_index, true)?
            .map(|asks| ask_depth_at_or_better(asks, up_ask));
        let down_signal_depth = cached_asks(&mut book_cache, snapshots, signal_index, false)?
            .map(|asks| ask_depth_at_or_better(asks, down_ask));
        let Some(up_signal_depth) = up_signal_depth else {
            reject_active_spread_scenarios(
                scenario_totals,
                &completed,
                &market.coin,
                "missing_up_signal_book",
            );
            reset_active_confirmations(&mut confirmation_started_at, &completed);
            continue;
        };
        let Some(down_signal_depth) = down_signal_depth else {
            reject_active_spread_scenarios(
                scenario_totals,
                &completed,
                &market.coin,
                "missing_down_signal_book",
            );
            reset_active_confirmations(&mut confirmation_started_at, &completed);
            continue;
        };
        if up_signal_depth < config.spread_min_ask_depth_usdc
            || down_signal_depth < config.spread_min_ask_depth_usdc
        {
            reject_active_spread_scenarios(
                scenario_totals,
                &completed,
                &market.coin,
                "signal_depth_below_threshold",
            );
            reset_active_confirmations(&mut confirmation_started_at, &completed);
            continue;
        }
        let signal_gross_edge = Decimal::ONE - signal_sum;
        let up_limit = (up_ask + config.spread_max_slippage_per_share).min(Decimal::ONE);
        let down_limit = (down_ask + config.spread_max_slippage_per_share).min(Decimal::ONE);
        if !config.spread_dynamic_fee_gate_paper
            && signal_gross_edge < config.spread_execution_spread
        {
            reject_active_spread_scenarios(
                scenario_totals,
                &completed,
                &market.coin,
                "signal_execution_spread_below_threshold",
            );
            reset_active_confirmations(&mut confirmation_started_at, &completed);
            continue;
        }
        let up_first = match signal
            .up_ask_size
            .unwrap_or_default()
            .cmp(&signal.down_ask_size.unwrap_or_default())
        {
            std::cmp::Ordering::Less => true,
            std::cmp::Ordering::Greater => false,
            std::cmp::Ordering::Equal => up_ask <= down_ask,
        };
        for (index, scenario_run) in scenario_totals.iter_mut().enumerate() {
            if completed[index] {
                continue;
            }
            let scenario = scenario_run.scenario;
            let totals = spread_asset_totals(scenario_run, &market.coin);
            let confirmation_start = confirmation_started_at[index].get_or_insert(signal.time_ms);
            if signal.time_ms.saturating_sub(*confirmation_start) < scenario.confirmation_ms {
                reject(totals, "signal_confirmation_pending");
                continue;
            }
            totals.admitted += 1;
            let Some(preflight_index) = snapshot_index_at_or_after(
                snapshots,
                signal.time_ms.saturating_add(scenario.preflight_delay_ms),
            ) else {
                reject(totals, "no_preflight_snapshot");
                completed[index] = true;
                continue;
            };
            let signal_fee_per_share =
                dynamic_fee_per_share(config.fee_rate, config.fee_exponent, up_ask)
                    + dynamic_fee_per_share(config.fee_rate, config.fee_exponent, down_ask);
            let mut filled_shares =
                (scenario.order_usdc / (signal_sum + signal_fee_per_share)).round_dp(6);
            if filled_shares <= Decimal::ZERO {
                reject(totals, "order_size_rounds_to_zero");
                completed[index] = true;
                continue;
            }
            let up_cost =
                cached_asks(&mut book_cache, snapshots, preflight_index, true)?.and_then(|asks| {
                    walk_ask_levels_up_to(asks, filled_shares, scenario.liquidity_haircut, up_limit)
                });
            let Some(mut up_cost) = up_cost else {
                reject(totals, "up_execution_depth_insufficient");
                continue;
            };
            let down_cost = cached_asks(&mut book_cache, snapshots, preflight_index, false)?
                .and_then(|asks| {
                    walk_ask_levels_up_to(
                        asks,
                        filled_shares,
                        scenario.liquidity_haircut,
                        down_limit,
                    )
                });
            let Some(mut down_cost) = down_cost else {
                reject(totals, "down_execution_depth_insufficient");
                continue;
            };
            let second_is_up = !up_first;
            let second_limit = if up_first { down_limit } else { up_limit };
            let coverage_target = filled_shares * scenario.second_leg_depth_multiplier;
            let second_coverage =
                cached_asks(&mut book_cache, snapshots, preflight_index, second_is_up)?.and_then(
                    |asks| {
                        walk_ask_levels_up_to(
                            asks,
                            coverage_target,
                            scenario.liquidity_haircut,
                            second_limit,
                        )
                    },
                );
            if second_coverage.is_none() {
                reject(totals, "second_leg_coverage_insufficient");
                continue;
            }
            let mut up_average = up_cost / filled_shares;
            let mut down_average = down_cost / filled_shares;
            let mut fees =
                (dynamic_fee_per_share(config.fee_rate, config.fee_exponent, up_average)
                    + dynamic_fee_per_share(config.fee_rate, config.fee_exponent, down_average))
                    * filled_shares;
            if up_cost + down_cost + fees > scenario.order_usdc {
                filled_shares = (filled_shares * scenario.order_usdc
                    / (up_cost + down_cost + fees)
                    * dec!(0.999))
                .round_dp(6);
                let resized_up_cost = cached_asks(
                    &mut book_cache,
                    snapshots,
                    preflight_index,
                    true,
                )?
                .and_then(|asks| {
                    walk_ask_levels_up_to(asks, filled_shares, scenario.liquidity_haircut, up_limit)
                });
                let resized_down_cost =
                    cached_asks(&mut book_cache, snapshots, preflight_index, false)?.and_then(
                        |asks| {
                            walk_ask_levels_up_to(
                                asks,
                                filled_shares,
                                scenario.liquidity_haircut,
                                down_limit,
                            )
                        },
                    );
                let (Some(resized_up_cost), Some(resized_down_cost)) =
                    (resized_up_cost, resized_down_cost)
                else {
                    reject(totals, "budget_resize_depth_insufficient");
                    continue;
                };
                up_cost = resized_up_cost;
                down_cost = resized_down_cost;
                up_average = up_cost / filled_shares;
                down_average = down_cost / filled_shares;
                fees = (dynamic_fee_per_share(config.fee_rate, config.fee_exponent, up_average)
                    + dynamic_fee_per_share(config.fee_rate, config.fee_exponent, down_average))
                    * filled_shares;
            }
            if up_cost + down_cost + fees > scenario.order_usdc
                || up_cost < dec!(1)
                || down_cost < dec!(1)
            {
                reject(totals, "budget_or_minimum_leg_notional");
                continue;
            }
            let gross_edge_per_share = Decimal::ONE - up_average - down_average;
            let fee_per_share = fees / filled_shares;
            let net_edge_per_share = gross_edge_per_share - fee_per_share;
            if !config.spread_dynamic_fee_gate_paper
                && gross_edge_per_share < config.spread_execution_spread
            {
                reject(totals, "execution_spread_below_threshold");
                continue;
            }
            if net_edge_per_share < config.spread_min_edge + config.spread_fee_buffer {
                reject(totals, "net_edge_below_threshold");
                continue;
            }
            let preflight_gross_pnl = filled_shares - up_cost - down_cost;
            if preflight_gross_pnl - fees < config.spread_min_net_profit {
                reject(totals, "net_profit_below_threshold");
                continue;
            }
            let first_cost = if up_first { up_cost } else { down_cost };
            let first_average = first_cost / filled_shares;
            let first_fees =
                dynamic_fee_per_share(config.fee_rate, config.fee_exponent, first_average)
                    * filled_shares;
            let second_target_ms = snapshots[preflight_index]
                .time_ms
                .saturating_add(scenario.second_leg_delay_ms);
            let second_index = snapshot_index_at_or_after(snapshots, second_target_ms);
            let second_cost = second_index
                .map(|second_index| {
                    cached_asks(&mut book_cache, snapshots, second_index, second_is_up).map(
                        |asks| {
                            asks.and_then(|asks| {
                                walk_ask_levels_up_to(
                                    asks,
                                    filled_shares,
                                    scenario.liquidity_haircut,
                                    second_limit,
                                )
                            })
                        },
                    )
                })
                .transpose()?
                .flatten();
            let Some(second_cost) = second_cost else {
                totals.one_sided_fills += 1;
                let unwind_reference_ms = second_index
                    .map(|snapshot_index| snapshots[snapshot_index].time_ms)
                    .unwrap_or(second_target_ms);
                let unwind_target_ms =
                    unwind_reference_ms.saturating_add(scenario.emergency_unwind_delay_ms);
                let unwind_proceeds = snapshot_index_at_or_after(snapshots, unwind_target_ms)
                    .map(|unwind_index| {
                        cached_bids(&mut book_cache, snapshots, unwind_index, up_first).map(
                            |bids| {
                                bids.and_then(|bids| {
                                    walk_bid_levels(bids, filled_shares, scenario.liquidity_haircut)
                                })
                            },
                        )
                    })
                    .transpose()?
                    .flatten();
                if let Some(unwind_proceeds) = unwind_proceeds {
                    let unwind_average = unwind_proceeds / filled_shares;
                    let unwind_fees =
                        dynamic_fee_per_share(config.fee_rate, config.fee_exponent, unwind_average)
                            * filled_shares;
                    let unwind_gross_pnl = unwind_proceeds - first_cost;
                    totals.emergency_unwinds += 1;
                    totals.gross_pnl += unwind_gross_pnl;
                    totals.fees += first_fees + unwind_fees;
                    totals.emergency_unwind_gross_pnl += unwind_gross_pnl;
                    totals.emergency_unwind_fees += first_fees + unwind_fees;
                    reject(totals, "second_leg_unfilled_emergency_unwind");
                    completed[index] = true;
                    continue;
                }
                let first_won = winner_matches(market.winner.as_deref(), up_first);
                let payout = if first_won {
                    filled_shares
                } else {
                    Decimal::ZERO
                };
                totals.unrecovered_one_sided += 1;
                totals.one_sided_wins += usize::from(first_won);
                totals.one_sided_losses += usize::from(!first_won);
                let one_sided_gross_pnl = payout - first_cost;
                totals.gross_pnl += one_sided_gross_pnl;
                totals.fees += first_fees;
                totals.one_sided_gross_pnl += one_sided_gross_pnl;
                totals.one_sided_fees += first_fees;
                reject(totals, "second_leg_unfilled");
                completed[index] = true;
                continue;
            };
            let second_average = second_cost / filled_shares;
            let second_fees =
                dynamic_fee_per_share(config.fee_rate, config.fee_exponent, second_average)
                    * filled_shares;
            let gross_pnl = filled_shares - first_cost - second_cost;
            totals.fills += 1;
            totals.wins += 1;
            totals.gross_pnl += gross_pnl;
            totals.fees += first_fees + second_fees;
            totals.paired_gross_pnl += gross_pnl;
            totals.paired_fees += first_fees + second_fees;
            completed[index] = true;
        }
        if completed.iter().all(|done| *done) {
            return Ok(());
        }
    }
    Ok(())
}

fn spread_asset_totals<'a>(scenario: &'a mut SpreadScenarioTotals, asset: &str) -> &'a mut Totals {
    scenario
        .assets
        .get_mut(asset)
        .expect("asset totals initialized")
}

fn reject_active_spread_scenarios(
    scenarios: &mut [SpreadScenarioTotals],
    completed: &[bool],
    asset: &str,
    reason: &str,
) {
    for (index, scenario) in scenarios.iter_mut().enumerate() {
        if !completed[index] {
            reject(spread_asset_totals(scenario, asset), reason);
        }
    }
}

fn reset_active_confirmations(confirmations: &mut [Option<i64>], completed: &[bool]) {
    for (index, confirmation) in confirmations.iter_mut().enumerate() {
        if !completed[index] {
            *confirmation = None;
        }
    }
}

fn replay_winner_market_grid(
    config: &ReplayConfig,
    market: &Market,
    snapshots: &[Snapshot],
    scenario_totals: &mut [WinnerScenarioTotals],
) -> Result<()> {
    let store = ChainlinkTwapStore::default();
    let mut last_confirmation = None;
    let mut confirmation_direction = None;
    let mut confirmations = 0u32;
    let mut completed = vec![false; scenario_totals.len()];
    for signal in snapshots {
        if signal.twap_source.as_deref() == Some("chainlink_twap") {
            if let (Some(value), Some(observed_at_ms)) = (signal.twap, signal.twap_observed_at_ms) {
                store.ingest(
                    &market.coin,
                    TwapSample {
                        observed_at_ms,
                        value,
                    },
                );
            }
        }
        let remaining_ms = market.end_ms.saturating_sub(signal.time_ms);
        if remaining_ms < 0 || remaining_ms > config.winner_before_end_secs * 1_000 {
            continue;
        }
        let Some(direction) = detect_winner_asks(
            signal.up_ask,
            signal.down_ask,
            config.winner_min_price,
            config.winner_max_price,
        ) else {
            continue;
        };
        for (index, totals) in scenario_totals.iter_mut().enumerate() {
            if completed[index] {
                continue;
            }
            totals
                .assets
                .get_mut(&market.coin)
                .expect("asset totals initialized")
                .candidates += 1;
        }
        let (bid, ask_size) = if direction.buy_yes {
            (signal.up_bid, signal.up_ask_size)
        } else {
            (signal.down_bid, signal.down_ask_size)
        };
        let (Some(bid), Some(ask_size)) = (bid, ask_size) else {
            for (index, totals) in scenario_totals.iter_mut().enumerate() {
                if completed[index] {
                    continue;
                }
                reject(
                    totals
                        .assets
                        .get_mut(&market.coin)
                        .expect("asset totals initialized"),
                    "missing_bid_or_depth",
                );
            }
            continue;
        };
        let mut winner_policy = config.winner_policy_for_asset(&market.coin);
        winner_policy.fee_buffer =
            dynamic_fee_per_share(config.fee_rate, config.fee_exponent, direction.price);
        let evaluation = store.evaluate(
            WinnerModelInput {
                asset: &market.coin,
                window_start_ms: market.start_ms,
                now_ms: signal.time_ms,
                seconds_remaining: (remaining_ms / 1_000).max(1),
                buy_yes: direction.buy_yes,
                bid_price: bid,
                ask_price: direction.price,
                ask_size,
            },
            winner_policy,
        );
        if !evaluation.admitted() {
            if let Some(reason) = evaluation.rejection {
                let reason = format!("model_{reason:?}").to_ascii_lowercase();
                for (index, totals) in scenario_totals.iter_mut().enumerate() {
                    if completed[index] {
                        continue;
                    }
                    reject(
                        totals
                            .assets
                            .get_mut(&market.coin)
                            .expect("asset totals initialized"),
                        &reason,
                    );
                }
            }
            confirmations = 0;
            confirmation_direction = None;
            continue;
        }
        let Some(observed_at_ms) = evaluation.observed_at_ms else {
            for (index, totals) in scenario_totals.iter_mut().enumerate() {
                if completed[index] {
                    continue;
                }
                reject(
                    totals
                        .assets
                        .get_mut(&market.coin)
                        .expect("asset totals initialized"),
                    "model_missing_twap_observation",
                );
            }
            confirmations = 0;
            confirmation_direction = None;
            continue;
        };
        for (index, totals) in scenario_totals.iter_mut().enumerate() {
            if completed[index] {
                continue;
            }
            totals
                .assets
                .get_mut(&market.coin)
                .expect("asset totals initialized")
                .admitted += 1;
        }
        if confirmation_direction != Some(direction.buy_yes) {
            confirmations = 0;
            last_confirmation = None;
            confirmation_direction = Some(direction.buy_yes);
        }
        if last_confirmation != Some(observed_at_ms) {
            confirmations += 1;
            last_confirmation = Some(observed_at_ms);
        }
        if confirmations < config.winner_confirmations {
            continue;
        }
        for (index, scenario_run) in scenario_totals.iter_mut().enumerate() {
            if completed[index] {
                continue;
            }
            let totals = scenario_run
                .assets
                .get_mut(&market.coin)
                .expect("asset totals initialized");
            let scenario = scenario_run.scenario;
            let Some(execution) = snapshot_at_or_after(
                snapshots,
                signal.time_ms.saturating_add(scenario.execution_delay_ms),
            ) else {
                reject(totals, "no_delayed_snapshot");
                completed[index] = true;
                continue;
            };
            let execution_book = if direction.buy_yes {
                execution.up_book.as_deref()
            } else {
                execution.down_book.as_deref()
            };
            let signal_fee_per_share =
                dynamic_fee_per_share(config.fee_rate, config.fee_exponent, direction.price);
            let mut filled_shares =
                (config.order_usdc / (direction.price + signal_fee_per_share)).round_dp(6);
            let Some(mut cost) =
                walk_asks(execution_book, filled_shares, scenario.liquidity_haircut)?
            else {
                reject(totals, "execution_depth_insufficient");
                continue;
            };
            let mut average = cost / filled_shares;
            if average > config.winner_max_price {
                reject(totals, "execution_price_above_limit");
                continue;
            }
            let mut fees = dynamic_fee_per_share(config.fee_rate, config.fee_exponent, average)
                * filled_shares;
            if cost + fees > config.order_usdc {
                filled_shares =
                    (filled_shares * config.order_usdc / (cost + fees) * dec!(0.999)).round_dp(6);
                let Some(resized_cost) =
                    walk_asks(execution_book, filled_shares, scenario.liquidity_haircut)?
                else {
                    reject(totals, "budget_resize_depth_insufficient");
                    continue;
                };
                cost = resized_cost;
                average = cost / filled_shares;
                fees = dynamic_fee_per_share(config.fee_rate, config.fee_exponent, average)
                    * filled_shares;
            }
            if cost + fees > config.order_usdc || cost < dec!(1) {
                reject(totals, "budget_or_minimum_order_notional");
                continue;
            }
            let won = winner_matches(market.winner.as_deref(), direction.buy_yes);
            let payout = if won { filled_shares } else { Decimal::ZERO };
            totals.fills += 1;
            totals.wins += usize::from(won);
            totals.losses += usize::from(!won);
            totals.gross_pnl += payout - cost;
            totals.fees += fees;
            completed[index] = true;
        }
        if completed.iter().all(|done| *done) {
            return Ok(());
        }
    }
    Ok(())
}

fn walk_asks(
    book_zstd: Option<&[u8]>,
    target: Decimal,
    haircut: Decimal,
) -> Result<Option<Decimal>> {
    let Some(asks) = decode_asks(book_zstd)? else {
        return Ok(None);
    };
    Ok(walk_ask_levels(&asks, target, haircut))
}

fn decode_asks(book_zstd: Option<&[u8]>) -> Result<Option<Vec<(Decimal, Decimal)>>> {
    decode_book_side(book_zstd, "asks", true)
}

fn decode_bids(book_zstd: Option<&[u8]>) -> Result<Option<Vec<(Decimal, Decimal)>>> {
    decode_book_side(book_zstd, "bids", false)
}

fn decode_book_side(
    book_zstd: Option<&[u8]>,
    side: &str,
    ascending: bool,
) -> Result<Option<Vec<(Decimal, Decimal)>>> {
    let Some(book_zstd) = book_zstd else {
        return Ok(None);
    };
    let decoded = zstd::stream::decode_all(book_zstd).context("decode compressed order book")?;
    let value: Value = serde_json::from_slice(&decoded).context("parse compressed order book")?;
    let Some(levels) = value.get(side).and_then(Value::as_array) else {
        return Ok(None);
    };
    let mut asks = levels
        .iter()
        .filter_map(|level| {
            Some((
                value_decimal(level.get("price")?)?,
                value_decimal(level.get("size")?)?,
            ))
        })
        .filter(|(price, size)| *price > Decimal::ZERO && *size > Decimal::ZERO)
        .collect::<Vec<_>>();
    if ascending {
        asks.sort_by_key(|(price, _)| *price);
    } else {
        asks.sort_by_key(|(price, _)| std::cmp::Reverse(*price));
    }
    Ok(Some(asks))
}

fn walk_ask_levels(
    asks: &[(Decimal, Decimal)],
    target: Decimal,
    haircut: Decimal,
) -> Option<Decimal> {
    walk_ask_levels_up_to(asks, target, haircut, Decimal::MAX)
}

fn walk_ask_levels_up_to(
    asks: &[(Decimal, Decimal)],
    target: Decimal,
    haircut: Decimal,
    maximum_price: Decimal,
) -> Option<Decimal> {
    let mut remaining = target;
    let mut cost = Decimal::ZERO;
    for (price, size) in asks.iter().filter(|(price, _)| *price <= maximum_price) {
        let filled = remaining.min(*size * haircut);
        cost += filled * *price;
        remaining -= filled;
        if remaining <= Decimal::ZERO {
            return Some(cost);
        }
    }
    None
}

fn walk_bid_levels(
    bids: &[(Decimal, Decimal)],
    target: Decimal,
    haircut: Decimal,
) -> Option<Decimal> {
    let mut remaining = target;
    let mut proceeds = Decimal::ZERO;
    for (price, size) in bids {
        let filled = remaining.min(*size * haircut);
        proceeds += filled * *price;
        remaining -= filled;
        if remaining <= Decimal::ZERO {
            return Some(proceeds);
        }
    }
    None
}

fn cached_asks<'a>(
    cache: &'a mut BookCache,
    snapshots: &[Snapshot],
    snapshot_index: usize,
    up: bool,
) -> Result<Option<&'a [(Decimal, Decimal)]>> {
    let key = (snapshot_index, up, false);
    if let std::collections::hash_map::Entry::Vacant(entry) = cache.entry(key) {
        let snapshot = &snapshots[snapshot_index];
        let bytes = if up {
            snapshot.up_book.as_deref()
        } else {
            snapshot.down_book.as_deref()
        };
        entry.insert(decode_asks(bytes)?);
    }
    Ok(cache.get(&key).and_then(|asks| asks.as_deref()))
}

fn cached_bids<'a>(
    cache: &'a mut BookCache,
    snapshots: &[Snapshot],
    snapshot_index: usize,
    up: bool,
) -> Result<Option<&'a [(Decimal, Decimal)]>> {
    let key = (snapshot_index, up, true);
    if let std::collections::hash_map::Entry::Vacant(entry) = cache.entry(key) {
        let snapshot = &snapshots[snapshot_index];
        let bytes = if up {
            snapshot.up_book.as_deref()
        } else {
            snapshot.down_book.as_deref()
        };
        entry.insert(decode_bids(bytes)?);
    }
    Ok(cache.get(&key).and_then(|bids| bids.as_deref()))
}

fn ask_depth_at_or_better(asks: &[(Decimal, Decimal)], best_ask: Decimal) -> Decimal {
    asks.iter()
        .filter(|(price, size)| {
            *price > Decimal::ZERO && *price <= best_ask && *size > Decimal::ZERO
        })
        .map(|(price, size)| *price * *size)
        .sum()
}

fn snapshot_at_or_after(snapshots: &[Snapshot], target_ms: i64) -> Option<&Snapshot> {
    snapshot_index_at_or_after(snapshots, target_ms).and_then(|index| snapshots.get(index))
}

fn snapshot_index_at_or_after(snapshots: &[Snapshot], target_ms: i64) -> Option<usize> {
    let index = snapshots.partition_point(|snapshot| snapshot.time_ms < target_ms);
    (index < snapshots.len()).then_some(index)
}

fn dynamic_fee_per_share(rate: Decimal, exponent: u64, price: Decimal) -> Decimal {
    let base = price * (Decimal::ONE - price);
    let powered = (0..exponent).fold(Decimal::ONE, |value, _| value * base);
    rate * powered
}

fn winner_matches(winner: Option<&str>, buy_yes: bool) -> bool {
    match winner.map(|value| value.trim().to_ascii_lowercase()) {
        Some(value) if matches!(value.as_str(), "up" | "yes") => buy_yes,
        Some(value) if matches!(value.as_str(), "down" | "no") => !buy_yes,
        _ => false,
    }
}

fn winner_scenarios(config: &ReplayConfig) -> Vec<WinnerScenario> {
    let mut scenarios = Vec::new();
    for delay in &config.winner_backtest_delays_ms {
        for haircut in &config.winner_backtest_liquidity_haircuts {
            scenarios.push(WinnerScenario {
                execution_delay_ms: *delay,
                liquidity_haircut: *haircut,
            });
        }
    }
    scenarios
}

fn spread_scenarios(config: &ReplayConfig) -> Vec<SpreadScenario> {
    let baseline = SpreadScenario {
        order_usdc: config.order_usdc,
        confirmation_ms: 100,
        preflight_delay_ms: config.execution_delay_ms,
        second_leg_delay_ms: config.execution_delay_ms,
        liquidity_haircut: config.liquidity_haircut,
        second_leg_depth_multiplier: dec!(2),
        emergency_unwind_delay_ms: 50,
    };
    let mut scenarios = vec![baseline];
    let mut push = |scenario: SpreadScenario| {
        if !scenarios.contains(&scenario) {
            scenarios.push(scenario);
        }
    };
    for order_usdc in &config.spread_backtest_order_sizes_usdc {
        push(SpreadScenario {
            order_usdc: *order_usdc,
            ..baseline
        });
    }
    for confirmation_ms in &config.spread_backtest_confirmation_ms {
        push(SpreadScenario {
            confirmation_ms: *confirmation_ms,
            ..baseline
        });
    }
    for preflight_delay_ms in &config.spread_backtest_preflight_delays_ms {
        push(SpreadScenario {
            preflight_delay_ms: *preflight_delay_ms,
            ..baseline
        });
    }
    for second_leg_delay_ms in &config.spread_backtest_second_leg_delays_ms {
        push(SpreadScenario {
            second_leg_delay_ms: *second_leg_delay_ms,
            ..baseline
        });
    }
    for liquidity_haircut in &config.spread_backtest_liquidity_haircuts {
        push(SpreadScenario {
            liquidity_haircut: *liquidity_haircut,
            ..baseline
        });
    }
    for second_leg_depth_multiplier in &config.spread_backtest_second_leg_depth_multipliers {
        push(SpreadScenario {
            second_leg_depth_multiplier: *second_leg_depth_multiplier,
            ..baseline
        });
    }
    for emergency_unwind_delay_ms in &config.spread_backtest_emergency_unwind_delays_ms {
        push(SpreadScenario {
            emergency_unwind_delay_ms: *emergency_unwind_delay_ms,
            ..baseline
        });
    }
    // The historical feed is snapshot-based: any positive preflight latency can
    // jump to the next coarse snapshot. Repeat the key safety dimensions around
    // a zero-preflight research profile so their effects remain identifiable.
    let fast_research = SpreadScenario {
        preflight_delay_ms: 0,
        ..baseline
    };
    for confirmation_ms in &config.spread_backtest_confirmation_ms {
        push(SpreadScenario {
            confirmation_ms: *confirmation_ms,
            ..fast_research
        });
    }
    for second_leg_delay_ms in &config.spread_backtest_second_leg_delays_ms {
        push(SpreadScenario {
            second_leg_delay_ms: *second_leg_delay_ms,
            ..fast_research
        });
    }
    for second_leg_depth_multiplier in &config.spread_backtest_second_leg_depth_multipliers {
        push(SpreadScenario {
            second_leg_depth_multiplier: *second_leg_depth_multiplier,
            ..fast_research
        });
    }
    for emergency_unwind_delay_ms in &config.spread_backtest_emergency_unwind_delays_ms {
        push(SpreadScenario {
            emergency_unwind_delay_ms: *emergency_unwind_delay_ms,
            ..fast_research
        });
    }
    push(SpreadScenario {
        order_usdc: dec!(2.5),
        confirmation_ms: 250,
        preflight_delay_ms: 100,
        second_leg_delay_ms: 25,
        liquidity_haircut: dec!(0.50),
        second_leg_depth_multiplier: dec!(3),
        emergency_unwind_delay_ms: 25,
    });
    push(SpreadScenario {
        order_usdc: dec!(2.5),
        confirmation_ms: 500,
        preflight_delay_ms: 250,
        second_leg_delay_ms: 100,
        liquidity_haircut: dec!(0.50),
        second_leg_depth_multiplier: dec!(5),
        emergency_unwind_delay_ms: 50,
    });
    scenarios
}

fn aggregate_spread_scenario_totals(scenario: &SpreadScenarioTotals) -> Totals {
    let mut aggregate = Totals::default();
    for totals in scenario.assets.values() {
        merge_totals(&mut aggregate, totals);
    }
    aggregate
}

fn aggregate_scenario_totals(scenario: &WinnerScenarioTotals) -> Totals {
    let mut aggregate = Totals::default();
    for totals in scenario.assets.values() {
        merge_totals(&mut aggregate, totals);
    }
    aggregate
}

fn merge_totals(target: &mut Totals, source: &Totals) {
    target.markets += source.markets;
    target.snapshots += source.snapshots;
    target.candidates += source.candidates;
    target.admitted += source.admitted;
    target.fills += source.fills;
    target.wins += source.wins;
    target.losses += source.losses;
    target.one_sided_fills += source.one_sided_fills;
    target.one_sided_wins += source.one_sided_wins;
    target.one_sided_losses += source.one_sided_losses;
    target.emergency_unwinds += source.emergency_unwinds;
    target.unrecovered_one_sided += source.unrecovered_one_sided;
    target.gross_pnl += source.gross_pnl;
    target.fees += source.fees;
    target.paired_gross_pnl += source.paired_gross_pnl;
    target.paired_fees += source.paired_fees;
    target.one_sided_gross_pnl += source.one_sided_gross_pnl;
    target.one_sided_fees += source.one_sided_fees;
    target.emergency_unwind_gross_pnl += source.emergency_unwind_gross_pnl;
    target.emergency_unwind_fees += source.emergency_unwind_fees;
    for (reason, count) in &source.rejections {
        *target.rejections.entry(reason.clone()).or_default() += count;
    }
}

fn winner_metrics_report(totals: &Totals) -> WinnerMetricsReport {
    WinnerMetricsReport {
        markets: totals.markets,
        snapshots: totals.snapshots,
        candidates: totals.candidates,
        admitted: totals.admitted,
        fills: totals.fills,
        wins: totals.wins,
        losses: totals.losses,
        one_sided_fills: totals.one_sided_fills,
        one_sided_wins: totals.one_sided_wins,
        one_sided_losses: totals.one_sided_losses,
        emergency_unwinds: totals.emergency_unwinds,
        unrecovered_one_sided: totals.unrecovered_one_sided,
        one_sided_rate_pct: if totals.fills + totals.one_sided_fills == 0 {
            "0".to_string()
        } else {
            (Decimal::from(totals.one_sided_fills)
                / Decimal::from(totals.fills + totals.one_sided_fills)
                * dec!(100))
            .round_dp(4)
            .to_string()
        },
        unrecovered_one_sided_rate_pct: if totals.fills + totals.one_sided_fills == 0 {
            "0".to_string()
        } else {
            (Decimal::from(totals.unrecovered_one_sided)
                / Decimal::from(totals.fills + totals.one_sided_fills)
                * dec!(100))
            .round_dp(4)
            .to_string()
        },
        emergency_unwind_net_pnl_usdc: (totals.emergency_unwind_gross_pnl
            - totals.emergency_unwind_fees)
            .round_dp(6)
            .to_string(),
        win_rate_pct: if totals.fills == 0 {
            "0".to_string()
        } else {
            (Decimal::from(totals.wins) / Decimal::from(totals.fills) * dec!(100))
                .round_dp(4)
                .to_string()
        },
        rejections: totals.rejections.clone(),
        gross_pnl_usdc: totals.gross_pnl.round_dp(6).to_string(),
        fees_usdc: totals.fees.round_dp(6).to_string(),
        net_pnl_usdc: (totals.gross_pnl - totals.fees).round_dp(6).to_string(),
        paired_net_pnl_usdc: (totals.paired_gross_pnl - totals.paired_fees)
            .round_dp(6)
            .to_string(),
        one_sided_net_pnl_usdc: (totals.one_sided_gross_pnl - totals.one_sided_fees)
            .round_dp(6)
            .to_string(),
    }
}

fn winner_scenario_report(scenario: &WinnerScenarioTotals) -> WinnerScenarioReport {
    WinnerScenarioReport {
        id: scenario.scenario.id(),
        execution_delay_ms: scenario.scenario.execution_delay_ms,
        liquidity_haircut: scenario.scenario.liquidity_haircut.to_string(),
        overall: winner_metrics_report(&aggregate_scenario_totals(scenario)),
        assets: scenario
            .assets
            .iter()
            .map(|(asset, totals)| (asset.clone(), winner_metrics_report(totals)))
            .collect(),
    }
}

fn spread_scenario_report(scenario: &SpreadScenarioTotals) -> SpreadScenarioReport {
    SpreadScenarioReport {
        id: scenario.scenario.id(),
        order_usdc: scenario.scenario.order_usdc.to_string(),
        confirmation_ms: scenario.scenario.confirmation_ms,
        preflight_delay_ms: scenario.scenario.preflight_delay_ms,
        second_leg_delay_ms: scenario.scenario.second_leg_delay_ms,
        liquidity_haircut: scenario.scenario.liquidity_haircut.to_string(),
        second_leg_depth_multiplier: scenario.scenario.second_leg_depth_multiplier.to_string(),
        emergency_unwind_delay_ms: scenario.scenario.emergency_unwind_delay_ms,
        overall: winner_metrics_report(&aggregate_spread_scenario_totals(scenario)),
        assets: scenario
            .assets
            .iter()
            .map(|(asset, totals)| (asset.clone(), winner_metrics_report(totals)))
            .collect(),
    }
}

fn write_json_file(path: &Path, value: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value)?;
    let mut file = File::create(path)?;
    file.write_all(&bytes)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

fn write_spread_analysis(path: &Path, report: &SpreadReplayReport) -> Result<()> {
    let total_fills: usize = report
        .scenarios
        .iter()
        .map(|scenario| scenario.overall.fills + scenario.overall.one_sided_fills)
        .sum();
    let mut markdown = String::from(
        "# Binary Complement Arbitrage 本地回测分析\n\n\
         ## 总结\n\n",
    );
    markdown.push_str(&format!(
        "市场周期：`{}`；准入判断：`{}`；正收益场景：`{}/{}`。\n\n",
        report.market_type,
        report.readiness,
        report.positive_scenarios,
        report.scenarios.len()
    ));
    if total_fills == 0 {
        markdown.push_str(
            "全部订单金额、延迟与流动性场景均无模拟成交。应先检查候选、信号深度、动态手续费净边际和执行深度漏斗，不宜直接进入 dry-run。\n\n",
        );
    } else if report.readiness == "NOT_READY_ONE_SIDED_RISK" {
        markdown.push_str(
            "至少一个场景在紧急退出后仍未恢复的单腿率超过配置上限。总 PnL 可能被方向性结算偶然抬高，不能视为套利收益；进入 dry-run 前必须先降低最终裸露风险。\n\n",
        );
    } else if report.readiness == "READY_FOR_PAPER_DRY_RUN_LATENCY_CALIBRATION" {
        markdown.push_str(
            "同快照研究场景已有足量且双腿净收益为正的样本，紧急退出后最终未恢复单腿率也在上限内；但离散历史快照无法解析真实毫秒级预检与第二腿延迟。适合进入服务器 PAPER dry-run 校准，尚不代表适合真实资金运行。\n\n",
        );
    } else if report.positive_scenarios == report.scenarios.len() {
        markdown.push_str(
            "全部敏感性场景的净收益均为正，具备进入 dry-run 复核的必要条件；是否达到样本门槛仍以准入判断为准。\n\n",
        );
    } else if report.positive_scenarios > 0 {
        markdown.push_str(
            "只有部分敏感性场景为正，策略对订单金额或执行条件敏感，不能用最佳场景推断可实现收益。\n\n",
        );
    } else {
        markdown.push_str(
            "存在模拟成交，但没有正收益场景；在改善净边际或成本模型前不建议进入 dry-run。\n\n",
        );
    }
    markdown.push_str("## 场景汇总\n\n");
    markdown.push_str(
        "| 场景 | 确认/预检/第二腿(ms) | 覆盖倍数 | 双腿 | 第二腿失败 | 紧急退出 | 未恢复 | 双腿PnL | 退出PnL | 单腿PnL | 总PnL |\n|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|\n",
    );
    for scenario in &report.scenarios {
        markdown.push_str(&format!(
            "| {} | {}/{}/{} | {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
            scenario.id,
            scenario.confirmation_ms,
            scenario.preflight_delay_ms,
            scenario.second_leg_delay_ms,
            scenario.second_leg_depth_multiplier,
            scenario.overall.fills,
            scenario.overall.one_sided_fills,
            scenario.overall.emergency_unwinds,
            scenario.overall.unrecovered_one_sided,
            scenario.overall.paired_net_pnl_usdc,
            scenario.overall.emergency_unwind_net_pnl_usdc,
            scenario.overall.one_sided_net_pnl_usdc,
            scenario.overall.net_pnl_usdc,
        ));
    }
    if let Some(baseline) = report.scenarios.first() {
        markdown.push_str("\n## 基准场景分资产结果\n\n");
        markdown.push_str(
            "| 资产 | 市场 | 快照 | 候选 | 准入 | 双腿成交 | 单腿成交 | 净收益(USDC) |\n|---|---:|---:|---:|---:|---:|---:|---:|\n",
        );
        for (asset, metrics) in &baseline.assets {
            markdown.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} | {} |\n",
                asset,
                metrics.markets,
                metrics.snapshots,
                metrics.candidates,
                metrics.admitted,
                metrics.fills,
                metrics.one_sided_fills,
                metrics.net_pnl_usdc,
            ));
        }
        let mut rejections = baseline.overall.rejections.iter().collect::<Vec<_>>();
        rejections.sort_by_key(|(_, count)| std::cmp::Reverse(**count));
        markdown.push_str("\n## 基准场景主要拒绝原因\n\n");
        for (reason, count) in rejections.into_iter().take(12) {
            markdown.push_str(&format!("- `{reason}`: {count}\n"));
        }
    }
    markdown.push_str(
        "\n## Dry-run 前质量门槛\n\n- 最保守场景至少 100 个独立市场成交。\n- 所有场景扣除费用后净收益为正。\n- 收益不能只集中于单一资产或单一宽松流动性场景。\n- dry-run 仍需校准队列、部分成交、两腿风险、真实费率和 API 延迟。\n",
    );
    let mut file = File::create(path)?;
    file.write_all(markdown.as_bytes())?;
    file.sync_all()?;
    Ok(())
}

fn write_winner_analysis(path: &Path, report: &WinnerReplayReport) -> Result<()> {
    let positive = report
        .scenarios
        .iter()
        .filter(|scenario| decimal_string(&scenario.overall.net_pnl_usdc) > Decimal::ZERO)
        .count();
    let total_fills: usize = report
        .scenarios
        .iter()
        .map(|scenario| scenario.overall.fills)
        .sum();
    let mut markdown = String::from(
        "# Late-Resolution Winner Trading 本地回测分析\n\n\
         ## 总结\n\n",
    );
    markdown.push_str(&format!(
        "市场周期：`{}`；准入判断：`{}`；正收益场景：`{}/{}`。\n\n",
        report.market_type,
        report.readiness,
        report.positive_scenarios,
        report.scenarios.len()
    ));
    if total_fills == 0 {
        markdown.push_str(
            "全部敏感性场景均无模拟成交。当前数据说明候选盘口存在，但模型准入、连续确认或执行约束尚未形成完整成交闭环；进入 dry-run 前应先查看拒绝漏斗。\n\n",
        );
    } else if positive == report.scenarios.len() {
        markdown.push_str(
            "全部延迟与流动性场景的净收益均为正，历史回放具有初步稳定性；仍需通过 dry-run 校准真实延迟和成交率。\n\n",
        );
    } else if positive > 0 {
        markdown.push_str(
            "只有部分延迟与流动性场景为正，策略对执行条件敏感，不应直接使用最优场景推断实盘收益。\n\n",
        );
    } else {
        markdown.push_str(
            "存在模拟成交，但全部敏感性场景净收益非正；当前参数不建议进入 dry-run 扩大验证。\n\n",
        );
    }
    markdown.push_str("## 场景汇总\n\n");
    markdown.push_str(
        "| 场景 | 延迟(ms) | 流动性折扣 | 候选 | 准入 | 成交 | 胜率 | 净收益(USDC) |\n|---|---:|---:|---:|---:|---:|---:|---:|\n",
    );
    for scenario in &report.scenarios {
        markdown.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {}% | {} |\n",
            scenario.id,
            scenario.execution_delay_ms,
            scenario.liquidity_haircut,
            scenario.overall.candidates,
            scenario.overall.admitted,
            scenario.overall.fills,
            scenario.overall.win_rate_pct,
            scenario.overall.net_pnl_usdc,
        ));
    }
    markdown.push_str("\n## 分资产结果\n\n");
    markdown.push_str(
        "| 场景 | 资产 | 市场 | 成交 | 胜/负 | 胜率 | 净收益(USDC) |\n|---|---|---:|---:|---:|---:|---:|\n",
    );
    for scenario in &report.scenarios {
        for (asset, metrics) in &scenario.assets {
            markdown.push_str(&format!(
                "| {} | {} | {} | {} | {}/{} | {}% | {} |\n",
                scenario.id,
                asset,
                metrics.markets,
                metrics.fills,
                metrics.wins,
                metrics.losses,
                metrics.win_rate_pct,
                metrics.net_pnl_usdc,
            ));
        }
    }
    if let Some(baseline) = report.scenarios.first() {
        let mut rejections = baseline.overall.rejections.iter().collect::<Vec<_>>();
        rejections.sort_by_key(|(_, count)| std::cmp::Reverse(**count));
        markdown.push_str("\n## 基准场景主要拒绝原因\n\n");
        for (reason, count) in rejections.into_iter().take(12) {
            markdown.push_str(&format!("- `{reason}`: {count}\n"));
        }
    }
    markdown.push_str(
        "\n## 使用边界\n\n- 手续费为配置假设，不是逐市场历史费率。\n- 历史盘口不能证明真实队列位置或订单成交。\n- 延迟与流动性折扣用于敏感性分析，需在第二阶段 dry-run 中校准。\n",
    );
    let mut file = File::create(path)?;
    file.write_all(markdown.as_bytes())?;
    file.sync_all()?;
    Ok(())
}

fn decimal_string(value: &str) -> Decimal {
    Decimal::from_str(value).unwrap_or_default()
}

fn reject(totals: &mut Totals, reason: &str) {
    *totals.rejections.entry(reason.to_string()).or_default() += 1;
}

fn normalize_ms(value: i64) -> i64 {
    if value.abs() < 10_000_000_000 {
        value.saturating_mul(1_000)
    } else {
        value
    }
}

fn decimal_column(row: &Row<'_>, index: usize) -> rusqlite::Result<Option<Decimal>> {
    row.get::<_, Option<String>>(index)
        .map(|value| value.and_then(|value| Decimal::from_str(&value).ok()))
}

fn value_decimal(value: &Value) -> Option<Decimal> {
    match value {
        Value::String(value) => Decimal::from_str(value).ok(),
        Value::Number(value) => Decimal::from_str(&value.to_string()).ok(),
        _ => None,
    }
}

fn env_decimal(name: &str, default: &str) -> Result<Decimal> {
    Decimal::from_str(&std::env::var(name).unwrap_or_else(|_| default.to_string()))
        .with_context(|| format!("{name} must be a decimal"))
}

fn env_bool(name: &str, default: bool) -> Result<bool> {
    let value = std::env::var(name).unwrap_or_else(|_| default.to_string());
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => bail!("{name} must be true or false"),
    }
}

fn env_parse<T>(name: &str, default: T) -> Result<T>
where
    T: FromStr + std::fmt::Display,
    T::Err: std::error::Error + Send + Sync + 'static,
{
    std::env::var(name)
        .unwrap_or_else(|_| default.to_string())
        .parse()
        .with_context(|| format!("{name} has an invalid value"))
}

fn env_i64_list(name: &str, default: &[i64]) -> Result<Vec<i64>> {
    let raw = std::env::var(name).unwrap_or_else(|_| {
        default
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(",")
    });
    let mut values = raw
        .split(',')
        .map(|value| {
            value
                .trim()
                .parse::<i64>()
                .with_context(|| format!("{name} contains an invalid integer"))
        })
        .collect::<Result<Vec<_>>>()?;
    values.sort_unstable();
    values.dedup();
    Ok(values)
}

fn validate_non_negative_i64_list(name: &str, values: &[i64]) -> Result<()> {
    if values.is_empty() || values.iter().any(|value| *value < 0) {
        bail!("{name} must contain non-negative values");
    }
    Ok(())
}

fn env_decimal_list(name: &str, default: &[Decimal]) -> Result<Vec<Decimal>> {
    let raw = std::env::var(name).unwrap_or_else(|_| {
        default
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(",")
    });
    let mut values = raw
        .split(',')
        .map(|value| {
            Decimal::from_str(value.trim())
                .with_context(|| format!("{name} contains an invalid decimal"))
        })
        .collect::<Result<Vec<_>>>()?;
    values.sort_unstable();
    values.dedup();
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn market_loader_isolates_five_and_fifteen_minute_periods() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE markets (
                    market_id TEXT PRIMARY KEY,
                    coin TEXT NOT NULL,
                    market_type TEXT NOT NULL,
                    start_time INTEGER,
                    end_time INTEGER,
                    winner TEXT,
                    status TEXT NOT NULL
                 );
                 INSERT INTO markets VALUES
                    ('five', 'btc', '5m', 1000, 301000, 'Up', 'complete'),
                    ('fifteen', 'btc', '15m', 1000, 901000, 'Down', 'complete'),
                    ('pending', 'eth', '15m', 1000, 901000, NULL, 'downloading');",
            )
            .unwrap();

        let five = load_markets(&connection, "5m").unwrap();
        let fifteen = load_markets(&connection, "15m").unwrap();
        assert_eq!(five.len(), 1);
        assert_eq!(five[0].id, "five");
        assert_eq!(fifteen.len(), 1);
        assert_eq!(fifteen[0].id, "fifteen");
    }

    #[test]
    fn fee_and_time_normalization_are_deterministic() {
        assert_eq!(normalize_ms(1_700_000_000), 1_700_000_000_000);
        assert_eq!(normalize_ms(1_700_000_000_123), 1_700_000_000_123);
        assert_eq!(
            dynamic_fee_per_share(dec!(0.25), 2, dec!(0.5)),
            dec!(0.015625)
        );
    }

    #[test]
    fn winner_labels_map_to_binary_side() {
        assert!(winner_matches(Some("Up"), true));
        assert!(winner_matches(Some("NO"), false));
        assert!(!winner_matches(Some("Down"), true));
        assert!(!winner_matches(None, false));
    }

    #[test]
    fn winner_assets_use_distinct_policy_buckets() {
        assert_eq!(winner_asset_bucket("BTC"), "btc");
        assert_eq!(winner_asset_bucket("ethereum"), "eth");
        assert_eq!(winner_asset_bucket("sol"), "other");
    }

    #[test]
    fn delayed_snapshot_lookup_uses_first_snapshot_at_or_after_target() {
        let snapshot = |time_ms| Snapshot {
            time_ms,
            twap: None,
            twap_observed_at_ms: None,
            twap_source: None,
            up_bid: None,
            up_ask: None,
            up_ask_size: None,
            down_bid: None,
            down_ask: None,
            down_ask_size: None,
            up_book: None,
            down_book: None,
        };
        let snapshots = vec![snapshot(100), snapshot(250), snapshot(500)];
        assert_eq!(snapshot_at_or_after(&snapshots, 101).unwrap().time_ms, 250);
        assert_eq!(snapshot_at_or_after(&snapshots, 250).unwrap().time_ms, 250);
        assert!(snapshot_at_or_after(&snapshots, 501).is_none());
    }

    #[test]
    fn spread_depth_walk_applies_price_limit_and_liquidity_haircut() {
        let asks = vec![(dec!(0.40), dec!(10)), (dec!(0.50), dec!(10))];
        assert_eq!(ask_depth_at_or_better(&asks, dec!(0.40)), dec!(4));
        assert_eq!(
            walk_ask_levels_up_to(&asks, dec!(6), dec!(0.50), dec!(0.50)),
            Some(dec!(2.50))
        );
        assert_eq!(
            walk_ask_levels_up_to(&asks, dec!(6), dec!(0.50), dec!(0.40)),
            None
        );
    }

    #[test]
    fn emergency_unwind_walks_highest_bids_and_requires_full_fok_size() {
        let bids = vec![(dec!(0.60), dec!(4)), (dec!(0.55), dec!(4))];
        assert_eq!(
            walk_bid_levels(&bids, dec!(3), dec!(0.50)),
            Some(dec!(1.75))
        );
        assert_eq!(walk_bid_levels(&bids, dec!(5), dec!(0.50)), None);
    }

    #[test]
    fn broken_edge_resets_only_active_confirmation_windows() {
        let mut confirmations = vec![Some(100), Some(200), Some(300)];
        reset_active_confirmations(&mut confirmations, &[false, true, false]);
        assert_eq!(confirmations, vec![None, Some(200), None]);
    }

    #[test]
    fn second_leg_failure_and_unrecovered_rates_share_the_first_leg_denominator() {
        let totals = Totals {
            fills: 3,
            one_sided_fills: 1,
            unrecovered_one_sided: 1,
            ..Totals::default()
        };
        let metrics = winner_metrics_report(&totals);
        assert_eq!(metrics.one_sided_rate_pct, "25.00");
        assert_eq!(metrics.unrecovered_one_sided_rate_pct, "25.00");
    }
}
