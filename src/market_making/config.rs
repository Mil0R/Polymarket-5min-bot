use anyhow::{bail, Context, Result};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::env;
use std::path::PathBuf;
use std::str::FromStr;

#[derive(Debug, Clone)]
pub struct MarketMakingResearchConfig {
    pub clob_url: String,
    pub gamma_url: String,
    pub data_url: String,
    pub results_file: PathBuf,
    pub reward_audit_file: PathBuf,
    pub reward_audit_enabled: bool,
    pub reward_poll_secs: u64,
    pub max_markets: usize,
    pub discovery_pages: usize,
    pub enrichment_limit: usize,
    pub request_concurrency: usize,
    pub min_daily_reward_usdc: Decimal,
    pub max_rewards_min_size: Decimal,
    pub min_liquidity_usdc: Decimal,
    pub min_volume_24h_usdc: Decimal,
    pub max_fee_rate: Decimal,
    pub high_fee_threshold: Decimal,
    pub high_fee_volume_multiplier: Decimal,
    pub max_observed_spread: Decimal,
    pub min_midpoint: Decimal,
    pub max_midpoint: Decimal,
    pub market_filter: Option<String>,
    pub quote_size_shares: Decimal,
    pub quote_half_spread: Decimal,
    pub quote_half_spread_grid: Vec<Decimal>,
    pub dynamic_quote_distance: bool,
    pub max_midpoint_move: Decimal,
    pub midpoint_move_window_secs: u64,
    pub min_trades_1h: usize,
    pub book_activity_window_secs: u64,
    pub min_book_updates: usize,
    pub max_inventory_shares: Decimal,
    pub inventory_skew_per_share: Decimal,
    pub max_consecutive_negative_markouts: u32,
    pub max_quotes_per_market_hour: usize,
    pub reward_band_fraction: Decimal,
    pub book_poll_secs: u64,
    pub quote_interval_secs: u64,
    pub market_refresh_secs: u64,
    pub min_market_residency_secs: u64,
    pub replacement_score_margin: Decimal,
    pub market_cooldown_secs: u64,
    pub max_consecutive_market_errors: u32,
    pub min_time_to_end_secs: u64,
    pub http_max_retries: usize,
    pub retry_base_ms: u64,
    pub trade_cache_secs: u64,
    pub websocket_books: bool,
    pub websocket_book_max_age_ms: i64,
    pub markout_horizons_secs: Vec<u64>,
    pub min_independent_fill_samples: usize,
    pub min_reward_capture_ratio: Decimal,
    pub target_reward_capture_ratio: Decimal,
    pub reward_break_even_margin_ratio: Decimal,
    pub canary_max_markets: usize,
    pub canary_max_quote_notional_usdc: Decimal,
    pub canary_max_inventory_usdc: Decimal,
    pub canary_max_midpoint_move: Decimal,
    pub news_halt_file: PathBuf,
    pub duration_secs: u64,
}

