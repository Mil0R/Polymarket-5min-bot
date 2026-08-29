use anyhow::{bail, Context, Result};
use polymarket_client_sdk_v2::clob::types::OrderType;
use std::env;

use polymarket_client_sdk::types::Address;

use crate::live_safety::{
    ABSOLUTE_CANARY_LOSS_LIMIT_USDC, ABSOLUTE_CANARY_PAIR_LIMIT_USDC,
    ABSOLUTE_CANARY_TOTAL_LIMIT_USDC,
};
use crate::trading::CLOB_API_URL_DEFAULT;

/// Parse arbitrage order type: GTC, GTD, FOK, FAK; case-insensitive; invalid/unknown defaults to GTD.
fn parse_arbitrage_order_type(s: &str) -> Result<OrderType> {
    match s.trim().to_uppercase().as_str() {
        "GTC" => Ok(OrderType::GTC),
        "GTD" => Ok(OrderType::GTD),
        "FOK" => Ok(OrderType::FOK),
        "FAK" => Ok(OrderType::FAK),
        other => bail!(
            "BINARY_COMPLEMENT_ARBITRAGE_ORDER_TYPE must be GTC, GTD, FOK or FAK; got {other}"
        ),
    }
}

/// Parse slippage array: comma-separated, e.g. "-0.02,0.0".
/// Index 0=up/flat side, 1=down-only side. Single value used for both. Default "0,0.01".
fn parse_slippage(s: &str) -> Result<[f64; 2]> {
    let parts: Vec<f64> = s
        .split(',')
        .map(|x| {
            x.trim()
                .parse::<f64>()
                .context("BINARY_COMPLEMENT_ARBITRAGE_SLIPPAGE contains an invalid number")
        })
        .collect::<Result<_>>()?;
    let parsed = match parts.as_slice() {
        [] => bail!("BINARY_COMPLEMENT_ARBITRAGE_SLIPPAGE cannot be empty"),
        [one] => [*one, *one],
        [first, second] => [*first, *second],
        _ => {
            bail!("BINARY_COMPLEMENT_ARBITRAGE_SLIPPAGE accepts one or two comma-separated numbers")
        }
    };
    Ok(parsed)
}

fn parse_millisecond_list(name: &str, raw: &str) -> Result<Vec<u64>> {
    let mut values = raw
        .split(',')
        .map(|value| {
            value
                .trim()
                .parse::<u64>()
                .with_context(|| format!("{name} contains an invalid millisecond value"))
        })
        .collect::<Result<Vec<_>>>()?;
    if values.is_empty() {
        bail!("{name} cannot be empty");
    }
    values.sort_unstable();
    values.dedup();
    Ok(values)
}

fn parse_positive_decimal_list(name: &str, raw: &str) -> Result<Vec<f64>> {
    let mut values = raw
        .split(',')
        .map(|value| {
            value
                .trim()
                .parse::<f64>()
                .with_context(|| format!("{name} contains an invalid decimal value"))
        })
        .collect::<Result<Vec<_>>>()?;
    if values.is_empty() {
        bail!("{name} cannot be empty");
    }
    if values
        .iter()
        .any(|value| !value.is_finite() || *value <= 0.0)
    {
        bail!("{name} values must be finite and greater than zero");
    }
    values.sort_by(f64::total_cmp);
    values.dedup_by(|left, right| left.total_cmp(right).is_eq());
    Ok(values)
}

fn env_parse<T>(name: &str, default: &str) -> Result<T>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    let raw = env::var(name).unwrap_or_else(|_| default.to_string());
    raw.trim()
        .parse::<T>()
        .map_err(|e| anyhow::anyhow!("{name} has invalid value {raw:?}: {e}"))
}

/// Normalize a hex private key: trim whitespace, strip a leading "0x"/"0X", lowercase.
fn normalize_private_key(raw: &str) -> String {
    let trimmed = raw.trim();
    let stripped = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
        .unwrap_or(trimmed);
    stripped.to_ascii_lowercase()
}

