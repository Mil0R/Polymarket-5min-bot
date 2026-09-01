//! Stage-1 replay for short-duration crypto Up/Down contracts treated as
//! cash-or-nothing digital options. This module is deliberately offline: it
//! opens only the PolyBacktest SQLite database and cannot reach a signer or a
//! trading client.

use anyhow::{bail, Context, Result};
use chrono::Utc;
use futures::StreamExt;
use rusqlite::{Connection, Row};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::time::Instant;
use tokio::time::{sleep, timeout};
use tracing::{info, warn};

use crate::late_resolution_winner_model::{
    run_chainlink_twap_feed, ChainlinkTwapStore, FairValueSnapshotReadiness, FairValueTwapSnapshot,
};
use crate::market::{MarketDiscoverer, MarketInfo};
use crate::monitor::OrderBookMonitor;
use crate::runtime_metrics::RuntimeMetrics;
use crate::trading::executor::WinnerBookTop;

#[derive(Debug, Clone)]
struct Config {
    database: PathBuf,
    output_dir: PathBuf,
    checkpoints_secs: Vec<i64>,
    volatility_lookback_secs: i64,
    min_observations: usize,
    min_volatility: HashMap<String, f64>,
    min_edge: f64,
    min_price: f64,
    max_price: f64,
    order_usdc: Decimal,
    delays_ms: Vec<i64>,
    haircuts: Vec<Decimal>,
    fee_rate: Decimal,
    fee_exponent: u64,
    progress_every: usize,
    market_offset: usize,
    max_markets: usize,
    min_direction_samples: usize,
    min_cost_fills: usize,
}

impl Config {
    fn from_env() -> Result<Self> {
        let _ = dotenvy::dotenv();
        let data_dir = std::env::var("POLYBACKTEST_DATA_DIR")
            .unwrap_or_else(|_| "data/polybacktest".to_string());
        let database = std::env::var_os("SHORT_DURATION_DIGITAL_FAIR_VALUE_DATABASE_FILE")
            .or_else(|| std::env::var_os("BACKTEST_DATABASE_FILE"))
            .or_else(|| std::env::var_os("POLYBACKTEST_DATABASE_FILE"))
            .map(PathBuf::from)
            .unwrap_or_else(|| Path::new(&data_dir).join("polybacktest.sqlite3"));
        let mut min_volatility = HashMap::new();
        min_volatility.insert(
            "btc".into(),
            env_parse(
                "SHORT_DURATION_DIGITAL_FAIR_VALUE_BTC_MIN_VOLATILITY_BPS",
                0.8,
            )?,
        );
        min_volatility.insert(
            "eth".into(),
            env_parse(
                "SHORT_DURATION_DIGITAL_FAIR_VALUE_ETH_MIN_VOLATILITY_BPS",
                1.2,
            )?,
        );
        min_volatility.insert(
            "sol".into(),
            env_parse(
                "SHORT_DURATION_DIGITAL_FAIR_VALUE_SOL_MIN_VOLATILITY_BPS",
                1.5,
            )?,
        );
        let config = Self {
            database,
            output_dir: std::env::var("BACKTEST_OUTPUT_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("results/backtests")),
            checkpoints_secs: env_i64_list(
                "SHORT_DURATION_DIGITAL_FAIR_VALUE_CHECKPOINTS_SECS",
                &[240, 180, 120, 60, 30, 10],
            )?,
            volatility_lookback_secs: env_parse(
                "SHORT_DURATION_DIGITAL_FAIR_VALUE_VOLATILITY_LOOKBACK_SECS",
                120,
            )?,
            min_observations: env_parse("SHORT_DURATION_DIGITAL_FAIR_VALUE_MIN_OBSERVATIONS", 20)?,
            min_volatility,
            min_edge: env_parse("SHORT_DURATION_DIGITAL_FAIR_VALUE_MIN_NET_EDGE", 0.03)?,
            min_price: env_parse("SHORT_DURATION_DIGITAL_FAIR_VALUE_MIN_PRICE", 0.05)?,
            max_price: env_parse("SHORT_DURATION_DIGITAL_FAIR_VALUE_MAX_PRICE", 0.95)?,
            order_usdc: env_decimal("SHORT_DURATION_DIGITAL_FAIR_VALUE_ORDER_USDC", "5")?,
            delays_ms: env_i64_list(
                "SHORT_DURATION_DIGITAL_FAIR_VALUE_BACKTEST_DELAYS_MS",
                &[100, 250, 500, 1_000],
            )?,
            haircuts: env_decimal_list(
                "SHORT_DURATION_DIGITAL_FAIR_VALUE_BACKTEST_LIQUIDITY_HAIRCUTS",
                &["0.50", "0.75", "1.0"],
            )?,
            fee_rate: env_decimal(
                "SHORT_DURATION_DIGITAL_FAIR_VALUE_BACKTEST_FEE_RATE",
                "0.07",
            )?,
            fee_exponent: env_parse("SHORT_DURATION_DIGITAL_FAIR_VALUE_BACKTEST_FEE_EXPONENT", 1)?,
            progress_every: env_parse("BACKTEST_PROGRESS_EVERY_MARKETS", 100)?,
            market_offset: env_parse("BACKTEST_MARKET_OFFSET", 0)?,
            max_markets: env_parse("BACKTEST_MAX_MARKETS", 0)?,
            min_direction_samples: env_parse(
                "SHORT_DURATION_DIGITAL_FAIR_VALUE_MIN_DIRECTION_SAMPLES",
                500,
            )?,
            min_cost_fills: env_parse("SHORT_DURATION_DIGITAL_FAIR_VALUE_MIN_COST_FILLS", 100)?,
        };
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        if !self.database.is_file() {
            bail!(
                "PolyBacktest database not found: {}",
                self.database.display()
            );
        }
        if self.checkpoints_secs.is_empty()
            || self.checkpoints_secs.iter().any(|v| *v <= 0 || *v >= 300)
        {
            bail!("checkpoints must be inside a five-minute market (1..299 seconds)");
        }
        if self.volatility_lookback_secs <= 0 || self.min_observations < 2 {
            bail!("volatility lookback and observation count must be positive");
        }
        if !(0.0..0.5).contains(&self.min_edge)
            || !(0.0..self.max_price).contains(&self.min_price)
            || self.max_price >= 1.0
        {
            bail!("invalid price or edge limits");
        }
        if self.order_usdc < Decimal::ONE
            || self.delays_ms.is_empty()
            || self.delays_ms.iter().any(|v| *v < 0)
            || self.haircuts.is_empty()
            || self
                .haircuts
                .iter()
                .any(|v| *v <= Decimal::ZERO || *v > Decimal::ONE)
        {
            bail!("invalid execution scenarios");
        }
        Ok(())
    }
}

#[derive(Debug)]
struct Market {
    id: String,
    coin: String,
    start_ms: i64,
    end_ms: i64,
    winner_up: bool,
}

#[derive(Debug)]
struct Snapshot {
    time_ms: i64,
    twap: Option<f64>,
    twap_observed_at_ms: Option<i64>,
    twap_source: Option<String>,
    up_bid: Option<f64>,
    up_ask: Option<f64>,
    down_ask: Option<f64>,
    up_book: Option<Vec<u8>>,
    down_book: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Default, Serialize)]
struct ProbabilityMetrics {
    samples: usize,
    accuracy: f64,
    brier_score: f64,
    log_loss: f64,
    market_samples: usize,
    market_brier_score: f64,
    calibration: Vec<CalibrationBin>,
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
struct CalibrationBin {
    lower: f64,
    upper: f64,
    samples: usize,
    average_probability: f64,
    observed_up_rate: f64,
}

#[derive(Default)]
struct MetricAccumulator {
    samples: usize,
    correct: usize,
    brier: f64,
    log_loss: f64,
    market_samples: usize,
    market_brier: f64,
    bin_samples: [usize; 10],
    bin_probability: [f64; 10],
    bin_up: [usize; 10],
}

impl MetricAccumulator {
    fn record(&mut self, probability: f64, winner_up: bool, market_probability: Option<f64>) {
        let y = if winner_up { 1.0 } else { 0.0 };
        let p = probability.clamp(0.000_001, 0.999_999);
        self.samples += 1;
        self.correct += usize::from((p >= 0.5) == winner_up);
        self.brier += (p - y).powi(2);
        self.log_loss += -(y * p.ln() + (1.0 - y) * (1.0 - p).ln());
        let bin = ((p * 10.0).floor() as usize).min(9);
        self.bin_samples[bin] += 1;
        self.bin_probability[bin] += p;
        self.bin_up[bin] += usize::from(winner_up);
        if let Some(market) = market_probability {
            self.market_samples += 1;
            self.market_brier += (market.clamp(0.0, 1.0) - y).powi(2);
        }
    }