impl MarketMakingResearchConfig {
    pub fn from_env() -> Result<Self> {
        let config = Self {
            clob_url: env::var("CLOB_API_URL")
                .unwrap_or_else(|_| "https://clob.polymarket.com".to_string()),
            gamma_url: env::var("GAMMA_API_URL")
                .unwrap_or_else(|_| "https://gamma-api.polymarket.com".to_string()),
            data_url: env::var("DATA_API_URL")
                .unwrap_or_else(|_| "https://data-api.polymarket.com".to_string()),
            results_file: PathBuf::from(env::var("MARKET_MAKING_RESULTS_FILE").unwrap_or_else(
                |_| "results/market-making/shadow-run-003/events.ndjson".to_string(),
            )),
            reward_audit_file: PathBuf::from(
                env::var("MARKET_MAKING_REWARD_AUDIT_FILE").unwrap_or_else(|_| {
                    "results/market-making/reward-audit/snapshots.ndjson".to_string()
                }),
            ),
            reward_audit_enabled: parse("MARKET_MAKING_REWARD_AUDIT_ENABLED", "false")?,
            reward_poll_secs: parse("MARKET_MAKING_REWARD_POLL_SECS", "3600")?,
            max_markets: parse("MARKET_MAKING_MAX_MARKETS", "5")?,
            discovery_pages: parse("MARKET_MAKING_DISCOVERY_PAGES", "3")?,
            enrichment_limit: parse("MARKET_MAKING_ENRICHMENT_LIMIT", "100")?,
            request_concurrency: parse("MARKET_MAKING_REQUEST_CONCURRENCY", "8")?,
            min_daily_reward_usdc: decimal_env("MARKET_MAKING_MIN_DAILY_REWARD_USDC", dec!(3))?,
            max_rewards_min_size: decimal_env("MARKET_MAKING_MAX_REWARDS_MIN_SIZE", dec!(100))?,
            min_liquidity_usdc: decimal_env("MARKET_MAKING_MIN_LIQUIDITY_USDC", dec!(50))?,
            min_volume_24h_usdc: decimal_env("MARKET_MAKING_MIN_VOLUME_24H_USDC", dec!(1000))?,
            max_fee_rate: decimal_env("MARKET_MAKING_MAX_FEE_RATE", dec!(0.05))?,
            high_fee_threshold: decimal_env("MARKET_MAKING_HIGH_FEE_THRESHOLD", dec!(0.04))?,
            high_fee_volume_multiplier: decimal_env(
                "MARKET_MAKING_HIGH_FEE_VOLUME_MULTIPLIER",
                dec!(2),
            )?,
            max_observed_spread: decimal_env("MARKET_MAKING_MAX_OBSERVED_SPREAD", dec!(0.12))?,
            min_midpoint: decimal_env("MARKET_MAKING_MIN_MIDPOINT", dec!(0.10))?,
            max_midpoint: decimal_env("MARKET_MAKING_MAX_MIDPOINT", dec!(0.90))?,
            market_filter: env::var("MARKET_MAKING_MARKET_FILTER")
                .ok()
                .map(|value| value.trim().to_ascii_lowercase())
                .filter(|value| !value.is_empty()),
            quote_size_shares: decimal_env("MARKET_MAKING_QUOTE_SIZE_SHARES", dec!(20))?,
            quote_half_spread: decimal_env("MARKET_MAKING_QUOTE_HALF_SPREAD", dec!(0.01))?,
            quote_half_spread_grid: parse_decimal_list(
                "MARKET_MAKING_QUOTE_HALF_SPREAD_GRID",
                &env::var("MARKET_MAKING_QUOTE_HALF_SPREAD_GRID")
                    .unwrap_or_else(|_| "0.01,0.02,0.03".to_string()),
            )?,
            dynamic_quote_distance: parse("MARKET_MAKING_DYNAMIC_QUOTE_DISTANCE", "true")?,
            max_midpoint_move: decimal_env("MARKET_MAKING_MAX_MIDPOINT_MOVE", dec!(0.02))?,
            midpoint_move_window_secs: parse("MARKET_MAKING_MIDPOINT_MOVE_WINDOW_SECS", "30")?,
            min_trades_1h: parse("MARKET_MAKING_MIN_TRADES_1H", "3")?,
            book_activity_window_secs: parse("MARKET_MAKING_BOOK_ACTIVITY_WINDOW_SECS", "300")?,
            min_book_updates: parse("MARKET_MAKING_MIN_BOOK_UPDATES", "3")?,
            max_inventory_shares: decimal_env("MARKET_MAKING_MAX_INVENTORY_SHARES", dec!(40))?,
            inventory_skew_per_share: decimal_env(
                "MARKET_MAKING_INVENTORY_SKEW_PER_SHARE",
                dec!(0.00025),
            )?,
            max_consecutive_negative_markouts: parse(
                "MARKET_MAKING_MAX_CONSECUTIVE_NEGATIVE_MARKOUTS",
                "3",
            )?,
            max_quotes_per_market_hour: parse("MARKET_MAKING_MAX_QUOTES_PER_MARKET_HOUR", "120")?,
            reward_band_fraction: decimal_env("MARKET_MAKING_REWARD_BAND_FRACTION", dec!(0.8))?,
            book_poll_secs: parse("MARKET_MAKING_BOOK_POLL_SECS", "5")?,
            quote_interval_secs: parse("MARKET_MAKING_QUOTE_INTERVAL_SECS", "30")?,
            market_refresh_secs: parse("MARKET_MAKING_MARKET_REFRESH_SECS", "300")?,
            min_market_residency_secs: parse("MARKET_MAKING_MIN_MARKET_RESIDENCY_SECS", "1800")?,
            replacement_score_margin: decimal_env(
                "MARKET_MAKING_REPLACEMENT_SCORE_MARGIN",
                dec!(0.15),
            )?,
            market_cooldown_secs: parse("MARKET_MAKING_MARKET_COOLDOWN_SECS", "1800")?,
            max_consecutive_market_errors: parse(
                "MARKET_MAKING_MAX_CONSECUTIVE_MARKET_ERRORS",
                "6",
            )?,
            min_time_to_end_secs: parse("MARKET_MAKING_MIN_TIME_TO_END_SECS", "1800")?,
            http_max_retries: parse("MARKET_MAKING_HTTP_MAX_RETRIES", "3")?,
            retry_base_ms: parse("MARKET_MAKING_RETRY_BASE_MS", "250")?,
            trade_cache_secs: parse("MARKET_MAKING_TRADE_CACHE_SECS", "5")?,
            websocket_books: parse("MARKET_MAKING_WEBSOCKET_BOOKS", "true")?,
            websocket_book_max_age_ms: parse("MARKET_MAKING_WEBSOCKET_BOOK_MAX_AGE_MS", "15000")?,
            markout_horizons_secs: parse_horizons(
                &env::var("MARKET_MAKING_MARKOUT_HORIZONS_SECS")
                    .unwrap_or_else(|_| "1,5,30,60,300,1800".to_string()),
            )?,
            min_independent_fill_samples: parse(
                "MARKET_MAKING_MIN_INDEPENDENT_FILL_SAMPLES",
                "100",
            )?,
            min_reward_capture_ratio: decimal_env(
                "MARKET_MAKING_MIN_REWARD_CAPTURE_RATIO",
                dec!(0.01),
            )?,
            target_reward_capture_ratio: decimal_env(
                "MARKET_MAKING_TARGET_REWARD_CAPTURE_RATIO",
                dec!(0.05),
            )?,
            reward_break_even_margin_ratio: decimal_env(
                "MARKET_MAKING_REWARD_BREAK_EVEN_MARGIN_RATIO",
                dec!(0.02),
            )?,
            canary_max_markets: parse("MARKET_MAKING_CANARY_MAX_MARKETS", "2")?,
            canary_max_quote_notional_usdc: decimal_env(
                "MARKET_MAKING_CANARY_MAX_QUOTE_NOTIONAL_USDC",
                dec!(5),
            )?,
            canary_max_inventory_usdc: decimal_env(
                "MARKET_MAKING_CANARY_MAX_INVENTORY_USDC",
                dec!(10),
            )?,
            canary_max_midpoint_move: decimal_env(
                "MARKET_MAKING_CANARY_MAX_MIDPOINT_MOVE",
                dec!(0.01),
            )?,
            news_halt_file: PathBuf::from(
                env::var("MARKET_MAKING_NEWS_HALT_FILE")
                    .unwrap_or_else(|_| "data/market-making/news-halts.txt".to_string()),
            ),
            duration_secs: parse("MARKET_MAKING_SHADOW_DURATION_SECS", "0")?,
        };
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        if !self.clob_url.starts_with("https://")
            || !self.gamma_url.starts_with("https://")
            || !self.data_url.starts_with("https://")
        {
            bail!("maker research endpoints must use https://");
        }
        if self.max_markets == 0 || self.max_markets > 25 {
            bail!("MARKET_MAKING_MAX_MARKETS must be in [1, 25]");
        }
        if self.reward_poll_secs < 300 || self.reward_poll_secs > 86_400 {
            bail!("MARKET_MAKING_REWARD_POLL_SECS must be in [300, 86400]");
        }
        if self.discovery_pages == 0 || self.discovery_pages > 10 {
            bail!("MARKET_MAKING_DISCOVERY_PAGES must be in [1, 10]");
        }
        if self.enrichment_limit < self.max_markets || self.enrichment_limit > 500 {
            bail!("MARKET_MAKING_ENRICHMENT_LIMIT must be >= MARKET_MAKING_MAX_MARKETS and <= 500");
        }
        if self.request_concurrency == 0 || self.request_concurrency > 32 {
            bail!("MARKET_MAKING_REQUEST_CONCURRENCY must be in [1, 32]");
        }
        for (name, value) in [
            (
                "MARKET_MAKING_MIN_DAILY_REWARD_USDC",
                self.min_daily_reward_usdc,
            ),
            (
                "MARKET_MAKING_MAX_REWARDS_MIN_SIZE",
                self.max_rewards_min_size,
            ),
            ("MARKET_MAKING_QUOTE_SIZE_SHARES", self.quote_size_shares),
            ("MARKET_MAKING_QUOTE_HALF_SPREAD", self.quote_half_spread),
            (
                "MARKET_MAKING_MIN_VOLUME_24H_USDC",
                self.min_volume_24h_usdc,
            ),
            (
                "MARKET_MAKING_HIGH_FEE_VOLUME_MULTIPLIER",
                self.high_fee_volume_multiplier,
            ),
            (
                "MARKET_MAKING_MAX_INVENTORY_SHARES",
                self.max_inventory_shares,
            ),
        ] {
            if value <= Decimal::ZERO {
                bail!("{name} must be greater than zero");
            }
        }
        if self.min_liquidity_usdc < Decimal::ZERO {
            bail!("MARKET_MAKING_MIN_LIQUIDITY_USDC cannot be negative");
        }
        if self.max_fee_rate < Decimal::ZERO || self.max_fee_rate > dec!(1) {
            bail!("MARKET_MAKING_MAX_FEE_RATE must be in [0, 1]");
        }
        if self.high_fee_threshold < Decimal::ZERO || self.high_fee_threshold > self.max_fee_rate {
            bail!("MARKET_MAKING_HIGH_FEE_THRESHOLD must be in [0, MARKET_MAKING_MAX_FEE_RATE]");
        }
        if self.inventory_skew_per_share < Decimal::ZERO {
            bail!("MARKET_MAKING_INVENTORY_SKEW_PER_SHARE cannot be negative");
        }
        if self.quote_half_spread_grid.is_empty()
            || self
                .quote_half_spread_grid
                .iter()
                .any(|value| *value <= Decimal::ZERO)
        {
            bail!("MARKET_MAKING_QUOTE_HALF_SPREAD_GRID must contain positive values");
        }
        if self.max_midpoint_move <= Decimal::ZERO || self.max_midpoint_move >= dec!(1) {
            bail!("MARKET_MAKING_MAX_MIDPOINT_MOVE must be in (0, 1)");
        }
        if self.midpoint_move_window_secs == 0 || self.midpoint_move_window_secs > 3_600 {
            bail!("MARKET_MAKING_MIDPOINT_MOVE_WINDOW_SECS must be in [1, 3600]");
        }
        if self.min_trades_1h > 10_000
            || self.book_activity_window_secs == 0
            || self.book_activity_window_secs > 3_600
            || self.min_book_updates > 10_000
        {
            bail!("maker activity thresholds are invalid");
        }
        if self.max_consecutive_negative_markouts == 0
            || self.max_consecutive_negative_markouts > 1_000
        {
            bail!("MARKET_MAKING_MAX_CONSECUTIVE_NEGATIVE_MARKOUTS must be in [1, 1000]");
        }
        if self.max_quotes_per_market_hour == 0 || self.max_quotes_per_market_hour > 3_600 {
            bail!("MARKET_MAKING_MAX_QUOTES_PER_MARKET_HOUR must be in [1, 3600]");
        }
        if self.http_max_retries == 0
            || self.http_max_retries > 10
            || self.retry_base_ms == 0
            || self.retry_base_ms > 60_000
            || self.trade_cache_secs > 300
            || self.websocket_book_max_age_ms <= 0
            || self.websocket_book_max_age_ms > 300_000
        {
            bail!("maker HTTP/cache/WebSocket settings are invalid");
        }
        if self.max_observed_spread <= Decimal::ZERO || self.max_observed_spread >= dec!(1) {
            bail!("MARKET_MAKING_MAX_OBSERVED_SPREAD must be in (0, 1)");
        }
        if self.min_midpoint < Decimal::ZERO
            || self.max_midpoint > dec!(1)
            || self.min_midpoint >= self.max_midpoint
        {
            bail!("MAKER midpoint range is invalid");
        }
        if self.reward_band_fraction <= Decimal::ZERO || self.reward_band_fraction > dec!(1) {
            bail!("MARKET_MAKING_REWARD_BAND_FRACTION must be in (0, 1]");
        }
        if self.book_poll_secs == 0
            || self.book_poll_secs > 60
            || self.quote_interval_secs < self.book_poll_secs
            || self.quote_interval_secs > 3_600
        {
            bail!("maker polling/quote intervals are invalid");
        }
        if self.market_refresh_secs < self.quote_interval_secs
            || self.market_refresh_secs > 86_400
            || self.min_market_residency_secs < self.market_refresh_secs
            || self.min_market_residency_secs > 604_800
            || self.market_cooldown_secs < self.market_refresh_secs
            || self.market_cooldown_secs > 604_800
        {
            bail!("maker market refresh/cooldown intervals are invalid");
        }
        if self.replacement_score_margin < Decimal::ZERO
            || self.replacement_score_margin > Decimal::ONE
        {
            bail!("MARKET_MAKING_REPLACEMENT_SCORE_MARGIN must be in [0, 1]");
        }
        if self.max_consecutive_market_errors == 0 || self.max_consecutive_market_errors > 1_000 {
            bail!("MARKET_MAKING_MAX_CONSECUTIVE_MARKET_ERRORS must be in [1, 1000]");
        }
        if self
            .markout_horizons_secs
            .last()
            .is_some_and(|horizon| *horizon > 3_600)
        {
            bail!("maker markout horizons cannot exceed 3600 seconds");
        }
        if !self.markout_horizons_secs.contains(&300) || !self.markout_horizons_secs.contains(&1800)
        {
            bail!("MARKET_MAKING_MARKOUT_HORIZONS_SECS must include 300 and 1800");
        }
        if self.min_independent_fill_samples == 0 || self.min_independent_fill_samples > 100_000 {
            bail!("MARKET_MAKING_MIN_INDEPENDENT_FILL_SAMPLES must be in [1, 100000]");
        }
        if self.min_reward_capture_ratio < Decimal::ZERO
            || self.target_reward_capture_ratio <= self.min_reward_capture_ratio
            || self.target_reward_capture_ratio > Decimal::ONE
            || self.reward_break_even_margin_ratio < Decimal::ZERO
            || self.reward_break_even_margin_ratio > Decimal::ONE
        {
            bail!("maker reward capture ratios require 0 <= minimum < target <= 1 and a margin in [0, 1]");
        }
        if self.canary_max_markets == 0
            || self.canary_max_markets > 3
            || self.canary_max_quote_notional_usdc <= Decimal::ZERO
            || self.canary_max_quote_notional_usdc > dec!(10)
            || self.canary_max_inventory_usdc <= Decimal::ZERO
            || self.canary_max_inventory_usdc > dec!(20)
            || self.canary_max_midpoint_move <= Decimal::ZERO
            || self.canary_max_midpoint_move > dec!(0.02)
        {
            bail!("maker canary limits exceed the research safety envelope");
        }
        Ok(())
    }
}