#[derive(Debug, Clone)]
pub struct Config {
    pub private_key: String,
    pub proxy_address: Option<Address>, // Funder from Settings (deposit wallet or legacy proxy)
    /// V2 CLOB API base URL (default https://clob.polymarket.com — do not use clob-v2 host)
    pub clob_api_url: String,
    /// CLOB signature type: Poly1271 | Proxy | GnosisSafe | Eoa (see SIGNATURE_TYPE)
    pub signature_type: String,
    pub min_profit_threshold: f64,
    /// Minimum cumulative ask-side liquidity (USD) required on BOTH legs before an
    /// opportunity is admitted. Filters out guard/phantom quotes on 5-min markets.
    pub min_ask_depth_usdc: f64,
    pub max_order_size_usdc: f64,
    pub crypto_symbols: Vec<String>,
    pub market_refresh_advance_secs: u64,
    /// Market window period in minutes: 5 or 15 are supported by Polymarket
    /// crypto up/down slugs (btc-updown-5m / btc-updown-15m).
    pub market_period_minutes: u64,
    pub risk_max_exposure_usdc: f64,
    /// Maximum capital/exposure assigned to Binary Complement Arbitrage.
    pub arbitrage_budget_usdc: f64,
    pub risk_imbalance_threshold: f64,
    pub hedge_take_profit_pct: f64, // Hedge take-profit % (e.g. 0.05 = 5%)
    pub hedge_stop_loss_pct: f64,   // Hedge stop-loss % (e.g. 0.05 = 5%)
    pub arbitrage_execution_spread: f64, // Execute arbitrage when yes+no <= 1 - this spread
    /// Additional net-edge safety/model/rounding buffer applied after exact dynamic fees.
    pub arbitrage_fee_buffer: f64,
    /// PAPER-only research mode: replace the fixed gross-edge gate with the
    /// exact dynamic-fee net-edge gate. Live execution always keeps both gates.
    pub arbitrage_dynamic_fee_gate_paper: bool,
    /// Slippage [first, second]: down-only uses second, up/flat uses first. e.g. "-0.02,0.0"
    pub slippage: [f64; 2],
    pub gtd_expiration_secs: u64, // GTD order expiry (seconds), default 300 (5 min); only when arbitrage_order_type=GTD
    /// Order type for arbitrage: GTC (good till cancel), GTD (with gtd_expiration_secs), FOK (fill or kill), FAK (fill and kill remainder)
    pub arbitrage_order_type: OrderType,
    pub stop_arbitrage_before_end_minutes: u64, // Stop arbitrage N minutes before market end, default 0 (no stop)
    /// Initial runtime state for the YES+NO binary complement arbitrage strategy.
    pub arbitrage_enabled: bool,
    /// Window-end "buy the determined winner" strategy (single-leg directional buy).
    pub late_resolution_winner_trading_enabled: bool,
    pub wallet_copy_trading_enabled: bool,
    /// Maximum simultaneous capital assigned to Late-Resolution Winner Trading.
    pub late_resolution_winner_trading_budget_usdc: f64,
    /// PAPER NAV assigned to Wallet-Copy Trading when embedded in polypulse.
    pub wallet_copy_trading_budget_usdc: f64,
    /// Trigger when the window has this many seconds left.
    pub late_resolution_winner_trading_before_end_secs: u32,
    /// Only buy a side whose best ask is >= this price (direction considered decided).
    pub late_resolution_winner_trading_min_price: f64,
    /// Do not buy above this price (edge < 1 - max_price becomes too thin).
    pub late_resolution_winner_trading_max_price: f64,
    /// PAPER-only research path: allow the evidence-backed Chainlink-direction
    /// veto cohort through the normal simulated order lifecycle.
    pub late_resolution_winner_trading_hybrid_paper_enabled: bool,
    /// Max notional (USDC) per late-resolution winner trading.
    pub late_resolution_winner_trading_max_order_usdc: f64,
    /// Max late-resolution winner tradings per window (0 = unlimited).
    pub late_resolution_winner_trading_max_per_window: u32,
    /// Stop Late-Resolution Winner Trading after cumulative realized losses reach this amount.
    pub late_resolution_winner_trading_max_strategy_loss_usdc: f64,
    /// Stop Late-Resolution Winner Trading for one asset after its realized losses reach this amount.
    pub late_resolution_winner_trading_max_asset_loss_usdc: f64,
    /// Stop Late-Resolution Winner Trading after this many consecutive losing settlements (0 = disabled).
    pub late_resolution_winner_trading_max_consecutive_losses: u32,
    /// Automatically redeem confirmed winning tokens in live mode.
    pub late_resolution_winner_trading_auto_redeem: bool,
    /// Polymarket RTDS endpoint for the official Chainlink 60-second TWAP.
    pub late_resolution_winner_trading_rtds_url: String,
    /// Late-Resolution Winner Trading WS snapshot age limit. Kept separate from the much stricter
    /// two-leg arbitrage synchronization threshold.
    pub late_resolution_winner_trading_max_book_age_ms: i64,
    pub late_resolution_winner_trading_feed_max_age_ms: i64,
    pub late_resolution_winner_trading_opening_max_skew_ms: i64,
    pub late_resolution_winner_trading_volatility_lookback_secs: u64,
    pub late_resolution_winner_trading_min_observations: usize,
    pub late_resolution_winner_trading_confirmation_observations: u32,
    pub late_resolution_winner_trading_max_attempts_per_market: u32,
    pub late_resolution_winner_trading_min_margin_bps: f64,
    pub late_resolution_winner_trading_reversal_sigma: f64,
    pub late_resolution_winner_trading_max_spread: f64,
    pub late_resolution_winner_trading_min_depth_usdc: f64,
    pub late_resolution_winner_trading_min_net_ev: f64,
    pub late_resolution_winner_trading_btc_min_volatility_bps: f64,
    pub late_resolution_winner_trading_eth_min_volatility_bps: f64,
    pub late_resolution_winner_trading_other_min_volatility_bps: f64,
    pub late_resolution_winner_trading_btc_model_risk_buffer: f64,
    pub late_resolution_winner_trading_eth_model_risk_buffer: f64,
    pub late_resolution_winner_trading_other_model_risk_buffer: f64,
    /// Scheduled Merge interval (minutes); 0 = disabled. CONDITION_ID from current window markets like orderbook.
    pub merge_interval_minutes: u64,
    /// YES price threshold: only execute arbitrage when YES >= this, default 0.0 (no limit)
    pub min_yes_price_threshold: f64,
    /// NO price threshold: only execute arbitrage when NO >= this, default 0.0 (no limit)
    pub min_no_price_threshold: f64,
    /// Position sync interval (seconds), default 10 (fetch from API, overwrite local cache)
    pub position_sync_interval_secs: u64,
    /// Position balance check interval (seconds), default 60
    pub position_balance_interval_secs: u64,
    /// Imbalance threshold: cancel orders only when position diff >= this, default 2.0
    pub position_balance_threshold: f64,
    /// Min total position: run balance only when total >= this, default 5.0
    pub position_balance_min_total: f64,
    /// Wind-down before window end: minutes before 5min window end to trigger (cancel→Merge→market sell rest). 0=disabled.
    pub wind_down_before_window_end_minutes: u64,
    /// Limit price for one-sided leg sells during wind-down (aim for fast fill), default 0.01
    pub wind_down_sell_price: f64,
    /// Optional append-only JSON opportunity log. Disabled when unset.
    pub arbitrage_log_file: Option<String>,
    /// Optional append-only shadow log for price-positive candidates rejected
    /// by the production depth floor. It never changes execution admission.
    pub arbitrage_shadow_log_file: Option<String>,
    /// Late-Resolution Winner Trading order/fill/settlement lifecycle journal.
    pub late_resolution_winner_trading_state_journal_file: String,
    /// Late-Resolution Winner Trading candidate and counterfactual evaluation journal.
    pub late_resolution_winner_trading_evaluation_journal_file: String,
    /// Observe and simulate fills without submitting, cancelling, merging or selling.
    pub dry_run: bool,
    /// Explicit, fail-closed mode for the first real-money validation run.
    pub canary_mode: bool,
    pub canary_max_pair_notional_usdc: f64,
    pub canary_max_total_notional_usdc: f64,
    pub canary_max_loss_usdc: f64,
    pub canary_max_pairs_per_window: u32,
    pub canary_max_pairs_total: u32,
    /// Consecutive one-sided fills that trip the safety gate (0 = disabled).
    pub one_sided_max_consecutive: u32,
    pub paper_initial_balance_usdc: f64,
    pub paper_liquidity_haircut: f64,
    pub arbitrage_paper_liquidity_haircut: f64,
    pub paper_second_leg_delay_ms: u64,
    /// Dry-run latency before re-reading the live book for Late-Resolution Winner Trading fills.
    pub paper_late_resolution_winner_trading_delay_ms: u64,
    /// Reject orderbook snapshots older than this many milliseconds.
    pub max_book_age_ms: i64,
    /// Reject YES/NO snapshots farther apart than this many milliseconds.
    pub max_book_skew_ms: i64,
    /// Suppress an identical market/price signal for this many milliseconds.
    pub signal_cooldown_ms: u64,
    /// Require an admissible full-depth edge across at least two independent
    /// book updates spanning this interval before execution.
    pub arbitrage_signal_confirmation_ms: u64,
    /// Read-only REST sampling offsets for price-positive PAPER candidates.
    pub arbitrage_rest_shadow_delays_ms: Vec<u64>,
    /// Per-leg USDC notionals evaluated from each concurrently fetched REST book pair.
    pub arbitrage_rest_shadow_notionals_usdc: Vec<f64>,
    pub state_journal_file: String,
    pub allow_unresolved_state: bool,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        dotenvy::dotenv().ok();