    fn report(&self) -> ProbabilityMetrics {
        ProbabilityMetrics {
            samples: self.samples,
            accuracy: ratio(self.correct, self.samples),
            brier_score: divide(self.brier, self.samples),
            log_loss: divide(self.log_loss, self.samples),
            market_samples: self.market_samples,
            market_brier_score: divide(self.market_brier, self.market_samples),
            calibration: (0..10)
                .map(|bin| CalibrationBin {
                    lower: bin as f64 / 10.0,
                    upper: (bin + 1) as f64 / 10.0,
                    samples: self.bin_samples[bin],
                    average_probability: divide(self.bin_probability[bin], self.bin_samples[bin]),
                    observed_up_rate: ratio(self.bin_up[bin], self.bin_samples[bin]),
                })
                .collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
struct ScenarioKey {
    checkpoint_secs: i64,
    execution_delay_ms: i64,
    liquidity_haircut: String,
}

#[derive(Default, Serialize)]
struct ExecutionTotals {
    candidates: usize,
    fills: usize,
    wins: usize,
    losses: usize,
    gross_pnl_usdc: f64,
    fees_usdc: f64,
    net_pnl_usdc: f64,
    validation_fills: usize,
    validation_net_pnl_usdc: f64,
    test_fills: usize,
    test_net_pnl_usdc: f64,
    #[serde(skip)]
    rejections: BTreeMap<String, usize>,
}

#[derive(Serialize)]
struct ScenarioReport {
    checkpoint_secs: i64,
    execution_delay_ms: i64,
    liquidity_haircut: String,
    candidates: usize,
    fills: usize,
    wins: usize,
    losses: usize,
    win_rate: f64,
    gross_pnl_usdc: f64,
    fees_usdc: f64,
    net_pnl_usdc: f64,
    average_net_pnl_usdc: f64,
    validation_fills: usize,
    validation_net_pnl_usdc: f64,
    test_fills: usize,
    test_net_pnl_usdc: f64,
    rejections: BTreeMap<String, usize>,
}

#[derive(Serialize)]
struct Report {
    strategy: &'static str,
    generated_at: String,
    database: String,
    markets: usize,
    markets_with_valid_predictions: usize,
    resolution_reference_samples: usize,
    resolution_reference_matches: usize,
    resolution_reference_match_rate: f64,
    parameters: Value,
    direction_overall: ProbabilityMetrics,
    direction_by_checkpoint: BTreeMap<i64, ProbabilityMetrics>,
    direction_by_asset: BTreeMap<String, ProbabilityMetrics>,
    direction_by_split: BTreeMap<String, ProbabilityMetrics>,
    execution_scenarios: Vec<ScenarioReport>,
    readiness: String,
    direction_ready: bool,
    conservative_cost_ready: bool,
    limitations: Vec<&'static str>,
}

const LIVE_TARGET: &str = "polypulse::strategy::short_duration_digital_fair_value";

#[derive(Clone)]
struct DryRunConfig {
    symbols: Vec<String>,
    checkpoint_secs: i64,
    scan_interval_ms: u64,
    feed_ready_timeout_secs: u64,
    rest_fallback_cooldown_ms: i64,
    execution_delay_ms: u64,
    max_book_age_ms: i64,
    max_feed_age_ms: i64,
    opening_max_skew_ms: i64,
    volatility_lookback_secs: u64,
    min_observations: usize,
    min_volatility: HashMap<String, f64>,
    min_edge: f64,
    max_spread: f64,
    min_depth_usdc: f64,
    min_price: f64,
    max_price: f64,
    order_usdc: f64,
    liquidity_haircut: f64,
    fee_rate: f64,
    fee_exponent: u32,
    rtds_url: String,
    clob_url: String,
    results_file: PathBuf,
}

impl DryRunConfig {
    fn from_env() -> Result<Self> {
        let _ = dotenvy::dotenv();
        let symbols = std::env::var("SHORT_DURATION_DIGITAL_FAIR_VALUE_SYMBOLS")
            .or_else(|_| std::env::var("CRYPTO_SYMBOLS"))
            .unwrap_or_else(|_| "btc,eth,sol".to_string())
            .split(',')
            .map(|value| value.trim().to_ascii_lowercase())
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>();
        let mut min_volatility = HashMap::new();
        min_volatility.insert(
            "btc".to_string(),
            env_parse(
                "SHORT_DURATION_DIGITAL_FAIR_VALUE_BTC_MIN_VOLATILITY_BPS",
                0.8,
            )?,
        );
        min_volatility.insert(
            "eth".to_string(),
            env_parse(
                "SHORT_DURATION_DIGITAL_FAIR_VALUE_ETH_MIN_VOLATILITY_BPS",
                1.2,
            )?,
        );
        min_volatility.insert(
            "sol".to_string(),
            env_parse(
                "SHORT_DURATION_DIGITAL_FAIR_VALUE_SOL_MIN_VOLATILITY_BPS",
                1.5,
            )?,
        );
        let config = Self {
            symbols,
            checkpoint_secs: env_parse(
                "SHORT_DURATION_DIGITAL_FAIR_VALUE_DRY_RUN_CHECKPOINT_SECS",
                10,
            )?,
            scan_interval_ms: env_parse(
                "SHORT_DURATION_DIGITAL_FAIR_VALUE_DRY_RUN_SCAN_INTERVAL_MS",
                100,
            )?,
            feed_ready_timeout_secs: env_parse(
                "SHORT_DURATION_DIGITAL_FAIR_VALUE_FEED_READY_TIMEOUT_SECS",
                15,
            )?,
            rest_fallback_cooldown_ms: env_parse(
                "SHORT_DURATION_DIGITAL_FAIR_VALUE_REST_FALLBACK_COOLDOWN_MS",
                1_000,
            )?,
            execution_delay_ms: env_parse(
                "SHORT_DURATION_DIGITAL_FAIR_VALUE_DRY_RUN_EXECUTION_DELAY_MS",
                1_000,
            )?,
            max_book_age_ms: env_parse("SHORT_DURATION_DIGITAL_FAIR_VALUE_MAX_BOOK_AGE_MS", 1_000)?,
            max_feed_age_ms: env_parse("SHORT_DURATION_DIGITAL_FAIR_VALUE_FEED_MAX_AGE_MS", 5_000)?,
            opening_max_skew_ms: env_parse(
                "SHORT_DURATION_DIGITAL_FAIR_VALUE_OPENING_MAX_SKEW_MS",
                5_000,
            )?,
            volatility_lookback_secs: env_parse(
                "SHORT_DURATION_DIGITAL_FAIR_VALUE_VOLATILITY_LOOKBACK_SECS",
                120,
            )?,
            min_observations: env_parse("SHORT_DURATION_DIGITAL_FAIR_VALUE_MIN_OBSERVATIONS", 20)?,
            min_volatility,
            min_edge: env_parse("SHORT_DURATION_DIGITAL_FAIR_VALUE_MIN_NET_EDGE", 0.03)?,
            max_spread: env_parse("SHORT_DURATION_DIGITAL_FAIR_VALUE_MAX_SPREAD", 0.03)?,
            min_depth_usdc: env_parse("SHORT_DURATION_DIGITAL_FAIR_VALUE_MIN_DEPTH_USDC", 10.0)?,
            min_price: env_parse("SHORT_DURATION_DIGITAL_FAIR_VALUE_MIN_PRICE", 0.05)?,
            max_price: env_parse("SHORT_DURATION_DIGITAL_FAIR_VALUE_MAX_PRICE", 0.95)?,
            order_usdc: env_parse("SHORT_DURATION_DIGITAL_FAIR_VALUE_ORDER_USDC", 5.0)?,
            liquidity_haircut: env_parse(
                "SHORT_DURATION_DIGITAL_FAIR_VALUE_DRY_RUN_LIQUIDITY_HAIRCUT",
                0.50,
            )?,
            fee_rate: env_parse("SHORT_DURATION_DIGITAL_FAIR_VALUE_DRY_RUN_FEE_RATE", 0.07)?,
            fee_exponent: env_parse("SHORT_DURATION_DIGITAL_FAIR_VALUE_DRY_RUN_FEE_EXPONENT", 1)?,
            rtds_url: std::env::var("SHORT_DURATION_DIGITAL_FAIR_VALUE_RTDS_URL")
                .unwrap_or_else(|_| "wss://ws-live-data.polymarket.com".to_string()),
            clob_url: std::env::var("CLOB_API_URL")
                .unwrap_or_else(|_| "https://clob.polymarket.com".to_string()),
            results_file: std::env::var("SHORT_DURATION_DIGITAL_FAIR_VALUE_RESULTS_FILE")
                .map(PathBuf::from)
                .unwrap_or_else(|_| {
                    PathBuf::from("results/short-duration-digital-fair-value/paper/events.ndjson")
                }),
        };
        if config.symbols.is_empty()
            || config.checkpoint_secs != 10
            || config.scan_interval_ms < 25
            || config.feed_ready_timeout_secs == 0
            || config.feed_ready_timeout_secs > 120
            || config.rest_fallback_cooldown_ms < 250
            || config.rest_fallback_cooldown_ms > 10_000
            || config.execution_delay_ms > 30_000
            || config.max_book_age_ms <= 0
            || config.max_feed_age_ms <= 0
            || config.opening_max_skew_ms < 0
            || !(0.0..1.0).contains(&config.liquidity_haircut)
            || config.liquidity_haircut == 0.0
            || config.order_usdc < 1.0
        {
            bail!("invalid dry-run configuration; only the backtest-approved 10-second checkpoint is enabled");
        }
        Ok(config)
    }
}

#[derive(Debug, Deserialize)]
struct LiveRestBook {
    #[serde(default)]
    bids: Vec<LiveRestLevel>,
    #[serde(default)]
    asks: Vec<LiveRestLevel>,
}

#[derive(Debug, Deserialize)]
struct LiveRestLevel {
    price: String,
    size: String,
}

#[derive(Debug, Clone, Copy)]
struct LiveBookTop {
    bid: Option<f64>,
    ask: Option<f64>,
    ask_size: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize)]
struct DryRunWindowStats {
    scan_ticks: u64,
    market_evaluations: u64,
    opportunities: u64,
    shadow_rejections: u64,
    shadow_fills: u64,
    rest_fallback_attempts: u64,
    rest_fallback_successes: u64,
    rest_latency_samples: u64,
    rest_latency_total_ms: u64,
    rest_latency_max_ms: u64,
    #[serde(default)]
    websocket_stream_errors: u64,
    #[serde(default)]
    rejection_counts: BTreeMap<String, u64>,
    #[serde(default)]
    terminal_reason_by_market: BTreeMap<String, String>,
}

impl DryRunWindowStats {
    fn reject(&mut self, slug: &str, reason: &str) {
        *self.rejection_counts.entry(reason.to_string()).or_default() += 1;
        self.terminal_reason_by_market
            .insert(slug.to_string(), reason.to_string());
    }

    fn record_rest_latency(&mut self, latency_ms: u64) {
        self.rest_latency_samples = self.rest_latency_samples.saturating_add(1);
        self.rest_latency_total_ms = self.rest_latency_total_ms.saturating_add(latency_ms);
        self.rest_latency_max_ms = self.rest_latency_max_ms.max(latency_ms);
    }

    fn average_rest_latency_ms(&self) -> f64 {
        if self.rest_latency_samples == 0 {
            0.0
        } else {
            self.rest_latency_total_ms as f64 / self.rest_latency_samples as f64
        }
    }
}

#[derive(Clone)]
struct EventJournal {
    file: Arc<Mutex<File>>,
    run_id: String,
}

impl EventJournal {
    fn open(path: &Path, run_id: String) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        Ok(Self {
            file: Arc::new(Mutex::new(
                OpenOptions::new().create(true).append(true).open(path)?,
            )),
            run_id,
        })
    }

    fn append(&self, mut value: Value) -> Result<()> {
        if let Some(object) = value.as_object_mut() {
            object
                .entry("run_id".to_string())
                .or_insert_with(|| Value::String(self.run_id.clone()));
        }
        let mut file = self
            .file
            .lock()
            .map_err(|_| anyhow::anyhow!("journal lock poisoned"))?;
        serde_json::to_writer(&mut *file, &value)?;
        writeln!(file)?;
        file.flush()?;
        Ok(())
    }
}

/// Forced-safe stage-2 runner. It uses public Gamma, RTDS and CLOB market-data
/// endpoints only; no private key, API credentials, signer or trading client is
/// constructed anywhere in this path.
pub async fn run_dry_run() -> Result<()> {
    let config = DryRunConfig::from_env()?;
    let run_started_ms = Utc::now().timestamp_millis();
    let run_id = uuid::Uuid::new_v4().to_string();
    std::env::set_var("DRY_RUN", "true");
    std::env::set_var(
        "LOG_FILE",
        std::env::var("SHORT_DURATION_DIGITAL_FAIR_VALUE_LOG_FILE")
            .unwrap_or_else(|_| "logs/short-duration-digital-fair-value.log".to_string()),
    );
    if std::env::var_os("LOG_STDOUT_TEE").is_none() {
        std::env::set_var("LOG_STDOUT_TEE", "true");
    }
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();
    crate::utils::logger::init_logger(false)?;
    let journal = EventJournal::open(&config.results_file, run_id.clone())?;
    journal.append(serde_json::json!({
        "event": "run_started",
        "recorded_at": Utc::now(),
        "strategy": "short-duration-digital-fair-value",
        "config": {
            "symbols": config.symbols.clone(),
            "checkpoint_secs": config.checkpoint_secs,
            "scan_interval_ms": config.scan_interval_ms,
            "execution_delay_ms": config.execution_delay_ms,
            "liquidity_haircut": config.liquidity_haircut,
            "min_edge": config.min_edge,
            "order_usdc": config.order_usdc,
            "max_book_age_ms": config.max_book_age_ms,
            "max_feed_age_ms": config.max_feed_age_ms
        }
    }))?;
    let shutdown = Arc::new(AtomicBool::new(false));
    let signal_shutdown = shutdown.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        signal_shutdown.store(true, Ordering::Relaxed);
    });
    let store = Arc::new(ChainlinkTwapStore::default());
    tokio::spawn(run_chainlink_twap_feed(
        config.rtds_url.clone(),
        store.clone(),
        shutdown.clone(),
        Arc::new(RuntimeMetrics::default()),
    ));
    wait_for_feed_readiness(&config, &store, &shutdown).await;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;
    let run_result = async {
    info!(target: LIVE_TARGET,
        symbols = ?config.symbols,
        checkpoint_secs = config.checkpoint_secs,
        execution_delay_ms = config.execution_delay_ms,
        liquidity_haircut = config.liquidity_haircut,
        fee_rate_assumption = config.fee_rate,
        results_file = %config.results_file.display(),
        "Short-Duration Digital Fair Value dry-run started (public read-only data)"
    );
    while !shutdown.load(Ordering::Relaxed) {
        let discoverer = MarketDiscoverer::new(config.symbols.clone(), 5);
        let now = Utc::now();
        let window_start = discoverer.calculate_current_window_timestamp(now);
        let window_end_ms = (window_start + 300) * 1_000;
        let markets = discoverer.get_markets_for_timestamp(window_start).await?;
        if markets.is_empty() {
            sleep(Duration::from_secs(2)).await;
            continue;
        }
        let mut monitor = OrderBookMonitor::new();
        for market in &markets {
            monitor.subscribe_market(market)?;
        }
        let mut stream = monitor.create_orderbook_stream()?;
        let mut stream_reconnect_attempt = 0u32;
        let attempted = Arc::new(Mutex::new(std::collections::HashSet::new()));
        let stats = Arc::new(Mutex::new(DryRunWindowStats::default()));
        let mut rest_fallback_last_ms = HashMap::new();
        let mut ticker = tokio::time::interval(Duration::from_millis(config.scan_interval_ms));
        info!(target: LIVE_TARGET, window_start, markets = markets.len(), "Monitoring live window");
        if run_started_ms > window_start * 1_000 + config.opening_max_skew_ms {
            warn!(target: LIVE_TARGET,
                window_start,
                next_window_start = window_start + 300,
                "Current window started before the process could capture its opening TWAP; warming up until the next full window"
            );
            if let Ok(mut guard) = stats.lock() {
                for market in &markets {
                    guard.reject(&market.slug, "startup_window_missing_opening_reference");
                }
            }
        }
        loop {
            if shutdown.load(Ordering::Relaxed) || Utc::now().timestamp_millis() >= window_end_ms {
                break;
            }
            tokio::select! {
                update = stream.next() => {
                    match update {
                        Some(Ok(book)) => {
                            stream_reconnect_attempt = 0;
                            monitor.handle_book_update(book);
                        }
                        Some(Err(error)) => {
                            if let Ok(mut guard) = stats.lock() {
                                guard.websocket_stream_errors = guard.websocket_stream_errors.saturating_add(1);
                            }
                            stream_reconnect_attempt = stream_reconnect_attempt.saturating_add(1);
                            let delay_ms = (250u64.saturating_mul(1u64 << stream_reconnect_attempt.min(5))).min(8_000);
                            warn!(target: LIVE_TARGET, %error, attempt = stream_reconnect_attempt, delay_ms,
                                "Order-book stream error; reconnecting within current window");
                            monitor.invalidate_books();
                            sleep(Duration::from_millis(delay_ms)).await;
                            match monitor.create_orderbook_stream() {
                                Ok(new_stream) => {
                                    stream = new_stream;
                                    continue;
                                }
                                Err(reconnect_error) => {
                                    warn!(target: LIVE_TARGET, %reconnect_error,
                                        "Order-book stream reconnect failed; ending current window");
                                    break;
                                }
                            }
                        }
                        None => {
                            if let Ok(mut guard) = stats.lock() {
                                guard.websocket_stream_errors = guard.websocket_stream_errors.saturating_add(1);
                            }
                            stream_reconnect_attempt = stream_reconnect_attempt.saturating_add(1);
                            let delay_ms = (250u64.saturating_mul(1u64 << stream_reconnect_attempt.min(5))).min(8_000);
                            warn!(target: LIVE_TARGET, attempt = stream_reconnect_attempt, delay_ms,
                                "Order-book stream ended; reconnecting within current window");
                            monitor.invalidate_books();
                            sleep(Duration::from_millis(delay_ms)).await;
                            match monitor.create_orderbook_stream() {
                                Ok(new_stream) => {
                                    stream = new_stream;
                                    continue;
                                }
                                Err(reconnect_error) => {
                                    warn!(target: LIVE_TARGET, %reconnect_error,
                                        "Order-book stream reconnect failed; ending current window");
                                    break;
                                }
                            }
                        }
                    }
                }
                _ = ticker.tick() => {
                    scan_live_markets(
                        &config,
                        &markets,
                        window_start * 1_000,
                        window_end_ms,
                        &store,
                        &monitor,
                        &client,
                        &journal,
                        &attempted,
                        &stats,
                        &mut rest_fallback_last_ms,
                    );
                }
            }
        }
        if shutdown.load(Ordering::Relaxed) {
            finalize_window_summary(
                journal.clone(),
                stats,
                window_start * 1_000,
                window_end_ms,
                markets.len(),
                0,
            )
            .await;
        } else {
            let summary_delay_ms = config.execution_delay_ms.saturating_add(6_500);
            tokio::spawn(finalize_window_summary(
                journal.clone(),
                stats,
                window_start * 1_000,
                window_end_ms,
                markets.len(),
                summary_delay_ms,
            ));
        }
        if !shutdown.load(Ordering::Relaxed) {
            sleep(Duration::from_millis(250)).await;
        }
    }
    Ok::<(), anyhow::Error>(())
    }.await;
    let stop_reason = run_result.as_ref().err().map(ToString::to_string);
    let _ = journal.append(serde_json::json!({
        "event": "run_stopped",
        "recorded_at": Utc::now(),
        "started_at_ms": run_started_ms,
        "stop_reason": stop_reason,
        "normal": run_result.is_ok()
    }));
    info!(target: LIVE_TARGET, normal = run_result.is_ok(), "Short-Duration Digital Fair Value dry-run stopped");
    run_result
}

