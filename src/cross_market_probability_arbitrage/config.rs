use anyhow::{bail, Context, Result};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::env;
use std::path::PathBuf;
use std::str::FromStr;

#[derive(Debug, Clone)]
pub struct CrossMarketProbabilityArbitrageConfig {
    pub gamma_url: String,
    pub clob_url: String,
    pub results_file: PathBuf,
    pub summary_file: PathBuf,
    pub semantic_review_file: PathBuf,
    pub event_pages: usize,
    pub events_per_page: usize,
    pub max_relationships: usize,
    pub max_partition_legs: usize,
    pub request_concurrency: usize,
    pub poll_seconds: u64,
    pub full_scan_seconds: u64,
    pub unchanged_log_seconds: u64,
    pub relationship_refresh_seconds: u64,
    pub summary_seconds: u64,
    pub duration_seconds: u64,
    pub results_max_bytes: u64,
    pub results_retention_files: usize,
    pub size_ladder_shares: Vec<Decimal>,
    pub min_bundle_notional_usdc: Decimal,
    pub min_net_edge: Decimal,
    pub near_miss_edge: Decimal,
    pub execution_buffer_pct: Decimal,
    pub execution_buffer_curve_pct: Vec<Decimal>,
    pub near_miss_log_seconds: u64,
    pub ordinary_log_seconds: u64,
    pub websocket_books: bool,
    pub websocket_book_max_age_ms: i64,
    pub semantic_allowlist: Vec<String>,
}