        // Parse proxy_address (optional)
        let proxy_address = match env::var("POLYMARKET_PROXY_ADDRESS") {
            Ok(addr) if !addr.trim().is_empty() => Some(
                addr.trim()
                    .parse()
                    .context("POLYMARKET_PROXY_ADDRESS has invalid format")?,
            ),
            _ => None,
        };

        let dry_run = env_bool("DRY_RUN", false)?;
        let mode = if dry_run { "paper" } else { "live" };
        let state_journal_file = env::var("BINARY_COMPLEMENT_ARBITRAGE_STATE_JOURNAL_FILE")
            .unwrap_or_else(|_| {
                format!("results/binary-complement-arbitrage/{mode}/orders.ndjson")
            });
        let late_resolution_winner_trading_state_journal_file =
            env::var("LATE_RESOLUTION_WINNER_TRADING_STATE_JOURNAL_FILE").unwrap_or_else(|_| {
                format!("results/late-resolution-winner-trading/{mode}/orders.ndjson")
            });
        let late_resolution_winner_trading_evaluation_journal_file =
            env::var("LATE_RESOLUTION_WINNER_TRADING_EVALUATION_JOURNAL_FILE").unwrap_or_else(
                |_| format!("results/late-resolution-winner-trading/{mode}/evaluations.ndjson"),
            );
        let risk_max_exposure_usdc =
            env_parse("BINARY_COMPLEMENT_ARBITRAGE_MAX_EXPOSURE_USDC", "1000.0")?;
        let paper_initial_balance_usdc = env_parse(
            "LATE_RESOLUTION_WINNER_TRADING_PAPER_INITIAL_BALANCE_USDC",
            "50.0",
        )?;
        let arbitrage_budget_usdc = env::var("BINARY_COMPLEMENT_ARBITRAGE_BUDGET_USDC")
            .ok()
            .map(|value| value.trim().parse::<f64>())
            .transpose()
            .context("BINARY_COMPLEMENT_ARBITRAGE_BUDGET_USDC has an invalid value")?
            .unwrap_or(risk_max_exposure_usdc);
        let late_resolution_winner_trading_budget_usdc =
            env::var("LATE_RESOLUTION_WINNER_TRADING_BUDGET_USDC")
                .ok()
                .map(|value| value.trim().parse::<f64>())
                .transpose()
                .context("LATE_RESOLUTION_WINNER_TRADING_BUDGET_USDC has an invalid value")?
                .unwrap_or(paper_initial_balance_usdc);
        let wallet_copy_trading_budget_usdc = env::var("WALLET_COPY_TRADING_BUDGET_USDC")
            .or_else(|_| env::var("WALLET_COPY_TRADING_INITIAL_NAV_USDC"))
            .unwrap_or_else(|_| "10000.0".into())
            .trim()
            .parse::<f64>()
            .context("WALLET_COPY_TRADING_BUDGET_USDC has an invalid value")?;