async fn wait_for_feed_readiness(
    config: &DryRunConfig,
    store: &Arc<ChainlinkTwapStore>,
    shutdown: &Arc<AtomicBool>,
) {
    let started = Instant::now();
    let timeout_duration = Duration::from_secs(config.feed_ready_timeout_secs);
    loop {
        let now_ms = Utc::now().timestamp_millis();
        let ready = config.symbols.iter().all(|symbol| {
            store
                .latest_observed_at_ms(symbol)
                .is_some_and(|observed| now_ms.saturating_sub(observed) <= config.max_feed_age_ms)
        });
        if ready {
            info!(target: LIVE_TARGET,
                symbols = ?config.symbols,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "Chainlink TWAP feed ready for fair-value dry-run"
            );
            return;
        }
        if shutdown.load(Ordering::Relaxed) {
            return;
        }
        if started.elapsed() >= timeout_duration {
            warn!(target: LIVE_TARGET,
                symbols = ?config.symbols,
                timeout_secs = config.feed_ready_timeout_secs,
                "Chainlink TWAP feed did not become ready before timeout; continuing fail-closed"
            );
            return;
        }
        sleep(Duration::from_millis(250)).await;
    }
}

#[allow(clippy::too_many_arguments)]
fn scan_live_markets(
    config: &DryRunConfig,
    markets: &[MarketInfo],
    window_start_ms: i64,
    window_end_ms: i64,
    store: &Arc<ChainlinkTwapStore>,
    monitor: &OrderBookMonitor,
    client: &reqwest::Client,
    journal: &EventJournal,
    attempted: &Arc<Mutex<std::collections::HashSet<String>>>,
    stats: &Arc<Mutex<DryRunWindowStats>>,
    rest_fallback_last_ms: &mut HashMap<String, i64>,
) {
    let now_ms = Utc::now().timestamp_millis();
    let remaining_ms = window_end_ms - now_ms;
    if remaining_ms <= 0 || remaining_ms > config.checkpoint_secs * 1_000 {
        return;
    }
    if let Ok(mut guard) = stats.lock() {
        guard.scan_ticks = guard.scan_ticks.saturating_add(1);
    }
    for market in markets {
        if attempted
            .lock()
            .map(|guard| guard.contains(&market.slug))
            .unwrap_or(true)
        {
            continue;
        }
        if let Ok(mut guard) = stats.lock() {
            guard.market_evaluations = guard.market_evaluations.saturating_add(1);
        }
        let floor = config
            .min_volatility
            .get(&market.crypto_symbol)
            .copied()
            .unwrap_or(1.5);
        let readiness = store.fair_value_snapshot_readiness(
            &market.crypto_symbol,
            window_start_ms,
            now_ms,
            config.opening_max_skew_ms,
            config.max_feed_age_ms,
            config.volatility_lookback_secs,
            config.min_observations,
            floor,
        );
        let state = match readiness {
            FairValueSnapshotReadiness::Ready(state) => state,
            rejected => {
                record_window_rejection(
                    stats,
                    &market.slug,
                    rejected.reason().unwrap_or("model_not_ready"),
                );
                continue;
            }
        };
        let up = monitor.winner_book_top(market.yes_token_id);
        let down = monitor.winner_book_top(market.no_token_id);
        let ws_usable = up
            .as_ref()
            .is_some_and(|top| live_ws_top_usable(top, now_ms, config))
            && down
                .as_ref()
                .is_some_and(|top| live_ws_top_usable(top, now_ms, config));
        if !ws_usable {
            let last = rest_fallback_last_ms
                .get(&market.slug)
                .copied()
                .unwrap_or(i64::MIN);
            if now_ms.saturating_sub(last) < config.rest_fallback_cooldown_ms {
                continue;
            }
            rest_fallback_last_ms.insert(market.slug.clone(), now_ms);
            if let Ok(mut guard) = stats.lock() {
                guard.rest_fallback_attempts = guard.rest_fallback_attempts.saturating_add(1);
            }
            tokio::spawn(evaluate_rest_fallback(
                config.clone(),
                client.clone(),
                journal.clone(),
                stats.clone(),
                attempted.clone(),
                market.clone(),
                state,
                remaining_ms,
                window_end_ms,
            ));
            continue;
        }
        let up = live_top_from_ws(up.expect("checked usable WebSocket UP book"));
        let down = live_top_from_ws(down.expect("checked usable WebSocket DOWN book"));
        let probability_up = live_probability(state, remaining_ms as f64 / 1_000.0);
        evaluate_live_tops(
            config,
            market,
            state,
            probability_up,
            up,
            down,
            "websocket",
            0,
            remaining_ms,
            window_start_ms,
            window_end_ms,
            client,
            journal,
            attempted,
            stats,
        );
    }
}