impl CrossMarketProbabilityArbitrageConfig {
    pub fn from_env() -> Result<Self> {
        let config = Self {
            gamma_url: env::var("GAMMA_API_URL")
                .unwrap_or_else(|_| "https://gamma-api.polymarket.com".into()),
            clob_url: env::var("CLOB_API_URL")
                .unwrap_or_else(|_| "https://clob.polymarket.com".into()),
            results_file: PathBuf::from(
                env::var("CROSS_MARKET_PROBABILITY_ARBITRAGE_RESULTS_FILE").unwrap_or_else(|_| {
                    "results/cross-market-probability-arbitrage/shadow-run-003/events.ndjson".into()
                }),
            ),
            summary_file: PathBuf::from(
                env::var("CROSS_MARKET_PROBABILITY_ARBITRAGE_SUMMARY_FILE").unwrap_or_else(|_| {
                    "results/cross-market-probability-arbitrage/shadow-run-003/summary.ndjson"
                        .into()
                }),
            ),
            semantic_review_file: PathBuf::from(
                env::var("CROSS_MARKET_PROBABILITY_ARBITRAGE_SEMANTIC_REVIEW_FILE").unwrap_or_else(
                    |_| "data/cross-market-probability-semantic-reviews.json".into(),
                ),
            ),
            event_pages: parse("CROSS_MARKET_PROBABILITY_ARBITRAGE_EVENT_PAGES", "10")?,
            events_per_page: parse("CROSS_MARKET_PROBABILITY_ARBITRAGE_EVENTS_PER_PAGE", "100")?,
            max_relationships: parse(
                "CROSS_MARKET_PROBABILITY_ARBITRAGE_MAX_RELATIONSHIPS",
                "500",
            )?,
            max_partition_legs: parse(
                "CROSS_MARKET_PROBABILITY_ARBITRAGE_MAX_PARTITION_LEGS",
                "12",
            )?,
            request_concurrency: parse(
                "CROSS_MARKET_PROBABILITY_ARBITRAGE_REQUEST_CONCURRENCY",
                "12",
            )?,
            poll_seconds: parse("CROSS_MARKET_PROBABILITY_ARBITRAGE_POLL_SECONDS", "10")?,
            full_scan_seconds: parse("CROSS_MARKET_PROBABILITY_ARBITRAGE_FULL_SCAN_SECONDS", "60")?,
            unchanged_log_seconds: parse(
                "CROSS_MARKET_PROBABILITY_ARBITRAGE_UNCHANGED_LOG_SECONDS",
                "600",
            )?,
            relationship_refresh_seconds: parse(
                "CROSS_MARKET_PROBABILITY_ARBITRAGE_RELATIONSHIP_REFRESH_SECONDS",
                "300",
            )?,
            summary_seconds: parse("CROSS_MARKET_PROBABILITY_ARBITRAGE_SUMMARY_SECONDS", "600")?,
            duration_seconds: parse(
                "CROSS_MARKET_PROBABILITY_ARBITRAGE_SHADOW_DURATION_SECONDS",
                "0",
            )?,
            results_max_bytes: parse(
                "CROSS_MARKET_PROBABILITY_ARBITRAGE_RESULTS_MAX_BYTES",
                "52428800",
            )?,
            results_retention_files: parse(
                "CROSS_MARKET_PROBABILITY_ARBITRAGE_RESULTS_RETENTION_FILES",
                "5",
            )?,
            size_ladder_shares: parse_decimal_list(
                "CROSS_MARKET_PROBABILITY_ARBITRAGE_SIZE_LADDER_SHARES",
                "1,2,5,10,20,50,100",
            )?,
            min_bundle_notional_usdc: decimal_env(
                "CROSS_MARKET_PROBABILITY_ARBITRAGE_MIN_BUNDLE_NOTIONAL_USDC",
                dec!(1),
            )?,
            min_net_edge: decimal_env(
                "CROSS_MARKET_PROBABILITY_ARBITRAGE_MIN_NET_EDGE",
                dec!(0.01),
            )?,
            near_miss_edge: decimal_env(
                "CROSS_MARKET_PROBABILITY_ARBITRAGE_NEAR_MISS_EDGE",
                dec!(-0.03),
            )?,
            execution_buffer_pct: decimal_env(
                "CROSS_MARKET_PROBABILITY_ARBITRAGE_EXECUTION_BUFFER_PCT",
                dec!(0.003),
            )?,
            execution_buffer_curve_pct: parse_decimal_list_allow_zero(
                "CROSS_MARKET_PROBABILITY_ARBITRAGE_EXECUTION_BUFFER_CURVE_PCT",
                "0,0.003,0.005,0.01",
            )?,
            near_miss_log_seconds: parse(
                "CROSS_MARKET_PROBABILITY_ARBITRAGE_NEAR_MISS_LOG_SECONDS",
                "60",
            )?,
            ordinary_log_seconds: parse(
                "CROSS_MARKET_PROBABILITY_ARBITRAGE_ORDINARY_LOG_SECONDS",
                "3600",
            )?,
            websocket_books: parse("CROSS_MARKET_PROBABILITY_ARBITRAGE_WEBSOCKET_BOOKS", "true")?,
            websocket_book_max_age_ms: parse(
                "CROSS_MARKET_PROBABILITY_ARBITRAGE_WEBSOCKET_BOOK_MAX_AGE_MS",
                "15000",
            )?,
            semantic_allowlist: parse_string_list(
                "CROSS_MARKET_PROBABILITY_ARBITRAGE_SEMANTIC_ALLOWLIST",
            ),
        };
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        if !self.gamma_url.starts_with("https://") || !self.clob_url.starts_with("https://") {
            bail!("cross-market probability research endpoints must use https://");
        }
        if self.event_pages == 0 || self.event_pages > 20 {
            bail!("CROSS_MARKET_PROBABILITY_ARBITRAGE_EVENT_PAGES must be in [1, 20]");
        }
        if self.events_per_page == 0 || self.events_per_page > 500 {
            bail!("CROSS_MARKET_PROBABILITY_ARBITRAGE_EVENTS_PER_PAGE must be in [1, 500]");
        }
        if self.max_relationships == 0 || self.max_relationships > 2_000 {
            bail!("CROSS_MARKET_PROBABILITY_ARBITRAGE_MAX_RELATIONSHIPS must be in [1, 2000]");
        }
        if self.max_partition_legs < 2 || self.max_partition_legs > 50 {
            bail!("CROSS_MARKET_PROBABILITY_ARBITRAGE_MAX_PARTITION_LEGS must be in [2, 50]");
        }
        if self.request_concurrency == 0 || self.request_concurrency > 32 {
            bail!("CROSS_MARKET_PROBABILITY_ARBITRAGE_REQUEST_CONCURRENCY must be in [1, 32]");
        }
        if self.poll_seconds == 0 || self.poll_seconds > 3_600 {
            bail!("CROSS_MARKET_PROBABILITY_ARBITRAGE_POLL_SECONDS must be in [1, 3600]");
        }
        if self.full_scan_seconds < self.poll_seconds || self.full_scan_seconds > 86_400 {
            bail!("CROSS_MARKET_PROBABILITY_ARBITRAGE_FULL_SCAN_SECONDS must be >= poll interval and <= 86400");
        }
        if self.unchanged_log_seconds < self.poll_seconds {
            bail!("CROSS_MARKET_PROBABILITY_ARBITRAGE_UNCHANGED_LOG_SECONDS must be >= CROSS_MARKET_PROBABILITY_ARBITRAGE_POLL_SECONDS");
        }
        if self.relationship_refresh_seconds < self.poll_seconds
            || self.relationship_refresh_seconds > 86_400
            || self.summary_seconds < self.poll_seconds
            || self.summary_seconds > 86_400
        {
            bail!("Bayesian refresh/summary intervals are invalid");
        }
        if self.results_max_bytes < 1_048_576 || self.results_max_bytes > 10_737_418_240 {
            bail!("CROSS_MARKET_PROBABILITY_ARBITRAGE_RESULTS_MAX_BYTES must be between 1 MiB and 10 GiB");
        }
        if self.results_retention_files == 0 || self.results_retention_files > 100 {
            bail!("CROSS_MARKET_PROBABILITY_ARBITRAGE_RESULTS_RETENTION_FILES must be in [1, 100]");
        }
        if self.size_ladder_shares.is_empty()
            || self
                .size_ladder_shares
                .iter()
                .any(|value| *value <= Decimal::ZERO)
        {
            bail!(
                "CROSS_MARKET_PROBABILITY_ARBITRAGE_SIZE_LADDER_SHARES must contain positive sizes"
            );
        }
        if self.min_bundle_notional_usdc < Decimal::ZERO {
            bail!("CROSS_MARKET_PROBABILITY_ARBITRAGE_MIN_BUNDLE_NOTIONAL_USDC cannot be negative");
        }
        if self.min_net_edge < Decimal::ZERO || self.min_net_edge >= dec!(1) {
            bail!("CROSS_MARKET_PROBABILITY_ARBITRAGE_MIN_NET_EDGE must be in [0, 1)");
        }
        if self.near_miss_edge < dec!(-1) || self.near_miss_edge >= self.min_net_edge {
            bail!("CROSS_MARKET_PROBABILITY_ARBITRAGE_NEAR_MISS_EDGE must be in [-1, CROSS_MARKET_PROBABILITY_ARBITRAGE_MIN_NET_EDGE)");
        }
        if self.execution_buffer_pct < Decimal::ZERO || self.execution_buffer_pct >= dec!(0.25) {
            bail!("CROSS_MARKET_PROBABILITY_ARBITRAGE_EXECUTION_BUFFER_PCT must be in [0, 0.25)");
        }
        if self.execution_buffer_curve_pct.is_empty()
            || self
                .execution_buffer_curve_pct
                .iter()
                .any(|value| *value < Decimal::ZERO || *value >= dec!(0.25))
        {
            bail!("CROSS_MARKET_PROBABILITY_ARBITRAGE_EXECUTION_BUFFER_CURVE_PCT values must be in [0, 0.25)");
        }
        if !self
            .execution_buffer_curve_pct
            .contains(&self.execution_buffer_pct)
        {
            bail!("CROSS_MARKET_PROBABILITY_ARBITRAGE_EXECUTION_BUFFER_CURVE_PCT must include CROSS_MARKET_PROBABILITY_ARBITRAGE_EXECUTION_BUFFER_PCT");
        }
        if self.near_miss_log_seconds < self.poll_seconds
            || self.ordinary_log_seconds < self.near_miss_log_seconds
        {
            bail!("Bayesian detail log intervals are invalid");
        }
        if self.websocket_book_max_age_ms <= 0 || self.websocket_book_max_age_ms > 300_000 {
            bail!("CROSS_MARKET_PROBABILITY_ARBITRAGE_WEBSOCKET_BOOK_MAX_AGE_MS must be in [1, 300000]");
        }
        Ok(())
    }
}

fn parse_string_list(name: &str) -> Vec<String> {
    let mut values = env::var(name)
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    values.sort();
    values.dedup();
    values
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

fn parse_decimal_list(name: &str, default: &str) -> Result<Vec<Decimal>> {
    let mut values = env::var(name)
        .unwrap_or_else(|_| default.to_string())
        .split(',')
        .map(|value| Decimal::from_str(value.trim()).with_context(|| format!("{name} is invalid")))
        .collect::<Result<Vec<_>>>()?;
    values.retain(|value| *value > Decimal::ZERO);
    values.sort();
    values.dedup();
    Ok(values)
}

fn parse_decimal_list_allow_zero(name: &str, default: &str) -> Result<Vec<Decimal>> {
    let mut values = env::var(name)
        .unwrap_or_else(|_| default.to_string())
        .split(',')
        .map(|value| Decimal::from_str(value.trim()).with_context(|| format!("{name} is invalid")))
        .collect::<Result<Vec<_>>>()?;
    values.sort();
    values.dedup();
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_ladder_is_sorted_and_deduplicated() {
        assert_eq!(
            parse_decimal_list("TEST", "20,5,10,5,0").unwrap(),
            vec![dec!(5), dec!(10), dec!(20)]
        );
    }
}
