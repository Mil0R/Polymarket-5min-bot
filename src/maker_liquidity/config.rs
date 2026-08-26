use anyhow::{bail, Context, Result};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::env;
use std::path::PathBuf;
use std::str::FromStr;

#[derive(Debug, Clone)]
pub struct MakerResearchConfig {
    pub clob_url: String,
    pub gamma_url: String,
    pub data_url: String,
    pub results_file: PathBuf,
    pub max_markets: usize,
    pub discovery_pages: usize,
    pub enrichment_limit: usize,
    pub request_concurrency: usize,
    pub min_daily_reward_usdc: Decimal,
    pub max_rewards_min_size: Decimal,
    pub min_liquidity_usdc: Decimal,
    pub max_observed_spread: Decimal,
    pub min_midpoint: Decimal,
    pub max_midpoint: Decimal,
    pub market_filter: Option<String>,
    pub quote_size_shares: Decimal,
    pub quote_half_spread: Decimal,
    pub reward_band_fraction: Decimal,
    pub book_poll_secs: u64,
    pub quote_interval_secs: u64,
    pub markout_horizons_secs: Vec<u64>,
    pub duration_secs: u64,
}

impl MakerResearchConfig {
    pub fn from_env() -> Result<Self> {
        let config = Self {
            clob_url: env::var("CLOB_API_URL")
                .unwrap_or_else(|_| "https://clob.polymarket.com".to_string()),
            gamma_url: env::var("GAMMA_API_URL")
                .unwrap_or_else(|_| "https://gamma-api.polymarket.com".to_string()),
            data_url: env::var("DATA_API_URL")
                .unwrap_or_else(|_| "https://data-api.polymarket.com".to_string()),
            results_file: PathBuf::from(env::var("MAKER_RESULTS_FILE").unwrap_or_else(|_| {
                "results/maker-liquidity/shadow-run-001/events.ndjson".to_string()
            })),
            max_markets: parse("MAKER_MAX_MARKETS", "5")?,
            discovery_pages: parse("MAKER_DISCOVERY_PAGES", "3")?,
            enrichment_limit: parse("MAKER_ENRICHMENT_LIMIT", "100")?,
            request_concurrency: parse("MAKER_REQUEST_CONCURRENCY", "8")?,
            min_daily_reward_usdc: decimal_env("MAKER_MIN_DAILY_REWARD_USDC", dec!(3))?,
            max_rewards_min_size: decimal_env("MAKER_MAX_REWARDS_MIN_SIZE", dec!(100))?,
            min_liquidity_usdc: decimal_env("MAKER_MIN_LIQUIDITY_USDC", dec!(50))?,
            max_observed_spread: decimal_env("MAKER_MAX_OBSERVED_SPREAD", dec!(0.12))?,
            min_midpoint: decimal_env("MAKER_MIN_MIDPOINT", dec!(0.10))?,
            max_midpoint: decimal_env("MAKER_MAX_MIDPOINT", dec!(0.90))?,
            market_filter: env::var("MAKER_MARKET_FILTER")
                .ok()
                .map(|value| value.trim().to_ascii_lowercase())
                .filter(|value| !value.is_empty()),
            quote_size_shares: decimal_env("MAKER_QUOTE_SIZE_SHARES", dec!(20))?,
            quote_half_spread: decimal_env("MAKER_QUOTE_HALF_SPREAD", dec!(0.01))?,
            reward_band_fraction: decimal_env("MAKER_REWARD_BAND_FRACTION", dec!(0.8))?,
            book_poll_secs: parse("MAKER_BOOK_POLL_SECS", "5")?,
            quote_interval_secs: parse("MAKER_QUOTE_INTERVAL_SECS", "30")?,
            markout_horizons_secs: parse_horizons(
                &env::var("MAKER_MARKOUT_HORIZONS_SECS")
                    .unwrap_or_else(|_| "1,5,30,60".to_string()),
            )?,
            duration_secs: parse("MAKER_SHADOW_DURATION_SECS", "0")?,
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
            bail!("MAKER_MAX_MARKETS must be in [1, 25]");
        }
        if self.discovery_pages == 0 || self.discovery_pages > 10 {
            bail!("MAKER_DISCOVERY_PAGES must be in [1, 10]");
        }
        if self.enrichment_limit < self.max_markets || self.enrichment_limit > 500 {
            bail!("MAKER_ENRICHMENT_LIMIT must be >= MAKER_MAX_MARKETS and <= 500");
        }
        if self.request_concurrency == 0 || self.request_concurrency > 32 {
            bail!("MAKER_REQUEST_CONCURRENCY must be in [1, 32]");
        }
        for (name, value) in [
            ("MAKER_MIN_DAILY_REWARD_USDC", self.min_daily_reward_usdc),
            ("MAKER_MAX_REWARDS_MIN_SIZE", self.max_rewards_min_size),
            ("MAKER_QUOTE_SIZE_SHARES", self.quote_size_shares),
            ("MAKER_QUOTE_HALF_SPREAD", self.quote_half_spread),
        ] {
            if value <= Decimal::ZERO {
                bail!("{name} must be greater than zero");
            }
        }
        if self.min_liquidity_usdc < Decimal::ZERO {
            bail!("MAKER_MIN_LIQUIDITY_USDC cannot be negative");
        }
        if self.max_observed_spread <= Decimal::ZERO || self.max_observed_spread >= dec!(1) {
            bail!("MAKER_MAX_OBSERVED_SPREAD must be in (0, 1)");
        }
        if self.min_midpoint < Decimal::ZERO
            || self.max_midpoint > dec!(1)
            || self.min_midpoint >= self.max_midpoint
        {
            bail!("MAKER midpoint range is invalid");
        }
        if self.reward_band_fraction <= Decimal::ZERO || self.reward_band_fraction > dec!(1) {
            bail!("MAKER_REWARD_BAND_FRACTION must be in (0, 1]");
        }
        if self.book_poll_secs == 0
            || self.book_poll_secs > 60
            || self.quote_interval_secs < self.book_poll_secs
            || self.quote_interval_secs > 3_600
        {
            bail!("maker polling/quote intervals are invalid");
        }
        if self
            .markout_horizons_secs
            .last()
            .is_some_and(|horizon| *horizon > 3_600)
        {
            bail!("maker markout horizons cannot exceed 3600 seconds");
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
                .context("MAKER_MARKOUT_HORIZONS_SECS contains an invalid value")
        })
        .collect::<Result<Vec<_>>>()?;
    values.retain(|value| *value > 0);
    values.sort_unstable();
    values.dedup();
    if values.is_empty() {
        bail!("MAKER_MARKOUT_HORIZONS_SECS cannot be empty");
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