fn record_window_rejection(stats: &Arc<Mutex<DryRunWindowStats>>, slug: &str, reason: &str) {
    if let Ok(mut guard) = stats.lock() {
        guard.reject(slug, reason);
    }
}

fn live_ws_top_usable(top: &WinnerBookTop, now_ms: i64, config: &DryRunConfig) -> bool {
    now_ms.saturating_sub(top.received_at_ms) <= config.max_book_age_ms
        && top.bid_price.is_some()
        && top.ask_price.is_some()
        && top.ask_size.is_some()
}

fn live_top_from_ws(top: WinnerBookTop) -> LiveBookTop {
    LiveBookTop {
        bid: top.bid_price.and_then(|value| value.to_f64()),
        ask: top.ask_price.and_then(|value| value.to_f64()),
        ask_size: top.ask_size.and_then(|value| value.to_f64()),
    }
}

async fn evaluate_rest_fallback(
    config: DryRunConfig,
    client: reqwest::Client,
    journal: EventJournal,
    stats: Arc<Mutex<DryRunWindowStats>>,
    attempted: Arc<Mutex<std::collections::HashSet<String>>>,
    market: MarketInfo,
    state: FairValueTwapSnapshot,
    remaining_ms: i64,
    window_end_ms: i64,
) {
    let up_token_id = market.yes_token_id.to_string();
    let down_token_id = market.no_token_id.to_string();
    let (up_result, down_result) = tokio::join!(
        fetch_live_rest_top(&config, &client, &up_token_id),
        fetch_live_rest_top(&config, &client, &down_token_id),
    );
    let (up, up_latency_ms) = match up_result {
        Ok(value) => value,
        Err(error) => {
            record_window_rejection(&stats, &market.slug, "rest_up_book_failed");
            warn!(target: LIVE_TARGET, slug = %market.slug, %error, "UP REST book fallback failed");
            return;
        }
    };
    let (down, down_latency_ms) = match down_result {
        Ok(value) => value,
        Err(error) => {
            record_window_rejection(&stats, &market.slug, "rest_down_book_failed");
            warn!(target: LIVE_TARGET, slug = %market.slug, %error, "DOWN REST book fallback failed");
            return;
        }
    };
    let rest_latency_ms = up_latency_ms.max(down_latency_ms);
    if let Ok(mut guard) = stats.lock() {
        guard.rest_fallback_successes = guard.rest_fallback_successes.saturating_add(1);
        guard.record_rest_latency(up_latency_ms);
        guard.record_rest_latency(down_latency_ms);
    }
    let now_ms = Utc::now().timestamp_millis();
    let adjusted_remaining_ms = window_end_ms.saturating_sub(now_ms).min(remaining_ms);
    if adjusted_remaining_ms <= 0 {
        record_window_rejection(&stats, &market.slug, "rest_fallback_completed_after_window");
        return;
    }
    let probability_up = live_probability(state, adjusted_remaining_ms as f64 / 1_000.0);
    evaluate_live_tops(
        &config,
        &market,
        state,
        probability_up,
        up,
        down,
        "rest_fallback",
        rest_latency_ms,
        adjusted_remaining_ms,
        window_end_ms - 300_000,
        window_end_ms,
        &client,
        &journal,
        &attempted,
        &stats,
    );
}

async fn fetch_live_rest_top(
    config: &DryRunConfig,
    client: &reqwest::Client,
    token_id: &str,
) -> Result<(LiveBookTop, u64)> {
    let started = Instant::now();
    let response = timeout(
        Duration::from_secs(6),
        client
            .get(format!("{}/book", config.clob_url.trim_end_matches('/')))
            .query(&[("token_id", token_id)])
            .send(),
    )
    .await
    .context("REST book fallback timed out")??;
    let status = response.status();
    if !status.is_success() {
        bail!("REST book fallback returned HTTP {status}");
    }
    let book = response.json::<LiveRestBook>().await?;
    let best_bid = book
        .bids
        .iter()
        .filter_map(|level| {
            Some((
                level.price.parse::<f64>().ok()?,
                level.size.parse::<f64>().ok()?,
            ))
        })
        .filter(|(_, size)| *size > 0.0)
        .max_by(|a, b| a.0.total_cmp(&b.0));
    let best_ask = book
        .asks
        .iter()
        .filter_map(|level| {
            Some((
                level.price.parse::<f64>().ok()?,
                level.size.parse::<f64>().ok()?,
            ))
        })
        .filter(|(_, size)| *size > 0.0)
        .min_by(|a, b| a.0.total_cmp(&b.0));
    Ok((
        LiveBookTop {
            bid: best_bid.map(|level| level.0),
            ask: best_ask.map(|level| level.0),
            ask_size: best_ask.map(|level| level.1),
        },
        started.elapsed().as_millis() as u64,
    ))
}

#[allow(clippy::too_many_arguments)]
fn evaluate_live_tops(
    config: &DryRunConfig,
    market: &MarketInfo,
    state: FairValueTwapSnapshot,
    probability_up: f64,
    up: LiveBookTop,
    down: LiveBookTop,
    book_source: &'static str,
    book_latency_ms: u64,
    remaining_ms: i64,
    window_start_ms: i64,
    window_end_ms: i64,
    client: &reqwest::Client,
    journal: &EventJournal,
    attempted: &Arc<Mutex<std::collections::HashSet<String>>>,
    stats: &Arc<Mutex<DryRunWindowStats>>,
) {
    let up_evaluation = live_edge_evaluation(config, probability_up, up.bid, up.ask, up.ask_size);
    let down_evaluation = live_edge_evaluation(
        config,
        1.0 - probability_up,
        down.bid,
        down.ask,
        down.ask_size,
    );
    let up_edge = up_evaluation.unwrap_or(f64::NEG_INFINITY);
    let down_edge = down_evaluation.unwrap_or(f64::NEG_INFINITY);
    let choice = if up_edge >= down_edge && up_edge >= config.min_edge {
        up.ask
            .map(|ask| (true, probability_up, ask, up_edge, market.yes_token_id))
    } else if down_edge >= config.min_edge {
        down.ask.map(|ask| {
            (
                false,
                1.0 - probability_up,
                ask,
                down_edge,
                market.no_token_id,
            )
        })
    } else {
        None
    };
    let Some((buy_up, probability, ask, edge, token)) = choice else {
        if let Ok(mut guard) = stats.lock() {
            let terminal_reason = if up_edge.is_finite() || down_edge.is_finite() {
                *guard
                    .rejection_counts
                    .entry("net_edge_below_threshold".to_string())
                    .or_default() += 1;
                "net_edge_below_threshold".to_string()
            } else {
                if let Err(reason) = up_evaluation {
                    *guard
                        .rejection_counts
                        .entry(format!("up_{reason}"))
                        .or_default() += 1;
                }
                if let Err(reason) = down_evaluation {
                    *guard
                        .rejection_counts
                        .entry(format!("down_{reason}"))
                        .or_default() += 1;
                }
                format!(
                    "book_filters:up={},down={}",
                    up_evaluation.err().unwrap_or("admissible"),
                    down_evaluation.err().unwrap_or("admissible")
                )
            };
            guard
                .terminal_reason_by_market
                .insert(market.slug.clone(), terminal_reason);
        }
        return;
    };
    let inserted = attempted
        .lock()
        .map(|mut guard| guard.insert(market.slug.clone()))
        .unwrap_or(false);
    if !inserted {
        return;
    }
    let evaluation_id = uuid::Uuid::new_v4().to_string();
    let event = serde_json::json!({
        "event":"opportunity",
        "evaluation_id":evaluation_id,
        "recorded_at":Utc::now(),
        "asset":market.crypto_symbol,
        "slug":market.slug,
        "window_start_ms":window_start_ms,
        "window_end_ms":window_end_ms,
        "seconds_remaining":remaining_ms as f64 / 1000.0,
        "book_source":book_source,
        "book_latency_ms":book_latency_ms,
        "buy_up":buy_up,
        "opening_twap":state.opening,
        "current_twap":state.current,
        "twap_observed_at_ms":state.observed_at_ms,
        "volatility_bps_per_sqrt_second":state.volatility_bps_per_sqrt_second,
        "probability":probability,
        "signal_ask":ask,
        "net_edge_before_delay":edge,
    });
    if let Err(error) = journal.append(event) {
        warn!(target: LIVE_TARGET, %error, "Could not persist opportunity");
        return;
    }
    if let Ok(mut guard) = stats.lock() {
        guard.opportunities = guard.opportunities.saturating_add(1);
        guard
            .terminal_reason_by_market
            .insert(market.slug.clone(), "opportunity".to_string());
    }
    info!(target: LIVE_TARGET,
        asset = %market.crypto_symbol,
        slug = %market.slug,
        book_source,
        book_latency_ms,
        buy_up,
        probability,
        ask,
        net_edge = edge,
        seconds_remaining = remaining_ms as f64 / 1000.0,
        "Fair-value opportunity"
    );
    tokio::spawn(execute_live_shadow(
        config.clone(),
        client.clone(),
        journal.clone(),
        stats.clone(),
        evaluation_id,
        market.slug.clone(),
        market.crypto_symbol.clone(),
        token.to_string(),
        buy_up,
        probability,
        ask,
        edge,
        book_source,
        book_latency_ms,
        remaining_ms as f64 / 1_000.0,
        window_end_ms,
    ));
}