        let config = Config {
            private_key: normalize_private_key(
                &env::var("POLYMARKET_PRIVATE_KEY")
                    .context("POLYMARKET_PRIVATE_KEY must be set")?,
            ),
            proxy_address,
            clob_api_url: env::var("CLOB_API_URL")
                .unwrap_or_else(|_| CLOB_API_URL_DEFAULT.to_string()),
            signature_type: env::var("SIGNATURE_TYPE").unwrap_or_else(|_| "Poly1271".to_string()),
            min_profit_threshold: env_parse(
                "BINARY_COMPLEMENT_ARBITRAGE_MIN_PROFIT_THRESHOLD",
                "0.001",
            )?,
            min_ask_depth_usdc: env_parse(
                "BINARY_COMPLEMENT_ARBITRAGE_MIN_ASK_DEPTH_USDC",
                "10.0",
            )?,
            max_order_size_usdc: env_parse(
                "BINARY_COMPLEMENT_ARBITRAGE_MAX_ORDER_SIZE_USDC",
                "100.0",
            )?,
            crypto_symbols: env::var("CRYPTO_SYMBOLS")
                .unwrap_or_else(|_| "btc,eth,xrp,sol".to_string())
                .split(',')
                .map(|s| s.trim().to_lowercase())
                .collect(),
            market_refresh_advance_secs: env_parse("MARKET_REFRESH_ADVANCE_SECS", "5")?,
            market_period_minutes: env_parse("MARKET_PERIOD_MINUTES", "5")?,
            risk_max_exposure_usdc,
            arbitrage_budget_usdc,
            risk_imbalance_threshold: env_parse("RISK_IMBALANCE_THRESHOLD", "0.1")?,
            hedge_take_profit_pct: env_parse("HEDGE_TAKE_PROFIT_PCT", "0.05")?,
            hedge_stop_loss_pct: env_parse("HEDGE_STOP_LOSS_PCT", "0.05")?,
            arbitrage_execution_spread: env_parse(
                "BINARY_COMPLEMENT_ARBITRAGE_EXECUTION_SPREAD",
                "0.01",
            )?,
            arbitrage_fee_buffer: env_parse("BINARY_COMPLEMENT_ARBITRAGE_FEE_BUFFER", "0.002")?,
            arbitrage_dynamic_fee_gate_paper: env_bool(
                "BINARY_COMPLEMENT_ARBITRAGE_DYNAMIC_FEE_GATE_PAPER",
                false,
            )?,
            slippage: parse_slippage(
                &env::var("BINARY_COMPLEMENT_ARBITRAGE_SLIPPAGE")
                    .unwrap_or_else(|_| "0,0.01".to_string()),
            )?,
            gtd_expiration_secs: env_parse(
                "BINARY_COMPLEMENT_ARBITRAGE_GTD_EXPIRATION_SECS",
                "300",
            )?,
            arbitrage_order_type: parse_arbitrage_order_type(
                &env::var("BINARY_COMPLEMENT_ARBITRAGE_ORDER_TYPE")
                    .unwrap_or_else(|_| "GTD".to_string()),
            )?,
            stop_arbitrage_before_end_minutes: env_parse(
                "BINARY_COMPLEMENT_ARBITRAGE_STOP_BEFORE_END_MINUTES",
                "0",
            )?,
            arbitrage_enabled: env_bool("BINARY_COMPLEMENT_ARBITRAGE_ENABLED", true)?,
            late_resolution_winner_trading_enabled: env_bool(
                "LATE_RESOLUTION_WINNER_TRADING_ENABLED",
                false,
            )?,
            wallet_copy_trading_enabled: env_bool("WALLET_COPY_TRADING_ENABLED", false)?,
            late_resolution_winner_trading_budget_usdc,
            wallet_copy_trading_budget_usdc,
            late_resolution_winner_trading_before_end_secs: env_parse(
                "LATE_RESOLUTION_WINNER_TRADING_BEFORE_END_SECS",
                "30",
            )?,
            late_resolution_winner_trading_min_price: env_parse(
                "LATE_RESOLUTION_WINNER_TRADING_MIN_PRICE",
                "0.95",
            )?,
            late_resolution_winner_trading_max_price: env_parse(
                "LATE_RESOLUTION_WINNER_TRADING_MAX_PRICE",
                "0.99",
            )?,
            late_resolution_winner_trading_hybrid_paper_enabled: env_bool(
                "LATE_RESOLUTION_WINNER_TRADING_HYBRID_PAPER_ENABLED",
                false,
            )?,
            late_resolution_winner_trading_max_order_usdc: env_parse(
                "LATE_RESOLUTION_WINNER_TRADING_MAX_ORDER_USDC",
                "50.0",
            )?,
            late_resolution_winner_trading_max_per_window: env_parse(
                "LATE_RESOLUTION_WINNER_TRADING_MAX_PER_WINDOW",
                "1",
            )?,
            late_resolution_winner_trading_max_strategy_loss_usdc: env_parse(
                "LATE_RESOLUTION_WINNER_TRADING_MAX_STRATEGY_LOSS_USDC",
                "20.0",
            )?,
            late_resolution_winner_trading_max_asset_loss_usdc: env_parse(
                "LATE_RESOLUTION_WINNER_TRADING_MAX_ASSET_LOSS_USDC",
                "10.0",
            )?,
            late_resolution_winner_trading_max_consecutive_losses: env_parse(
                "LATE_RESOLUTION_WINNER_TRADING_MAX_CONSECUTIVE_LOSSES",
                "3",
            )?,
            late_resolution_winner_trading_auto_redeem: env_bool(
                "LATE_RESOLUTION_WINNER_TRADING_AUTO_REDEEM",
                false,
            )?,
            late_resolution_winner_trading_rtds_url: env::var(
                "LATE_RESOLUTION_WINNER_TRADING_RTDS_URL",
            )
            .unwrap_or_else(|_| "wss://ws-live-data.polymarket.com".to_string()),
            late_resolution_winner_trading_max_book_age_ms: env_parse(
                "LATE_RESOLUTION_WINNER_TRADING_MAX_BOOK_AGE_MS",
                "1000",
            )?,
            late_resolution_winner_trading_feed_max_age_ms: env_parse(
                "LATE_RESOLUTION_WINNER_TRADING_FEED_MAX_AGE_MS",
                "5000",
            )?,
            late_resolution_winner_trading_opening_max_skew_ms: env_parse(
                "LATE_RESOLUTION_WINNER_TRADING_OPENING_MAX_SKEW_MS",
                "5000",
            )?,
            late_resolution_winner_trading_volatility_lookback_secs: env_parse(
                "LATE_RESOLUTION_WINNER_TRADING_VOLATILITY_LOOKBACK_SECS",
                "120",
            )?,
            late_resolution_winner_trading_min_observations: env_parse(
                "LATE_RESOLUTION_WINNER_TRADING_MIN_OBSERVATIONS",
                "10",
            )?,
            late_resolution_winner_trading_confirmation_observations: env_parse(
                "LATE_RESOLUTION_WINNER_TRADING_CONFIRMATION_OBSERVATIONS",
                "3",
            )?,
            late_resolution_winner_trading_max_attempts_per_market: env_parse(
                "LATE_RESOLUTION_WINNER_TRADING_MAX_ATTEMPTS_PER_MARKET",
                "1",
            )?,
            late_resolution_winner_trading_min_margin_bps: env_parse(
                "LATE_RESOLUTION_WINNER_TRADING_MIN_MARGIN_BPS",
                "3.0",
            )?,
            late_resolution_winner_trading_reversal_sigma: env_parse(
                "LATE_RESOLUTION_WINNER_TRADING_REVERSAL_SIGMA",
                "2.0",
            )?,
            late_resolution_winner_trading_max_spread: env_parse(
                "LATE_RESOLUTION_WINNER_TRADING_MAX_SPREAD",
                "0.03",
            )?,
            late_resolution_winner_trading_min_depth_usdc: env_parse(
                "LATE_RESOLUTION_WINNER_TRADING_MIN_DEPTH_USDC",
                "10.0",
            )?,
            late_resolution_winner_trading_min_net_ev: env_parse(
                "LATE_RESOLUTION_WINNER_TRADING_MIN_NET_EV",
                "0.002",
            )?,
            late_resolution_winner_trading_btc_min_volatility_bps: env_parse(
                "LATE_RESOLUTION_WINNER_TRADING_BTC_MIN_VOLATILITY_BPS",
                "0.8",
            )?,
            late_resolution_winner_trading_eth_min_volatility_bps: env_parse(
                "LATE_RESOLUTION_WINNER_TRADING_ETH_MIN_VOLATILITY_BPS",
                "1.2",
            )?,
            late_resolution_winner_trading_other_min_volatility_bps: env_parse(
                "LATE_RESOLUTION_WINNER_TRADING_OTHER_MIN_VOLATILITY_BPS",
                "1.5",
            )?,
            late_resolution_winner_trading_btc_model_risk_buffer: env_parse(
                "LATE_RESOLUTION_WINNER_TRADING_BTC_MODEL_RISK_BUFFER",
                "0.01",
            )?,
            late_resolution_winner_trading_eth_model_risk_buffer: env_parse(
                "LATE_RESOLUTION_WINNER_TRADING_ETH_MODEL_RISK_BUFFER",
                "0.015",
            )?,
            late_resolution_winner_trading_other_model_risk_buffer: env_parse(
                "LATE_RESOLUTION_WINNER_TRADING_OTHER_MODEL_RISK_BUFFER",
                "0.02",
            )?,
            merge_interval_minutes: env_parse("MERGE_INTERVAL_MINUTES", "0")?,
            min_yes_price_threshold: env_parse(
                "BINARY_COMPLEMENT_ARBITRAGE_MIN_YES_PRICE_THRESHOLD",
                "0.0",
            )?,
            min_no_price_threshold: env_parse(
                "BINARY_COMPLEMENT_ARBITRAGE_MIN_NO_PRICE_THRESHOLD",
                "0.0",
            )?,
            position_sync_interval_secs: env_parse("POSITION_SYNC_INTERVAL_SECS", "10")?,
            position_balance_interval_secs: env_parse("POSITION_BALANCE_INTERVAL_SECS", "60")?,
            position_balance_threshold: env_parse("POSITION_BALANCE_THRESHOLD", "2.0")?,
            position_balance_min_total: env_parse("POSITION_BALANCE_MIN_TOTAL", "5.0")?,
            wind_down_before_window_end_minutes: env_parse(
                "WIND_DOWN_BEFORE_WINDOW_END_MINUTES",
                "0",
            )?,
            wind_down_sell_price: env_parse("WIND_DOWN_SELL_PRICE", "0.01")?,
            arbitrage_log_file: env::var("BINARY_COMPLEMENT_ARBITRAGE_LOG_FILE")
                .ok()
                .filter(|value| !value.trim().is_empty()),
            arbitrage_shadow_log_file: env::var("BINARY_COMPLEMENT_ARBITRAGE_SHADOW_LOG_FILE")
                .ok()
                .filter(|value| !value.trim().is_empty()),
            late_resolution_winner_trading_state_journal_file,
            late_resolution_winner_trading_evaluation_journal_file,
            dry_run,
            canary_mode: env_bool("CANARY_MODE", false)?,
            canary_max_pair_notional_usdc: env_parse("CANARY_MAX_PAIR_NOTIONAL_USDC", "3.0")?,
            canary_max_total_notional_usdc: env_parse("CANARY_MAX_TOTAL_NOTIONAL_USDC", "3.0")?,
            canary_max_loss_usdc: env_parse("CANARY_MAX_LOSS_USDC", "2.0")?,
            canary_max_pairs_per_window: env_parse("CANARY_MAX_PAIRS_PER_WINDOW", "1")?,
            canary_max_pairs_total: env_parse("CANARY_MAX_PAIRS_TOTAL", "1")?,
            one_sided_max_consecutive: env_parse("ONE_SIDED_MAX_CONSECUTIVE", "3")?,
            paper_initial_balance_usdc,
            paper_liquidity_haircut: env_parse(
                "LATE_RESOLUTION_WINNER_TRADING_PAPER_LIQUIDITY_HAIRCUT",
                "0.75",
            )?,
            arbitrage_paper_liquidity_haircut: env_parse(
                "BINARY_COMPLEMENT_ARBITRAGE_PAPER_LIQUIDITY_HAIRCUT",
                "0.75",
            )?,
            paper_second_leg_delay_ms: env_parse(
                "BINARY_COMPLEMENT_ARBITRAGE_PAPER_SECOND_LEG_DELAY_MS",
                "250",
            )?,
            paper_late_resolution_winner_trading_delay_ms: env_parse(
                "LATE_RESOLUTION_WINNER_TRADING_PAPER_EXECUTION_DELAY_MS",
                "250",
            )?,
            max_book_age_ms: env_parse("BINARY_COMPLEMENT_ARBITRAGE_MAX_BOOK_AGE_MS", "2000")?,
            max_book_skew_ms: env_parse("BINARY_COMPLEMENT_ARBITRAGE_MAX_BOOK_SKEW_MS", "1000")?,
            signal_cooldown_ms: env_parse(
                "BINARY_COMPLEMENT_ARBITRAGE_SIGNAL_COOLDOWN_MS",
                "3000",
            )?,
            arbitrage_signal_confirmation_ms: env_parse(
                "BINARY_COMPLEMENT_ARBITRAGE_SIGNAL_CONFIRMATION_MS",
                "100",
            )?,
            arbitrage_rest_shadow_delays_ms: parse_millisecond_list(
                "BINARY_COMPLEMENT_ARBITRAGE_REST_SHADOW_DELAYS_MS",
                &env::var("BINARY_COMPLEMENT_ARBITRAGE_REST_SHADOW_DELAYS_MS")
                    .unwrap_or_else(|_| "0,50,100,250".to_string()),
            )?,
            arbitrage_rest_shadow_notionals_usdc: parse_positive_decimal_list(
                "BINARY_COMPLEMENT_ARBITRAGE_REST_SHADOW_NOTIONALS_USDC",
                &env::var("BINARY_COMPLEMENT_ARBITRAGE_REST_SHADOW_NOTIONALS_USDC")
                    .unwrap_or_else(|_| "1,2,3,5".to_string()),
            )?,
            state_journal_file,
            allow_unresolved_state: env_bool("ALLOW_UNRESOLVED_STATE", false)?,
        };
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        if self.private_key.is_empty() {
            bail!("POLYMARKET_PRIVATE_KEY cannot be empty");
        }
        let is_hex64 =
            self.private_key.len() == 64 && self.private_key.chars().all(|c| c.is_ascii_hexdigit());
        if !is_hex64 {
            bail!(
                "POLYMARKET_PRIVATE_KEY must be a 64-character hex string without 0x prefix (got {} chars)",
                self.private_key.len()
            );
        }
        if self.crypto_symbols.is_empty() || self.crypto_symbols.iter().any(String::is_empty) {
            bail!("CRYPTO_SYMBOLS must contain at least one non-empty symbol");
        }
        let signature = self.signature_type.trim().to_ascii_lowercase();
        let valid_signature = matches!(
            signature.as_str(),
            "eoa"
                | "0"
                | "proxy"
                | "magic"
                | "email"
                | "gnosissafe"
                | "safe"
                | "poly1271"
                | "deposit"
                | "deposit_wallet"
                | "3"
        );
        if !valid_signature {
            bail!(
                "SIGNATURE_TYPE has unsupported value {:?}",
                self.signature_type
            );
        }
        for (name, value) in [
            (
                "BINARY_COMPLEMENT_ARBITRAGE_MIN_PROFIT_THRESHOLD",
                self.min_profit_threshold,
            ),
            (
                "BINARY_COMPLEMENT_ARBITRAGE_EXECUTION_SPREAD",
                self.arbitrage_execution_spread,
            ),
            (
                "BINARY_COMPLEMENT_ARBITRAGE_FEE_BUFFER",
                self.arbitrage_fee_buffer,
            ),
            (
                "BINARY_COMPLEMENT_ARBITRAGE_MIN_YES_PRICE_THRESHOLD",
                self.min_yes_price_threshold,
            ),
            (
                "BINARY_COMPLEMENT_ARBITRAGE_MIN_NO_PRICE_THRESHOLD",
                self.min_no_price_threshold,
            ),
            ("HEDGE_TAKE_PROFIT_PCT", self.hedge_take_profit_pct),
            ("HEDGE_STOP_LOSS_PCT", self.hedge_stop_loss_pct),
        ] {
            if !value.is_finite() || !(0.0..1.0).contains(&value) {
                bail!("{name} must be finite and in [0, 1)");
            }
        }
        for (name, value) in [
            (
                "BINARY_COMPLEMENT_ARBITRAGE_MAX_ORDER_SIZE_USDC",
                self.max_order_size_usdc,
            ),
            (
                "BINARY_COMPLEMENT_ARBITRAGE_MIN_ASK_DEPTH_USDC",
                self.min_ask_depth_usdc,
            ),
            (
                "BINARY_COMPLEMENT_ARBITRAGE_MAX_EXPOSURE_USDC",
                self.risk_max_exposure_usdc,
            ),
            (
                "BINARY_COMPLEMENT_ARBITRAGE_BUDGET_USDC",
                self.arbitrage_budget_usdc,
            ),
            (
                "LATE_RESOLUTION_WINNER_TRADING_BUDGET_USDC",
                self.late_resolution_winner_trading_budget_usdc,
            ),
            (
                "WALLET_COPY_TRADING_BUDGET_USDC",
                self.wallet_copy_trading_budget_usdc,
            ),
            (
                "POSITION_BALANCE_THRESHOLD",
                self.position_balance_threshold,
            ),
            (
                "POSITION_BALANCE_MIN_TOTAL",
                self.position_balance_min_total,
            ),
            (
                "CANARY_MAX_PAIR_NOTIONAL_USDC",
                self.canary_max_pair_notional_usdc,
            ),
            (
                "CANARY_MAX_TOTAL_NOTIONAL_USDC",
                self.canary_max_total_notional_usdc,
            ),
            ("CANARY_MAX_LOSS_USDC", self.canary_max_loss_usdc),
            (
                "LATE_RESOLUTION_WINNER_TRADING_PAPER_INITIAL_BALANCE_USDC",
                self.paper_initial_balance_usdc,
            ),
            (
                "LATE_RESOLUTION_WINNER_TRADING_MAX_STRATEGY_LOSS_USDC",
                self.late_resolution_winner_trading_max_strategy_loss_usdc,
            ),
            (
                "LATE_RESOLUTION_WINNER_TRADING_MAX_ASSET_LOSS_USDC",
                self.late_resolution_winner_trading_max_asset_loss_usdc,
            ),
        ] {
            if !value.is_finite() || value <= 0.0 {
                bail!("{name} must be greater than 0");
            }
        }
        if !self.paper_liquidity_haircut.is_finite()
            || !(0.0..=1.0).contains(&self.paper_liquidity_haircut)
            || self.paper_liquidity_haircut == 0.0
        {
            bail!("LATE_RESOLUTION_WINNER_TRADING_PAPER_LIQUIDITY_HAIRCUT must be in (0, 1]");
        }
        if !self.arbitrage_paper_liquidity_haircut.is_finite()
            || !(0.0..=1.0).contains(&self.arbitrage_paper_liquidity_haircut)
            || self.arbitrage_paper_liquidity_haircut == 0.0
        {
            bail!("BINARY_COMPLEMENT_ARBITRAGE_PAPER_LIQUIDITY_HAIRCUT must be in (0, 1]");
        }
        if self.paper_second_leg_delay_ms > 30_000
            || self.paper_late_resolution_winner_trading_delay_ms > 30_000
        {
            bail!("paper execution delays cannot exceed 30000ms");
        }
        if self.canary_max_pair_notional_usdc > ABSOLUTE_CANARY_PAIR_LIMIT_USDC
            || self.canary_max_total_notional_usdc > ABSOLUTE_CANARY_TOTAL_LIMIT_USDC
            || self.canary_max_loss_usdc > ABSOLUTE_CANARY_LOSS_LIMIT_USDC
        {
            bail!(
                "canary limits exceed absolute safety ceilings: pair<=10, total<=20, loss<=5 USDC"
            );
        }
        if self.canary_max_pair_notional_usdc > self.canary_max_total_notional_usdc {
            bail!("CANARY_MAX_PAIR_NOTIONAL_USDC cannot exceed CANARY_MAX_TOTAL_NOTIONAL_USDC");
        }
        if self.late_resolution_winner_trading_max_asset_loss_usdc
            > self.late_resolution_winner_trading_max_strategy_loss_usdc
        {
            bail!("LATE_RESOLUTION_WINNER_TRADING_MAX_ASSET_LOSS_USDC cannot exceed LATE_RESOLUTION_WINNER_TRADING_MAX_STRATEGY_LOSS_USDC");
        }
        if self.max_order_size_usdc > self.arbitrage_budget_usdc {
            bail!("BINARY_COMPLEMENT_ARBITRAGE_MAX_ORDER_SIZE_USDC cannot exceed BINARY_COMPLEMENT_ARBITRAGE_BUDGET_USDC");
        }
        if self.arbitrage_dynamic_fee_gate_paper && !self.dry_run {
            bail!("BINARY_COMPLEMENT_ARBITRAGE_DYNAMIC_FEE_GATE_PAPER can only be enabled with DRY_RUN=true");
        }
        if self.late_resolution_winner_trading_max_order_usdc
            > self.late_resolution_winner_trading_budget_usdc
        {
            bail!("LATE_RESOLUTION_WINNER_TRADING_MAX_ORDER_USDC cannot exceed LATE_RESOLUTION_WINNER_TRADING_BUDGET_USDC");
        }
        if !self.late_resolution_winner_trading_min_price.is_finite()
            || !self.late_resolution_winner_trading_max_price.is_finite()
            || self.late_resolution_winner_trading_min_price <= 0.0
            || self.late_resolution_winner_trading_min_price
                > self.late_resolution_winner_trading_max_price
            || self.late_resolution_winner_trading_max_price >= 1.0
        {
            bail!("Late-Resolution Winner Trading prices require 0 < LATE_RESOLUTION_WINNER_TRADING_MIN_PRICE <= LATE_RESOLUTION_WINNER_TRADING_MAX_PRICE < 1");
        }
        if self.late_resolution_winner_trading_hybrid_paper_enabled && !self.dry_run {
            bail!("LATE_RESOLUTION_WINNER_TRADING_HYBRID_PAPER_ENABLED can only be enabled with DRY_RUN=true");
        }
        if self.canary_max_pairs_per_window == 0
            || self.canary_max_pairs_per_window > 1
            || self.canary_max_pairs_total == 0
            || self.canary_max_pairs_total > 3
        {
            bail!("canary pair limits require per-window=1 and total in 1..=3");
        }
        if self.canary_mode
            && !self.dry_run
            && !matches!(self.arbitrage_order_type, OrderType::FOK | OrderType::FAK)
        {
            bail!("live CANARY_MODE requires BINARY_COMPLEMENT_ARBITRAGE_ORDER_TYPE=FOK or FAK");
        }
        if self
            .slippage
            .iter()
            .any(|v| !v.is_finite() || *v <= -1.0 || *v >= 1.0)
        {
            bail!("BINARY_COMPLEMENT_ARBITRAGE_SLIPPAGE values must be finite and in (-1, 1)");
        }
        if !(0.0..=1.0).contains(&self.wind_down_sell_price) {
            bail!("WIND_DOWN_SELL_PRICE must be in [0, 1]");
        }
        if matches!(self.arbitrage_order_type, OrderType::GTD) && self.gtd_expiration_secs == 0 {
            bail!("BINARY_COMPLEMENT_ARBITRAGE_GTD_EXPIRATION_SECS must be greater than 0 for GTD orders");
        }
        if self
            .arbitrage_execution_spread
            .max(self.min_profit_threshold)
            + self.arbitrage_fee_buffer
            >= 1.0
        {
            bail!("max(BINARY_COMPLEMENT_ARBITRAGE_EXECUTION_SPREAD, BINARY_COMPLEMENT_ARBITRAGE_MIN_PROFIT_THRESHOLD) + BINARY_COMPLEMENT_ARBITRAGE_FEE_BUFFER must be below 1");
        }
        if self.market_refresh_advance_secs > 300 {
            bail!("MARKET_REFRESH_ADVANCE_SECS cannot exceed the 300-second window");
        }
        if self.max_book_age_ms <= 0 || self.max_book_skew_ms < 0 {
            bail!("BINARY_COMPLEMENT_ARBITRAGE_MAX_BOOK_AGE_MS must be positive and BINARY_COMPLEMENT_ARBITRAGE_MAX_BOOK_SKEW_MS cannot be negative");
        }
        if self
            .arbitrage_rest_shadow_delays_ms
            .last()
            .is_some_and(|delay| *delay > 10_000)
        {
            bail!("BINARY_COMPLEMENT_ARBITRAGE_REST_SHADOW_DELAYS_MS values cannot exceed 10000ms");
        }
        if self
            .arbitrage_rest_shadow_notionals_usdc
            .last()
            .is_some_and(|notional| *notional > self.arbitrage_budget_usdc)
        {
            bail!("BINARY_COMPLEMENT_ARBITRAGE_REST_SHADOW_NOTIONALS_USDC cannot exceed BINARY_COMPLEMENT_ARBITRAGE_BUDGET_USDC");
        }
        if self.state_journal_file.trim().is_empty() {
            bail!("BINARY_COMPLEMENT_ARBITRAGE_STATE_JOURNAL_FILE cannot be empty");
        }
        if self
            .late_resolution_winner_trading_state_journal_file
            .trim()
            .is_empty()
        {
            bail!("LATE_RESOLUTION_WINNER_TRADING_STATE_JOURNAL_FILE cannot be empty");
        }
        if self
            .late_resolution_winner_trading_evaluation_journal_file
            .trim()
            .is_empty()
        {
            bail!("LATE_RESOLUTION_WINNER_TRADING_EVALUATION_JOURNAL_FILE cannot be empty");
        }
        if !self
            .late_resolution_winner_trading_rtds_url
            .starts_with("wss://")
        {
            bail!("LATE_RESOLUTION_WINNER_TRADING_RTDS_URL must use wss://");
        }
        if self.late_resolution_winner_trading_max_book_age_ms <= 0
            || self.late_resolution_winner_trading_max_book_age_ms > 30_000
            || self.late_resolution_winner_trading_feed_max_age_ms <= 0
            || self.late_resolution_winner_trading_opening_max_skew_ms < 0
            || self.late_resolution_winner_trading_volatility_lookback_secs == 0
            || self.late_resolution_winner_trading_min_observations < 2
            || self.late_resolution_winner_trading_confirmation_observations == 0
            || self.late_resolution_winner_trading_max_attempts_per_market == 0
        {
            bail!("Late-Resolution Winner Trading book/model timing, observation and attempt limits are invalid");
        }
        for (name, value) in [
            (
                "LATE_RESOLUTION_WINNER_TRADING_MIN_MARGIN_BPS",
                self.late_resolution_winner_trading_min_margin_bps,
            ),
            (
                "LATE_RESOLUTION_WINNER_TRADING_REVERSAL_SIGMA",
                self.late_resolution_winner_trading_reversal_sigma,
            ),
            (
                "LATE_RESOLUTION_WINNER_TRADING_BTC_MIN_VOLATILITY_BPS",
                self.late_resolution_winner_trading_btc_min_volatility_bps,
            ),
            (
                "LATE_RESOLUTION_WINNER_TRADING_ETH_MIN_VOLATILITY_BPS",
                self.late_resolution_winner_trading_eth_min_volatility_bps,
            ),
            (
                "LATE_RESOLUTION_WINNER_TRADING_OTHER_MIN_VOLATILITY_BPS",
                self.late_resolution_winner_trading_other_min_volatility_bps,
            ),
            (
                "LATE_RESOLUTION_WINNER_TRADING_MIN_DEPTH_USDC",
                self.late_resolution_winner_trading_min_depth_usdc,
            ),
        ] {
            if !value.is_finite() || value <= 0.0 {
                bail!("{name} must be finite and greater than 0");
            }
        }
        for (name, value) in [
            (
                "LATE_RESOLUTION_WINNER_TRADING_MAX_SPREAD",
                self.late_resolution_winner_trading_max_spread,
            ),
            (
                "LATE_RESOLUTION_WINNER_TRADING_MIN_NET_EV",
                self.late_resolution_winner_trading_min_net_ev,
            ),
            (
                "LATE_RESOLUTION_WINNER_TRADING_BTC_MODEL_RISK_BUFFER",
                self.late_resolution_winner_trading_btc_model_risk_buffer,
            ),
            (
                "LATE_RESOLUTION_WINNER_TRADING_ETH_MODEL_RISK_BUFFER",
                self.late_resolution_winner_trading_eth_model_risk_buffer,
            ),
            (
                "LATE_RESOLUTION_WINNER_TRADING_OTHER_MODEL_RISK_BUFFER",
                self.late_resolution_winner_trading_other_model_risk_buffer,
            ),
        ] {
            if !value.is_finite() || !(0.0..1.0).contains(&value) {
                bail!("{name} must be finite and in [0, 1)");
            }
        }
        if self.stop_arbitrage_before_end_minutes > 5
            || self.wind_down_before_window_end_minutes > 5
        {
            bail!("stop-arbitrage and wind-down minutes cannot exceed the 5-minute market window");
        }
        validate_strategy_compatibility(
            self.dry_run,
            self.late_resolution_winner_trading_enabled,
            self.wind_down_before_window_end_minutes,
        )?;
        if self.proxy_address.is_none()
            && (!(signature == "eoa" || signature == "0")
                || self.position_sync_interval_secs > 0
                || self.merge_interval_minutes > 0
                || self.wind_down_before_window_end_minutes > 0)
        {
            bail!("POLYMARKET_PROXY_ADDRESS is required for non-EOA trading and for position sync, Merge or wind-down");
        }
        if !self.dry_run
            && (self.merge_interval_minutes > 0 || self.wind_down_before_window_end_minutes > 0)
            && [
                "POLY_BUILDER_API_KEY",
                "POLY_BUILDER_SECRET",
                "POLY_BUILDER_PASSPHRASE",
            ]
            .iter()
            .any(|name| env::var(name).map(|v| v.trim().is_empty()).unwrap_or(true))
        {
            bail!("scheduled Merge or wind-down requires all POLY_BUILDER_* credentials");
        }
        if !self.dry_run
            && self.late_resolution_winner_trading_enabled
            && self.late_resolution_winner_trading_auto_redeem
        {
            if self.proxy_address.is_none() {
                bail!("live LATE_RESOLUTION_WINNER_TRADING_AUTO_REDEEM requires POLYMARKET_PROXY_ADDRESS");
            }
            if [
                "POLY_BUILDER_API_KEY",
                "POLY_BUILDER_SECRET",
                "POLY_BUILDER_PASSPHRASE",
            ]
            .iter()
            .any(|name| env::var(name).map(|v| v.trim().is_empty()).unwrap_or(true))
            {
                bail!("live LATE_RESOLUTION_WINNER_TRADING_AUTO_REDEEM requires all POLY_BUILDER_* credentials");
            }
        }
        Ok(())
    }
}