fn parse<T>(name: &str, default: &str) -> Result<T>
where
    T: FromStr,
    T::Err: std::error::Error + Send + Sync + 'static,
{
    env::var(name)
        .unwrap_or_else(|_| default.to_string())
        .parse::<T>()
        .with_context(|| format!("{name} has an invalid value"))
}

fn decimal_env(name: &str, default: Decimal) -> Result<Decimal> {
    env::var(name)
        .ok()
        .map(|value| Decimal::from_str(value.trim()))
        .transpose()
        .with_context(|| format!("{name} has an invalid decimal value"))
        .map(|value| value.unwrap_or(default))
}

fn parse_horizons(raw: &str) -> Result<Vec<u64>> {
    let mut values = raw
        .split(',')
        .map(|value| {
            value
                .trim()
                .parse::<u64>()
                .context("MARKET_MAKING_MARKOUT_HORIZONS_SECS contains an invalid value")
        })
        .collect::<Result<Vec<_>>>()?;
    values.retain(|value| *value > 0);
    values.sort_unstable();
    values.dedup();
    if values.is_empty() {
        bail!("MARKET_MAKING_MARKOUT_HORIZONS_SECS cannot be empty");
    }
    Ok(values)
}

fn parse_decimal_list(name: &str, raw: &str) -> Result<Vec<Decimal>> {
    let mut values = raw
        .split(',')
        .map(|value| {
            Decimal::from_str(value.trim())
                .with_context(|| format!("{name} contains an invalid value"))
        })
        .collect::<Result<Vec<_>>>()?;
    values.sort_unstable();
    values.dedup();
    if values.is_empty() {
        bail!("{name} cannot be empty");
    }
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn horizons_are_positive_sorted_and_unique() {
        assert_eq!(parse_horizons("30,1,5,5,0").unwrap(), vec![1, 5, 30]);
        assert!(parse_horizons("0,0").is_err());
        assert!(parse_horizons("1,bad").is_err());
    }
}