async fn finalize_window_summary(
    journal: EventJournal,
    stats: Arc<Mutex<DryRunWindowStats>>,
    window_start_ms: i64,
    window_end_ms: i64,
    markets: usize,
    delay_ms: u64,
) {
    if delay_ms > 0 {
        sleep(Duration::from_millis(delay_ms)).await;
    }
    let snapshot = stats.lock().map(|guard| guard.clone()).unwrap_or_default();
    let average_rest_latency_ms = snapshot.average_rest_latency_ms();
    let event = serde_json::json!({
        "event":"window_summary",
        "recorded_at":Utc::now(),
        "window_start_ms":window_start_ms,
        "window_end_ms":window_end_ms,
        "markets":markets,
        "average_rest_latency_ms":average_rest_latency_ms,
        "stats":snapshot,
    });
    if let Err(error) = journal.append(event) {
        warn!(target: LIVE_TARGET, %error, "Could not persist fair-value window summary");
        return;
    }
    info!(target: LIVE_TARGET,
        window_start_ms,
        markets,
        scan_ticks = snapshot.scan_ticks,
        market_evaluations = snapshot.market_evaluations,
        opportunities = snapshot.opportunities,
        shadow_rejections = snapshot.shadow_rejections,
        shadow_fills = snapshot.shadow_fills,
        websocket_stream_errors = snapshot.websocket_stream_errors,
        rest_fallback_attempts = snapshot.rest_fallback_attempts,
        rest_fallback_successes = snapshot.rest_fallback_successes,
        average_rest_latency_ms,
        rest_latency_max_ms = snapshot.rest_latency_max_ms,
        rejection_counts = ?snapshot.rejection_counts,
        terminal_reason_by_market = ?snapshot.terminal_reason_by_market,
        "Fair-value dry-run window summary"
    );
}

fn live_probability(state: FairValueTwapSnapshot, remaining_secs: f64) -> f64 {
    fair_up_probability(
        state.opening.to_f64().unwrap_or(0.0),
        state.current.to_f64().unwrap_or(0.0),
        remaining_secs,
        state.volatility_bps_per_sqrt_second,
    )
}

fn live_edge_evaluation(
    config: &DryRunConfig,
    probability: f64,
    bid: Option<f64>,
    ask: Option<f64>,
    ask_size: Option<f64>,
) -> std::result::Result<f64, &'static str> {
    let (Some(bid), Some(ask), Some(size)) = (bid, ask, ask_size) else {
        return Err("incomplete_book");
    };
    if ask < config.min_price || ask > config.max_price {
        return Err("price_out_of_range");
    }
    if ask < bid {
        return Err("crossed_book");
    }
    if ask - bid > config.max_spread {
        return Err("spread_too_wide");
    }
    if ask * size < config.min_depth_usdc {
        return Err("insufficient_depth");
    }
    Ok(probability - ask - live_fee(config, ask))
}

async fn execute_live_shadow(
    config: DryRunConfig,
    client: reqwest::Client,
    journal: EventJournal,
    stats: Arc<Mutex<DryRunWindowStats>>,
    evaluation_id: String,
    slug: String,
    asset: String,
    token_id: String,
    buy_up: bool,
    probability: f64,
    signal_ask: f64,
    signal_net_edge: f64,
    signal_book_source: &'static str,
    signal_book_latency_ms: u64,
    signal_seconds_remaining: f64,
    window_end_ms: i64,
) {
    sleep(Duration::from_millis(config.execution_delay_ms)).await;
    let started = Instant::now();
    let request = client
        .get(format!("{}/book", config.clob_url.trim_end_matches('/')))
        .query(&[("token_id", token_id.as_str())]);
    let response = timeout(Duration::from_secs(6), request.send()).await;
    let latency_ms = started.elapsed().as_millis() as u64;
    let book = match response {
        Ok(Ok(response)) if response.status().is_success() => {
            response.json::<LiveRestBook>().await.ok()
        }
        _ => None,
    };
    let Some(book) = book else {
        if let Ok(mut guard) = stats.lock() {
            guard.shadow_rejections = guard.shadow_rejections.saturating_add(1);
            guard.record_rest_latency(latency_ms);
            guard.reject(&slug, "rest_book_failed_after_signal");
        }
        let _ = journal.append(serde_json::json!({
            "event":"shadow_rejected","evaluation_id":evaluation_id,"recorded_at":Utc::now(),
            "slug":slug,"reason":"rest_book_failed","rest_latency_ms":latency_ms,
            "signal_ask":signal_ask,"signal_net_edge":signal_net_edge,
            "signal_book_source":signal_book_source,"signal_book_latency_ms":signal_book_latency_ms,
            "signal_seconds_remaining":signal_seconds_remaining,"delay_ms":config.execution_delay_ms
        }));
        return;
    };
    let mut asks = book
        .asks
        .iter()
        .filter_map(|level| {
            Some((
                level.price.parse::<f64>().ok()?,
                level.size.parse::<f64>().ok()?,
            ))
        })
        .collect::<Vec<_>>();
    asks.sort_by(|a, b| a.0.total_cmp(&b.0));
    let signal_price = asks.first().map(|level| level.0).unwrap_or(1.0);
    let target = config.order_usdc / (signal_price + live_fee(&config, signal_price)).max(0.01);
    let Some(cost) = walk_ask_levels(&asks, target, config.liquidity_haircut) else {
        if let Ok(mut guard) = stats.lock() {
            guard.shadow_rejections = guard.shadow_rejections.saturating_add(1);
            guard.record_rest_latency(latency_ms);
            guard.reject(&slug, "delayed_depth_insufficient");
        }
        let _ = journal.append(serde_json::json!({
            "event":"shadow_rejected","evaluation_id":evaluation_id,"recorded_at":Utc::now(),
            "slug":slug,"reason":"delayed_depth_insufficient","rest_latency_ms":latency_ms,
            "signal_ask":signal_ask,"signal_net_edge":signal_net_edge,
            "signal_book_source":signal_book_source,"signal_book_latency_ms":signal_book_latency_ms,
            "signal_seconds_remaining":signal_seconds_remaining,"delay_ms":config.execution_delay_ms,
            "delayed_ask":signal_price
        }));
        return;
    };
    let average = cost / target;
    let fees = live_fee(&config, average) * target;
    let net_edge = probability - average - fees / target;
    if average < config.min_price || average > config.max_price || net_edge < config.min_edge {
        if let Ok(mut guard) = stats.lock() {
            guard.shadow_rejections = guard.shadow_rejections.saturating_add(1);
            guard.record_rest_latency(latency_ms);
            guard.reject(&slug, "delayed_net_edge_below_threshold");
        }
        let _ = journal.append(serde_json::json!({
            "event":"shadow_rejected","evaluation_id":evaluation_id,"recorded_at":Utc::now(),
            "slug":slug,"reason":"delayed_net_edge_below_threshold","average_price":average,
            "net_edge":net_edge,"rest_latency_ms":latency_ms,
            "signal_ask":signal_ask,"signal_net_edge":signal_net_edge,
            "signal_book_source":signal_book_source,"signal_book_latency_ms":signal_book_latency_ms,
            "signal_seconds_remaining":signal_seconds_remaining,"delay_ms":config.execution_delay_ms,
            "delayed_ask":signal_price
        }));
        return;
    }
    let expected_pnl = probability * target - cost - fees;
    if let Ok(mut guard) = stats.lock() {
        guard.shadow_fills = guard.shadow_fills.saturating_add(1);
        guard.record_rest_latency(latency_ms);
        guard
            .terminal_reason_by_market
            .insert(slug.clone(), "shadow_fill".to_string());
    }
    let _ = journal.append(serde_json::json!({
        "event":"shadow_fill","evaluation_id":evaluation_id,"recorded_at":Utc::now(),
        "asset":asset,"slug":slug,"buy_up":buy_up,"shares":target,"cost_usdc":cost,
        "average_price":average,"fees_usdc":fees,"expected_pnl_usdc":expected_pnl,
        "net_edge":net_edge,"rest_latency_ms":latency_ms,"window_end_ms":window_end_ms,
        "signal_ask":signal_ask,"signal_net_edge":signal_net_edge,
        "signal_book_source":signal_book_source,"signal_book_latency_ms":signal_book_latency_ms,
        "signal_seconds_remaining":signal_seconds_remaining,"delay_ms":config.execution_delay_ms,
        "delayed_ask":signal_price
    }));
    info!(target: LIVE_TARGET, %asset, %slug, buy_up, shares = target, average_price = average,
        fees_usdc = fees, expected_pnl_usdc = expected_pnl, rest_latency_ms = latency_ms,
        "Fair-value shadow fill");
    tokio::spawn(resolve_shadow_fill(
        journal,
        evaluation_id,
        slug,
        buy_up,
        target,
        cost,
        fees,
        window_end_ms,
    ));
}