fn validate_strategy_compatibility(
    dry_run: bool,
    late_resolution_winner_trading_enabled: bool,
    wind_down_before_window_end_minutes: u64,
) -> Result<()> {
    if !dry_run && late_resolution_winner_trading_enabled && wind_down_before_window_end_minutes > 0
    {
        bail!("live LATE_RESOLUTION_WINNER_TRADING_ENABLED and WIND_DOWN_BEFORE_WINDOW_END_MINUTES cannot be enabled together");
    }
    Ok(())
}

fn env_bool(name: &str, default: bool) -> Result<bool> {
    let raw = match env::var(name) {
        Ok(value) => value,
        Err(_) => return Ok(default),
    };
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => bail!("{name} must be true/false, 1/0, yes/no or on/off"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unknown_order_type() {
        assert!(parse_arbitrage_order_type("IOC").is_err());
    }

    #[test]
    fn parses_one_or_two_slippage_values() {
        assert_eq!(parse_slippage("0.01").unwrap(), [0.01, 0.01]);
        assert_eq!(parse_slippage("-0.01,0.02").unwrap(), [-0.01, 0.02]);
        assert!(parse_slippage("0,1,2").is_err());
        assert!(parse_slippage("bad").is_err());
    }

    #[test]
    fn parses_sorted_unique_shadow_delays() {
        assert_eq!(
            parse_millisecond_list("TEST_DELAYS", "250, 0,50,50,100").unwrap(),
            vec![0, 50, 100, 250]
        );
        assert!(parse_millisecond_list("TEST_DELAYS", "0,bad").is_err());
    }

    #[test]
    fn parses_sorted_unique_shadow_notionals() {
        assert_eq!(
            parse_positive_decimal_list("TEST_NOTIONALS", "5, 1,2,2,3").unwrap(),
            vec![1.0, 2.0, 3.0, 5.0]
        );
        assert!(parse_positive_decimal_list("TEST_NOTIONALS", "1,0").is_err());
        assert!(parse_positive_decimal_list("TEST_NOTIONALS", "1,bad").is_err());
    }

    #[test]
    fn parses_strict_boolean_values() {
        std::env::set_var("TEST_BOOL_CONFIG", "yes");
        assert!(env_bool("TEST_BOOL_CONFIG", false).unwrap());
        std::env::set_var("TEST_BOOL_CONFIG", "maybe");
        assert!(env_bool("TEST_BOOL_CONFIG", false).is_err());
        std::env::remove_var("TEST_BOOL_CONFIG");
    }

    #[test]
    fn normalizes_private_key_prefix_and_case() {
        assert_eq!(
            normalize_private_key("0xABC123"),
            "abc123",
            "strips 0x and lowercases"
        );
        assert_eq!(
            normalize_private_key("0XABC123"),
            "abc123",
            "strips uppercase 0X prefix"
        );
        assert_eq!(normalize_private_key("  abc123  "), "abc123");
        assert_eq!(normalize_private_key("abc123"), "abc123");
    }

    #[test]
    fn live_late_resolution_winner_trading_and_wind_down_are_mutually_exclusive() {
        assert!(validate_strategy_compatibility(false, true, 1).is_err());
        assert!(validate_strategy_compatibility(true, true, 1).is_ok());
        assert!(validate_strategy_compatibility(false, true, 0).is_ok());
        assert!(validate_strategy_compatibility(false, false, 1).is_ok());
    }
}