async fn resolve_shadow_fill(
    journal: EventJournal,
    evaluation_id: String,
    slug: String,
    buy_up: bool,
    shares: f64,
    cost: f64,
    fees: f64,
    window_end_ms: i64,
) {
    let wait_ms = (window_end_ms + 120_000 - Utc::now().timestamp_millis()).max(0) as u64;
    sleep(Duration::from_millis(wait_ms)).await;
    let discoverer = MarketDiscoverer::new(Vec::new(), 5);
    for _ in 0..240 {
        if let Ok(markets) = discoverer
            .fetch_closed_markets_by_slug(vec![slug.clone()])
            .await
        {
            let winner_up = markets.iter().find_map(|market| {
                let prices = market.outcome_prices.as_ref()?;
                (market.closed.unwrap_or(false) && prices.len() == 2)
                    .then(|| prices[0].to_f64().unwrap_or(0.0) > prices[1].to_f64().unwrap_or(0.0))
            });
            if let Some(winner_up) = winner_up {
                let won = winner_up == buy_up;
                let realized_pnl = if won { shares } else { 0.0 } - cost - fees;
                let _ = journal.append(serde_json::json!({
                    "event":"resolution","evaluation_id":evaluation_id,"recorded_at":Utc::now(),
                    "slug":slug,"winner_up":winner_up,"won":won,"realized_pnl_usdc":realized_pnl
                }));
                info!(target: LIVE_TARGET, %slug, won, realized_pnl_usdc = realized_pnl,
                    "Fair-value shadow position resolved");
                return;
            }
        }
        sleep(Duration::from_secs(30)).await;
    }
    warn!(target: LIVE_TARGET, %slug, "Shadow resolution was not observed before retry limit");
}

fn live_fee(config: &DryRunConfig, price: f64) -> f64 {
    config.fee_rate
        * (price * (1.0 - price))
            .max(0.0)
            .powi(config.fee_exponent as i32)
}

pub fn run_backtest() -> Result<PathBuf> {
    let config = Config::from_env()?;
    let connection =
        Connection::open_with_flags(&config.database, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut markets = load_markets(&connection)?
        .into_iter()
        .skip(config.market_offset)
        .collect::<Vec<_>>();
    if config.max_markets > 0 {
        markets.truncate(config.max_markets);
    }
    let run_dir = config
        .output_dir
        .join("short-duration-digital-fair-value")
        .join(format!(
            "run-{}-{}",
            Utc::now().format("%Y%m%dT%H%M%SZ"),
            uuid::Uuid::new_v4()
        ));
    fs::create_dir_all(&run_dir)?;
    let report_path = run_dir.join("report.json");
    let analysis_path = run_dir.join("analysis.zh-CN.md");
    let log_path = run_dir.join("run.log");
    let mut logger = BufWriter::new(File::create(&log_path)?);
    writeln!(
        logger,
        "INFO strategy=short-duration-digital-fair-value phase=started markets={} database={}",
        markets.len(),
        config.database.display()
    )?;

    let mut overall = MetricAccumulator::default();
    let mut by_checkpoint: BTreeMap<i64, MetricAccumulator> = BTreeMap::new();
    let mut by_asset: BTreeMap<String, MetricAccumulator> = BTreeMap::new();
    let mut by_split: BTreeMap<String, MetricAccumulator> = BTreeMap::new();
    let mut scenarios: BTreeMap<ScenarioKey, ExecutionTotals> = BTreeMap::new();
    for checkpoint in &config.checkpoints_secs {
        for delay in &config.delays_ms {
            for haircut in &config.haircuts {
                scenarios
                    .entry(ScenarioKey {
                        checkpoint_secs: *checkpoint,
                        execution_delay_ms: *delay,
                        liquidity_haircut: haircut.to_string(),
                    })
                    .or_default();
            }
        }
    }
    let mut valid_markets = 0usize;
    let mut resolution_reference_samples = 0usize;
    let mut resolution_reference_matches = 0usize;
    let started = Instant::now();
    for (index, market) in markets.iter().enumerate() {
        let split = if index * 10 < markets.len() * 6 {
            "train"
        } else if index * 10 < markets.len() * 8 {
            "validation"
        } else {
            "test"
        };
        let snapshots = load_snapshots(&connection, &market.id)?;
        let (valid, reference_match) = evaluate_market(
            &config,
            market,
            &snapshots,
            &mut overall,
            &mut by_checkpoint,
            &mut by_asset,
            &mut by_split,
            &mut scenarios,
            split,
        )?;
        valid_markets += usize::from(valid);
        if let Some(reference_match) = reference_match {
            resolution_reference_samples += 1;
            resolution_reference_matches += usize::from(reference_match);
        }
        let complete = index + 1;
        if complete == markets.len() || complete % config.progress_every == 0 {
            writeln!(logger, "INFO strategy=short-duration-digital-fair-value progress={}/{} elapsed_secs={} valid_markets={} predictions={}", complete, markets.len(), started.elapsed().as_secs(), valid_markets, overall.samples)?;
            logger.flush()?;
            println!(
                "short-duration-digital-fair-value {complete}/{} ({:.1}%) predictions={}",
                markets.len(),
                complete as f64 / markets.len().max(1) as f64 * 100.0,
                overall.samples
            );
        }
    }

    let direction_overall = overall.report();
    let direction_by_checkpoint = by_checkpoint
        .into_iter()
        .map(|(key, value)| (key, value.report()))
        .collect::<BTreeMap<_, _>>();
    let direction_by_asset = by_asset
        .into_iter()
        .map(|(key, value)| (key, value.report()))
        .collect::<BTreeMap<_, _>>();
    let direction_by_split = by_split
        .into_iter()
        .map(|(key, value)| (key, value.report()))
        .collect::<BTreeMap<_, _>>();
    let scenario_reports = scenarios
        .into_iter()
        .map(|(key, totals)| ScenarioReport {
            checkpoint_secs: key.checkpoint_secs,
            execution_delay_ms: key.execution_delay_ms,
            liquidity_haircut: key.liquidity_haircut,
            candidates: totals.candidates,
            fills: totals.fills,
            wins: totals.wins,
            losses: totals.losses,
            win_rate: ratio(totals.wins, totals.fills),
            gross_pnl_usdc: totals.gross_pnl_usdc,
            fees_usdc: totals.fees_usdc,
            net_pnl_usdc: totals.net_pnl_usdc,
            average_net_pnl_usdc: divide(totals.net_pnl_usdc, totals.fills),
            validation_fills: totals.validation_fills,
            validation_net_pnl_usdc: totals.validation_net_pnl_usdc,
            test_fills: totals.test_fills,
            test_net_pnl_usdc: totals.test_net_pnl_usdc,
            rejections: totals.rejections,
        })
        .collect::<Vec<_>>();
    let resolution_reference_match_rate =
        ratio(resolution_reference_matches, resolution_reference_samples);
    let test_direction = direction_by_split.get("test").cloned().unwrap_or_default();
    let direction_ready = test_direction.samples >= config.min_direction_samples
        && resolution_reference_samples
            >= config.min_direction_samples / config.checkpoints_secs.len().max(1)
        && resolution_reference_match_rate >= 0.99
        && test_direction.brier_score < 0.25
        && (test_direction.market_samples == 0
            || test_direction.brier_score <= test_direction.market_brier_score * 1.05);
    let conservative_delay = *config.delays_ms.iter().max().expect("validated");
    let conservative_haircut = config.haircuts.iter().min().expect("validated").to_string();
    let conservative = scenario_reports
        .iter()
        .filter(|scenario| {
            scenario.execution_delay_ms == conservative_delay
                && scenario.liquidity_haircut == conservative_haircut
        })
        .collect::<Vec<_>>();
    let conservative_cost_ready = conservative.iter().any(|scenario| {
        scenario.validation_fills >= config.min_cost_fills
            && scenario.validation_net_pnl_usdc > 0.0
            && scenario.test_fills >= config.min_cost_fills
            && scenario.test_net_pnl_usdc > 0.0
    });
    let readiness = if !direction_ready {
        "NOT_READY_DIRECTION_MODEL"
    } else if !conservative_cost_ready {
        "NOT_READY_COST_SENSITIVITY"
    } else {
        "READY_FOR_DRY_RUN_REVIEW"
    };
    let report = Report {
        strategy: "short-duration-digital-fair-value",
        generated_at: Utc::now().to_rfc3339(),
        database: config.database.display().to_string(),
        markets: markets.len(),
        markets_with_valid_predictions: valid_markets,
        resolution_reference_samples,
        resolution_reference_matches,
        resolution_reference_match_rate,
        parameters: serde_json::json!({
            "checkpoints_secs": config.checkpoints_secs,
            "volatility_lookback_secs": config.volatility_lookback_secs,
            "min_observations": config.min_observations,
            "min_volatility_bps_per_sqrt_second": config.min_volatility,
            "min_net_edge": config.min_edge,
            "price_range": [config.min_price, config.max_price],
            "order_usdc": config.order_usdc,
            "delays_ms": config.delays_ms,
            "liquidity_haircuts": config.haircuts,
            "fee_rate_assumption": config.fee_rate,
            "fee_exponent_assumption": config.fee_exponent,
            "price_to_beat_source": "first Chainlink TWAP observation within 5 seconds of market start",
        }),
        direction_overall,
        direction_by_checkpoint,
        direction_by_asset,
        direction_by_split,
        execution_scenarios: scenario_reports,
        readiness: readiness.to_string(),
        direction_ready,
        conservative_cost_ready,
        limitations: vec![
            "The first Chainlink TWAP observation near market start is treated as the Price to Beat; the audit uses the last pre-close TWAP snapshot, so a small mismatch rate can come from the missing final second rather than the reference itself.",
            "The Gaussian model treats observed Chainlink TWAP changes as a diffusion proxy and may understate jumps and overlapping-window dependence.",
            "Historical per-market fee parameters are absent; fee rate and exponent are scenario assumptions.",
            "Snapshot replay cannot prove an order would fill between snapshots or reproduce exchange queue priority.",
        ],
    };
    serde_json::to_writer_pretty(File::create(&report_path)?, &report)?;
    write_analysis(&analysis_path, &report)?;
    writeln!(logger, "INFO strategy=short-duration-digital-fair-value phase=completed readiness={} elapsed_secs={} report={}", readiness, started.elapsed().as_secs(), report_path.display())?;
    logger.flush()?;
    Ok(report_path)
}

#[allow(clippy::too_many_arguments)]
fn evaluate_market(
    config: &Config,
    market: &Market,
    snapshots: &[Snapshot],
    overall: &mut MetricAccumulator,
    by_checkpoint: &mut BTreeMap<i64, MetricAccumulator>,
    by_asset: &mut BTreeMap<String, MetricAccumulator>,
    by_split: &mut BTreeMap<String, MetricAccumulator>,
    scenarios: &mut BTreeMap<ScenarioKey, ExecutionTotals>,
    split: &str,
) -> Result<(bool, Option<bool>)> {
    let mut any = false;
    let mut book_cache: HashMap<(usize, bool), Option<Vec<(f64, f64)>>> = HashMap::new();
    let Some(reference) = opening_reference(market, snapshots) else {
        return Ok((false, None));
    };
    let reference_match = snapshots
        .iter()
        .rev()
        .find_map(valid_twap)
        .map(|final_twap| (final_twap >= reference) == market.winner_up);
    for checkpoint in &config.checkpoints_secs {
        let target_ms = market.end_ms - checkpoint * 1_000;
        let Some(signal_index) = snapshot_index_at_or_after(snapshots, target_ms) else {
            continue;
        };
        let signal = &snapshots[signal_index];
        let Some(current_twap) = valid_twap(signal) else {
            continue;
        };
        let Some(volatility) = observed_volatility(
            snapshots,
            signal_index,
            config.volatility_lookback_secs,
            config.min_observations,
        ) else {
            continue;
        };
        let floor = config
            .min_volatility
            .get(&market.coin)
            .copied()
            .unwrap_or(1.5);
        let probability_up = fair_up_probability(
            reference,
            current_twap,
            *checkpoint as f64,
            volatility.max(floor),
        );
        let market_probability = midpoint(signal.up_bid, signal.up_ask);
        overall.record(probability_up, market.winner_up, market_probability);
        by_checkpoint.entry(*checkpoint).or_default().record(
            probability_up,
            market.winner_up,
            market_probability,
        );
        by_asset.entry(market.coin.clone()).or_default().record(
            probability_up,
            market.winner_up,
            market_probability,
        );
        by_split.entry(split.to_string()).or_default().record(
            probability_up,
            market.winner_up,
            market_probability,
        );
        any = true;

        let up_edge = executable_edge(probability_up, signal.up_ask, config);
        let down_edge = executable_edge(1.0 - probability_up, signal.down_ask, config);
        let choice = if up_edge >= down_edge && up_edge >= config.min_edge {
            Some((true, probability_up, signal.up_ask))
        } else if down_edge >= config.min_edge {
            Some((false, 1.0 - probability_up, signal.down_ask))
        } else {
            None
        };
        for delay in &config.delays_ms {
            for haircut in &config.haircuts {
                let key = ScenarioKey {
                    checkpoint_secs: *checkpoint,
                    execution_delay_ms: *delay,
                    liquidity_haircut: haircut.to_string(),
                };
                let totals = scenarios.get_mut(&key).expect("scenario initialized");
                let Some((buy_up, probability, signal_ask)) = choice else {
                    reject(totals, "edge_below_threshold");
                    continue;
                };
                totals.candidates += 1;
                let Some(execution_index) =
                    snapshot_index_at_or_after(snapshots, signal.time_ms.saturating_add(*delay))
                else {
                    reject(totals, "no_delayed_snapshot");
                    continue;
                };
                let execution = &snapshots[execution_index];
                let cache_key = (execution_index, buy_up);
                if let std::collections::hash_map::Entry::Vacant(entry) =
                    book_cache.entry(cache_key)
                {
                    let book = if buy_up {
                        execution.up_book.as_deref()
                    } else {
                        execution.down_book.as_deref()
                    };
                    entry.insert(decode_asks(book)?);
                }
                let signal_fee = dynamic_fee(
                    config.fee_rate,
                    config.fee_exponent,
                    signal_ask.unwrap_or(1.0),
                );
                let shares = config.order_usdc.to_f64().unwrap_or(0.0)
                    / (signal_ask.unwrap_or(1.0) + signal_fee).max(0.01);
                let Some(cost) = book_cache
                    .get(&cache_key)
                    .and_then(|levels| levels.as_deref())
                    .and_then(|levels| {
                        walk_ask_levels(levels, shares, haircut.to_f64().unwrap_or(0.0))
                    })
                else {
                    reject(totals, "execution_depth_insufficient");
                    continue;
                };
                let average = cost / shares;
                if average < config.min_price || average > config.max_price {
                    reject(totals, "execution_price_out_of_range");
                    continue;
                }
                let fee_per_share = dynamic_fee(config.fee_rate, config.fee_exponent, average);
                let net_edge = probability - average - fee_per_share;
                if net_edge < config.min_edge {
                    reject(totals, "delayed_net_edge_below_threshold");
                    continue;
                }
                let fees = fee_per_share * shares;
                let won = market.winner_up == buy_up;
                let payout = if won { shares } else { 0.0 };
                totals.fills += 1;
                totals.wins += usize::from(won);
                totals.losses += usize::from(!won);
                totals.gross_pnl_usdc += payout - cost;
                totals.fees_usdc += fees;
                totals.net_pnl_usdc += payout - cost - fees;
                if split == "validation" {
                    totals.validation_fills += 1;
                    totals.validation_net_pnl_usdc += payout - cost - fees;
                } else if split == "test" {
                    totals.test_fills += 1;
                    totals.test_net_pnl_usdc += payout - cost - fees;
                }
            }
        }
    }
    Ok((any, reference_match))
}

fn opening_reference(market: &Market, snapshots: &[Snapshot]) -> Option<f64> {
    snapshots
        .iter()
        .filter_map(|snapshot| {
            let observed_at = snapshot.twap_observed_at_ms?;
            let value = valid_twap(snapshot)?;
            ((observed_at - market.start_ms).abs() <= 5_000).then_some((observed_at, value))
        })
        .min_by_key(|(observed_at, _)| (observed_at - market.start_ms).abs())
        .map(|(_, value)| value)
}

fn fair_up_probability(
    reference: f64,
    current: f64,
    remaining_secs: f64,
    volatility_bps: f64,
) -> f64 {
    if reference <= 0.0 || current <= 0.0 || remaining_secs <= 0.0 || volatility_bps <= 0.0 {
        return 0.5;
    }
    let margin_bps = (current / reference).ln() * 10_000.0;
    normal_cdf(margin_bps / (volatility_bps * remaining_secs.sqrt()).max(1e-9))
}

fn observed_volatility(
    snapshots: &[Snapshot],
    end_index: usize,
    lookback_secs: i64,
    min_observations: usize,
) -> Option<f64> {
    let cutoff = snapshots[end_index].time_ms - lookback_secs * 1_000;
    let mut observations = Vec::new();
    let mut last_observed_at = None;
    for snapshot in snapshots[..=end_index]
        .iter()
        .filter(|s| s.time_ms >= cutoff)
    {
        let (Some(value), Some(observed_at)) = (valid_twap(snapshot), snapshot.twap_observed_at_ms)
        else {
            continue;
        };
        if last_observed_at == Some(observed_at) {
            continue;
        }
        last_observed_at = Some(observed_at);
        observations.push((observed_at, value));
    }
    if observations.len() < min_observations {
        return None;
    }
    let mut sum = 0.0;
    let mut count = 0usize;
    for pair in observations.windows(2) {
        let dt = (pair[1].0 - pair[0].0) as f64 / 1_000.0;
        if dt <= 0.0 || pair[0].1 <= 0.0 || pair[1].1 <= 0.0 {
            continue;
        }
        let normalized = (pair[1].1 / pair[0].1).ln() * 10_000.0 / dt.sqrt();
        sum += normalized * normalized;
        count += 1;
    }
    (count > 0).then(|| (sum / count as f64).sqrt())
}

fn valid_twap(snapshot: &Snapshot) -> Option<f64> {
    (snapshot.twap_source.as_deref() == Some("chainlink_twap"))
        .then_some(snapshot.twap)
        .flatten()
        .filter(|value| *value > 0.0)
}

fn executable_edge(probability: f64, ask: Option<f64>, config: &Config) -> f64 {
    let Some(ask) = ask.filter(|price| *price >= config.min_price && *price <= config.max_price)
    else {
        return f64::NEG_INFINITY;
    };
    probability - ask - dynamic_fee(config.fee_rate, config.fee_exponent, ask)
}

fn dynamic_fee(rate: Decimal, exponent: u64, price: f64) -> f64 {
    let rate = rate.to_f64().unwrap_or(0.0);
    let base = (price * (1.0 - price)).max(0.0);
    rate * base.powi(i32::try_from(exponent).unwrap_or(i32::MAX))
}

fn midpoint(bid: Option<f64>, ask: Option<f64>) -> Option<f64> {
    match (bid, ask) {
        (Some(bid), Some(ask)) if bid > 0.0 && ask >= bid => Some((bid + ask) / 2.0),
        _ => None,
    }
}

fn normal_cdf(value: f64) -> f64 {
    let sign = if value < 0.0 { -1.0 } else { 1.0 };
    let x = value.abs() / 2.0_f64.sqrt();
    let t = 1.0 / (1.0 + 0.327_591_1 * x);
    let erf = 1.0
        - (((((1.061_405_429 * t - 1.453_152_027) * t) + 1.421_413_741) * t - 0.284_496_736) * t
            + 0.254_829_592)
            * t
            * (-x * x).exp();
    (0.5 * (1.0 + sign * erf)).clamp(0.000_001, 0.999_999)
}

fn decode_asks(book_zstd: Option<&[u8]>) -> Result<Option<Vec<(f64, f64)>>> {
    let Some(book_zstd) = book_zstd else {
        return Ok(None);
    };
    let decoded = zstd::stream::decode_all(book_zstd).context("decode compressed order book")?;
    let value: Value = serde_json::from_slice(&decoded).context("parse compressed order book")?;
    let Some(levels) = value.get("asks").and_then(Value::as_array) else {
        return Ok(None);
    };
    let mut asks = levels
        .iter()
        .filter_map(|level| {
            Some((
                value_f64(level.get("price")?)?,
                value_f64(level.get("size")?)?,
            ))
        })
        .filter(|(price, size)| *price > 0.0 && *size > 0.0)
        .collect::<Vec<_>>();
    asks.sort_by(|a, b| a.0.total_cmp(&b.0));
    Ok(Some(asks))
}

fn walk_ask_levels(asks: &[(f64, f64)], target: f64, haircut: f64) -> Option<f64> {
    let mut remaining = target;
    let mut cost = 0.0;
    for (price, size) in asks {
        let filled = remaining.min(*size * haircut);
        remaining -= filled;
        cost += filled * *price;
        if remaining <= 1e-9 {
            return Some(cost);
        }
    }
    None
}

fn load_markets(connection: &Connection) -> Result<Vec<Market>> {
    let mut statement = connection.prepare(
        "SELECT market_id,coin,start_time,end_time,winner FROM markets
         WHERE status='complete' AND start_time IS NOT NULL AND end_time IS NOT NULL
           AND winner IS NOT NULL
         ORDER BY start_time,market_id",
    )?;
    let markets = statement
        .query_map([], |row| {
            let winner: String = row.get(4)?;
            Ok(Market {
                id: row.get(0)?,
                coin: row.get::<_, String>(1)?.to_ascii_lowercase(),
                start_ms: normalize_ms(row.get(2)?),
                end_ms: normalize_ms(row.get(3)?),
                winner_up: matches!(winner.trim().to_ascii_lowercase().as_str(), "up" | "yes"),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(markets)
}

fn load_snapshots(connection: &Connection, market_id: &str) -> Result<Vec<Snapshot>> {
    let mut statement = connection.prepare(
        "SELECT time,coin_twap,coin_twap_observed_at,coin_twap_source,
                up_best_bid,up_best_ask,down_best_ask,
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
        twap: optional_f64(row, 1)?,
        twap_observed_at_ms: row.get::<_, Option<i64>>(2)?.map(normalize_ms),
        twap_source: row.get(3)?,
        up_bid: optional_f64(row, 4)?,
        up_ask: optional_f64(row, 5)?,
        down_ask: optional_f64(row, 6)?,
        up_book: row.get(7)?,
        down_book: row.get(8)?,
    })
}

fn snapshot_index_at_or_after(snapshots: &[Snapshot], target_ms: i64) -> Option<usize> {
    let index = snapshots.partition_point(|snapshot| snapshot.time_ms < target_ms);
    (index < snapshots.len()).then_some(index)
}

fn write_analysis(path: &Path, report: &Report) -> Result<()> {
    let mut file = BufWriter::new(File::create(path)?);
    writeln!(file, "# Short-Duration Digital Fair Value 回测分析\n")?;
    writeln!(file, "- 准入结论：`{}`", report.readiness)?;
    writeln!(
        file,
        "- 市场：{}；有效预测市场：{}",
        report.markets, report.markets_with_valid_predictions
    )?;
    writeln!(
        file,
        "- 结算参考一致性：{}/{}（{:.4}）",
        report.resolution_reference_matches,
        report.resolution_reference_samples,
        report.resolution_reference_match_rate
    )?;
    writeln!(
        file,
        "- 方向样本：{}；准确率：{:.4}；Brier：{:.6}；市场 Brier：{:.6}",
        report.direction_overall.samples,
        report.direction_overall.accuracy,
        report.direction_overall.brier_score,
        report.direction_overall.market_brier_score
    )?;
    writeln!(file, "\n## 分检查点概率质量\n")?;
    writeln!(
        file,
        "| 剩余秒数 | 样本 | 准确率 | 模型 Brier | 市场 Brier |\n|---:|---:|---:|---:|---:|"
    )?;
    for (checkpoint, metrics) in &report.direction_by_checkpoint {
        writeln!(
            file,
            "| {} | {} | {:.4} | {:.6} | {:.6} |",
            checkpoint,
            metrics.samples,
            metrics.accuracy,
            metrics.brier_score,
            metrics.market_brier_score
        )?;
    }
    writeln!(file, "\n## 时间顺序样本切分\n")?;
    writeln!(
        file,
        "| 分组 | 样本 | 准确率 | 模型 Brier | 市场 Brier |\n|---|---:|---:|---:|---:|"
    )?;
    for (split, metrics) in &report.direction_by_split {
        writeln!(
            file,
            "| {} | {} | {:.4} | {:.6} | {:.6} |",
            split,
            metrics.samples,
            metrics.accuracy,
            metrics.brier_score,
            metrics.market_brier_score
        )?;
    }
    writeln!(file, "\n## 成本情景\n")?;
    writeln!(file, "| 剩余秒数 | 延迟 ms | 深度折扣 | 候选 | 成交 | 净 PnL | 验证集成交 | 验证集净 PnL | 测试集成交 | 测试集净 PnL |\n|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|")?;
    for scenario in &report.execution_scenarios {
        writeln!(
            file,
            "| {} | {} | {} | {} | {} | {:.4} | {} | {:.4} | {} | {:.4} |",
            scenario.checkpoint_secs,
            scenario.execution_delay_ms,
            scenario.liquidity_haircut,
            scenario.candidates,
            scenario.fills,
            scenario.net_pnl_usdc,
            scenario.validation_fills,
            scenario.validation_net_pnl_usdc,
            scenario.test_fills,
            scenario.test_net_pnl_usdc
        )?;
    }
    writeln!(file, "\n## 解释\n")?;
    if report.readiness == "READY_FOR_DRY_RUN_REVIEW" {
        writeln!(file, "方向模型和最保守成本场景均达到最低门槛，可以进入独立实时 dry-run 的代码审核与部署准备；这不表示允许真实资金交易。")?;
    } else {
        writeln!(file, "当前报告未达到进入 dry-run 的自动门槛。应先检查概率校准、参考价映射、拒绝漏斗和保守执行情景，不应通过放宽阈值强行晋级。")?;
    }
    writeln!(file, "\n## 数据边界\n")?;
    for limitation in &report.limitations {
        writeln!(file, "- {limitation}")?;
    }
    Ok(())
}

fn reject(totals: &mut ExecutionTotals, reason: &str) {
    *totals.rejections.entry(reason.to_string()).or_default() += 1;
}

fn ratio(numerator: usize, denominator: usize) -> f64 {
    divide(numerator as f64, denominator)
}
fn divide(value: f64, denominator: usize) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        value / denominator as f64
    }
}
fn normalize_ms(value: i64) -> i64 {
    if value < 10_000_000_000 {
        value * 1_000
    } else {
        value
    }
}
fn optional_f64(row: &Row<'_>, index: usize) -> rusqlite::Result<Option<f64>> {
    Ok(row
        .get::<_, Option<String>>(index)?
        .and_then(|value| value.parse().ok()))
}
fn value_f64(value: &Value) -> Option<f64> {
    match value {
        Value::String(v) => v.parse().ok(),
        Value::Number(v) => v.as_f64(),
        _ => None,
    }
}
fn env_decimal(name: &str, default: &str) -> Result<Decimal> {
    Decimal::from_str(&std::env::var(name).unwrap_or_else(|_| default.to_string()))
        .with_context(|| format!("{name} must be a decimal"))
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
        .map(|v| {
            v.trim()
                .parse()
                .with_context(|| format!("{name} contains an invalid integer"))
        })
        .collect::<Result<Vec<_>>>()?;
    values.sort_unstable();
    values.dedup();
    Ok(values)
}
fn env_decimal_list(name: &str, default: &[&str]) -> Result<Vec<Decimal>> {
    let raw = std::env::var(name).unwrap_or_else(|_| default.join(","));
    let mut values = raw
        .split(',')
        .map(|v| {
            Decimal::from_str(v.trim())
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

    fn dry_run_config() -> DryRunConfig {
        DryRunConfig {
            symbols: vec!["btc".to_string()],
            checkpoint_secs: 10,
            scan_interval_ms: 100,
            feed_ready_timeout_secs: 15,
            rest_fallback_cooldown_ms: 1_000,
            execution_delay_ms: 1_000,
            max_book_age_ms: 1_000,
            max_feed_age_ms: 5_000,
            opening_max_skew_ms: 5_000,
            volatility_lookback_secs: 120,
            min_observations: 20,
            min_volatility: HashMap::new(),
            min_edge: 0.03,
            max_spread: 0.03,
            min_depth_usdc: 10.0,
            min_price: 0.05,
            max_price: 0.95,
            order_usdc: 5.0,
            liquidity_haircut: 0.50,
            fee_rate: 0.07,
            fee_exponent: 1,
            rtds_url: String::new(),
            clob_url: String::new(),
            results_file: PathBuf::new(),
        }
    }

    #[test]
    fn fair_probability_moves_with_moneyness_and_time() {
        let at_money = fair_up_probability(100.0, 100.0, 60.0, 1.0);
        let above = fair_up_probability(100.0, 100.1, 60.0, 1.0);
        let above_later = fair_up_probability(100.0, 100.1, 10.0, 1.0);
        assert!((at_money - 0.5).abs() < 1e-6);
        assert!(above > 0.5);
        assert!(above_later > above);
    }

    #[test]
    fn current_crypto_fee_is_largest_near_half_probability() {
        let rate = Decimal::from_str("0.07").unwrap();
        assert!(dynamic_fee(rate, 1, 0.5) > dynamic_fee(rate, 1, 0.9));
    }

    #[test]
    fn metric_accumulator_uses_probability_not_only_direction() {
        let mut good = MetricAccumulator::default();
        good.record(0.9, true, Some(0.6));
        let mut weak = MetricAccumulator::default();
        weak.record(0.6, true, Some(0.6));
        assert!(good.report().brier_score < weak.report().brier_score);
    }

    #[test]
    fn live_book_filter_reports_specific_rejection_reasons() {
        let config = dry_run_config();
        assert_eq!(
            live_edge_evaluation(&config, 0.9, None, Some(0.8), Some(20.0)),
            Err("incomplete_book")
        );
        assert_eq!(
            live_edge_evaluation(&config, 0.9, Some(0.80), Some(0.96), Some(20.0)),
            Err("price_out_of_range")
        );
        assert_eq!(
            live_edge_evaluation(&config, 0.9, Some(0.70), Some(0.80), Some(20.0)),
            Err("spread_too_wide")
        );
        assert_eq!(
            live_edge_evaluation(&config, 0.9, Some(0.79), Some(0.80), Some(5.0)),
            Err("insufficient_depth")
        );
        assert!(
            live_edge_evaluation(&config, 0.9, Some(0.79), Some(0.80), Some(20.0)).unwrap() > 0.0
        );
    }
}
