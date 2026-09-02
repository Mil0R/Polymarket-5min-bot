use anyhow::{bail, Context, Result};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::env;
use std::path::PathBuf;
use std::str::FromStr;

#[derive(Debug, Clone)]
pub struct BayesianConsistencyConfig {
    pub gamma_url: String,
    pub clob_url: String,
    pub registry_file: PathBuf,
    pub discovery_file: PathBuf,
    pub historical_discovery_file: PathBuf,
    pub boundary_report_file: PathBuf,
    pub seed_file: PathBuf,
    pub review_file: PathBuf,
    pub results_file: PathBuf,
    pub event_pages: usize,
    pub events_per_page: usize,
    pub max_discovery_candidates: usize,
    pub max_children: usize,
    pub request_concurrency: usize,
    pub poll_seconds: u64,
    pub duration_seconds: u64,
    pub size_ladder_shares: Vec<Decimal>,
    pub min_bundle_notional_usdc: Decimal,
    pub min_net_edge: Decimal,
    pub execution_buffer_pct: Decimal,
    pub max_book_age_ms: i64,
    pub max_leg_snapshot_skew_ms: i64,
    pub shadow_delay_ms: Vec<u64>,
    pub shadow_liquidity_haircuts: Vec<Decimal>,
    pub monitor_interval_seconds: u64,
    pub monitor_duration_seconds: u64,
    pub monitor_results_file: PathBuf,
    pub monitor_state_file: PathBuf,
    pub monitor_alert_on_new_candidate: bool,
    pub monitor_alert_on_rule_drift: bool,
}

impl BayesianConsistencyConfig {
    pub fn from_env() -> Result<Self> {
        let config = Self {
            gamma_url: env::var("GAMMA_API_URL")
                .unwrap_or_else(|_| "https://gamma-api.polymarket.com".into()),
            clob_url: env::var("CLOB_API_URL")
                .unwrap_or_else(|_| "https://clob.polymarket.com".into()),
            registry_file: PathBuf::from(
                env::var("BAYESIAN_CONSISTENCY_ARBITRAGE_REGISTRY_FILE")
                    .unwrap_or_else(|_| "config/bayesian-consistency-relationships.json".into()),
            ),
            discovery_file: PathBuf::from(
                env::var("BAYESIAN_CONSISTENCY_ARBITRAGE_DISCOVERY_FILE").unwrap_or_else(|_| {
                    "results/bayesian-consistency-arbitrage/discovery/candidates.json".into()
                }),
            ),
            historical_discovery_file: PathBuf::from(
                env::var("BAYESIAN_CONSISTENCY_ARBITRAGE_HISTORICAL_DISCOVERY_FILE")
                    .unwrap_or_else(|_| {
                        "results/bayesian-consistency-arbitrage/discovery/historical-candidates.json"
                            .into()
                    }),
            ),
            boundary_report_file: PathBuf::from(
                env::var("BAYESIAN_CONSISTENCY_ARBITRAGE_BOUNDARY_REPORT_FILE")
                    .unwrap_or_else(|_| {
                        "results/bayesian-consistency-arbitrage/discovery/boundary-diagnostics.json"
                            .into()
                    }),
            ),
            seed_file: PathBuf::from(
                env::var("BAYESIAN_CONSISTENCY_ARBITRAGE_SEED_FILE")
                    .unwrap_or_else(|_| "config/bayesian-consistency-candidate-seeds.json".into()),
            ),
            review_file: PathBuf::from(
                env::var("BAYESIAN_CONSISTENCY_ARBITRAGE_REVIEW_FILE").unwrap_or_else(|_| {
                    "config/bayesian-consistency-candidate-reviews.json".into()
                }),
            ),
            results_file: PathBuf::from(
                env::var("BAYESIAN_CONSISTENCY_ARBITRAGE_RESULTS_FILE").unwrap_or_else(|_| {
                    "results/bayesian-consistency-arbitrage/shadow-run-001/events.ndjson".into()
                }),
            ),
            event_pages: parse("BAYESIAN_CONSISTENCY_ARBITRAGE_EVENT_PAGES", "10")?,
            events_per_page: parse("BAYESIAN_CONSISTENCY_ARBITRAGE_EVENTS_PER_PAGE", "100")?,
            max_discovery_candidates: parse(
                "BAYESIAN_CONSISTENCY_ARBITRAGE_MAX_DISCOVERY_CANDIDATES",
                "200",
            )?,
            max_children: parse("BAYESIAN_CONSISTENCY_ARBITRAGE_MAX_CHILDREN", "12")?,
            request_concurrency: parse("BAYESIAN_CONSISTENCY_ARBITRAGE_REQUEST_CONCURRENCY", "12")?,
            poll_seconds: parse("BAYESIAN_CONSISTENCY_ARBITRAGE_POLL_SECONDS", "10")?,
            duration_seconds: parse(
                "BAYESIAN_CONSISTENCY_ARBITRAGE_SHADOW_DURATION_SECONDS",
                "0",
            )?,
            size_ladder_shares: decimal_list(
                "BAYESIAN_CONSISTENCY_ARBITRAGE_SIZE_LADDER_SHARES",
                "1,2,5,10,20",
            )?,
            min_bundle_notional_usdc: decimal(
                "BAYESIAN_CONSISTENCY_ARBITRAGE_MIN_BUNDLE_NOTIONAL_USDC",
                dec!(5),
            )?,
            min_net_edge: decimal("BAYESIAN_CONSISTENCY_ARBITRAGE_MIN_NET_EDGE", dec!(0.02))?,
            execution_buffer_pct: decimal(
                "BAYESIAN_CONSISTENCY_ARBITRAGE_EXECUTION_BUFFER_PCT",
                dec!(0.005),
            )?,
            max_book_age_ms: parse("BAYESIAN_CONSISTENCY_ARBITRAGE_MAX_BOOK_AGE_MS", "1000")?,
            max_leg_snapshot_skew_ms: parse(
                "BAYESIAN_CONSISTENCY_ARBITRAGE_MAX_LEG_SNAPSHOT_SKEW_MS",
                "500",
            )?,
            shadow_delay_ms: integer_list(
                "BAYESIAN_CONSISTENCY_ARBITRAGE_SHADOW_DELAYS_MS",
                "100,500,1000",
            )?,
            shadow_liquidity_haircuts: decimal_list(
                "BAYESIAN_CONSISTENCY_ARBITRAGE_SHADOW_LIQUIDITY_HAIRCUTS",
                "1,0.75,0.5",
            )?,
            monitor_interval_seconds: parse(
                "BAYESIAN_CONSISTENCY_ARBITRAGE_MONITOR_INTERVAL_SECONDS",
                "3600",
            )?,
            monitor_duration_seconds: parse(
                "BAYESIAN_CONSISTENCY_ARBITRAGE_MONITOR_DURATION_SECONDS",
                "0",
            )?,
            monitor_results_file: PathBuf::from(
                env::var("BAYESIAN_CONSISTENCY_ARBITRAGE_MONITOR_RESULTS_FILE")
                    .unwrap_or_else(|_| "results/bayesian-consistency-arbitrage/monitor/events.ndjson".into()),
            ),
            monitor_state_file: PathBuf::from(
                env::var("BAYESIAN_CONSISTENCY_ARBITRAGE_MONITOR_STATE_FILE")
                    .unwrap_or_else(|_| "results/bayesian-consistency-arbitrage/monitor/state.json".into()),
            ),
            monitor_alert_on_new_candidate: bool_env(
                "BAYESIAN_CONSISTENCY_ARBITRAGE_MONITOR_ALERT_ON_NEW_CANDIDATE", true,
            )?,
            monitor_alert_on_rule_drift: bool_env(
                "BAYESIAN_CONSISTENCY_ARBITRAGE_MONITOR_ALERT_ON_RULE_DRIFT", true,
            )?,
        };
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        if !self.gamma_url.starts_with("https://") || !self.clob_url.starts_with("https://") {
            bail!("BAYESIAN consistency Gamma and CLOB endpoints must use https://");
        }
        if !(1..=20).contains(&self.event_pages)
            || !(1..=500).contains(&self.events_per_page)
            || !(1..=2_000).contains(&self.max_discovery_candidates)
        {
            bail!("BAYESIAN discovery paging/candidate configuration is invalid");
        }
        if !(2..=50).contains(&self.max_children) {
            bail!("BAYESIAN_CONSISTENCY_ARBITRAGE_MAX_CHILDREN must be in [2, 50]");
        }
        if !(1..=32).contains(&self.request_concurrency) {
            bail!("BAYESIAN_CONSISTENCY_ARBITRAGE_REQUEST_CONCURRENCY must be in [1, 32]");
        }
        if self.poll_seconds == 0 || self.poll_seconds > 3600 {
            bail!("BAYESIAN_CONSISTENCY_ARBITRAGE_POLL_SECONDS must be in [1, 3600]");
        }
        if self.size_ladder_shares.is_empty()
            || self
                .size_ladder_shares
                .iter()
                .any(|value| *value <= Decimal::ZERO)
        {
            bail!("BAYESIAN size ladder must contain positive values");
        }
        if self.min_bundle_notional_usdc < Decimal::ZERO
            || self.min_net_edge < Decimal::ZERO
            || self.min_net_edge >= dec!(0.25)
            || self.execution_buffer_pct < Decimal::ZERO
            || self.execution_buffer_pct >= dec!(0.25)
        {
            bail!("BAYESIAN notional/edge/buffer configuration is invalid");
        }
        if self.max_book_age_ms <= 0 || self.max_leg_snapshot_skew_ms < 0 {
            bail!("BAYESIAN book age and leg skew must be non-negative with positive age");
        }
        if self.shadow_delay_ms.is_empty()
            || self.shadow_delay_ms.iter().any(|delay| *delay > 30_000)
        {
            bail!("BAYESIAN shadow delays must be non-empty and <= 30000ms");
        }
        if self.shadow_liquidity_haircuts.is_empty()
            || self
                .shadow_liquidity_haircuts
                .iter()
                .any(|haircut| *haircut <= Decimal::ZERO || *haircut > Decimal::ONE)
        {
            bail!("BAYESIAN liquidity haircuts must be in (0, 1]");
        }
        if !(60..=86_400).contains(&self.monitor_interval_seconds) {
            bail!("BAYESIAN monitor interval must be in [60, 86400] seconds");
        }
        Ok(())
    }
}

fn parse<T>(name: &str, fallback: &str) -> Result<T>
where
    T: FromStr,
    T::Err: std::fmt::Display,
{
    env::var(name)
        .unwrap_or_else(|_| fallback.to_string())
        .parse::<T>()
        .map_err(|error| anyhow::anyhow!("invalid {name}: {error}"))
}

fn decimal(name: &str, fallback: Decimal) -> Result<Decimal> {
    Decimal::from_str(&env::var(name).unwrap_or_else(|_| fallback.to_string()))
        .with_context(|| format!("invalid {name}"))
}

fn decimal_list(name: &str, fallback: &str) -> Result<Vec<Decimal>> {
    env::var(name)
        .unwrap_or_else(|_| fallback.to_string())
        .split(',')
        .map(|value| Decimal::from_str(value.trim()).with_context(|| format!("invalid {name}")))
        .collect()
}

fn integer_list(name: &str, fallback: &str) -> Result<Vec<u64>> {
    env::var(name)
        .unwrap_or_else(|_| fallback.to_string())
        .split(',')
        .map(|value| {
            value
                .trim()
                .parse()
                .with_context(|| format!("invalid {name}"))
        })
        .collect()
}

fn bool_env(name: &str, fallback: bool) -> Result<bool> {
    env::var(name)
        .map(|value| match value.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Ok(true),
            "0" | "false" | "no" | "off" => Ok(false),
            _ => Err(anyhow::anyhow!("invalid {name}: expected boolean")),
        })
        .unwrap_or(Ok(fallback))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_conservative() {
        let config = BayesianConsistencyConfig {
            gamma_url: "https://gamma-api.polymarket.com".into(),
            clob_url: "https://clob.polymarket.com".into(),
            registry_file: "registry.json".into(),
            discovery_file: "candidates.json".into(),
            historical_discovery_file: "historical-candidates.json".into(),
            boundary_report_file: "boundary-diagnostics.json".into(),
            seed_file: "seeds.json".into(),
            review_file: "reviews.json".into(),
            results_file: "events.ndjson".into(),
            event_pages: 10,
            events_per_page: 100,
            max_discovery_candidates: 200,
            max_children: 12,
            request_concurrency: 12,
            poll_seconds: 10,
            duration_seconds: 0,
            size_ladder_shares: vec![dec!(1), dec!(5)],
            min_bundle_notional_usdc: dec!(5),
            min_net_edge: dec!(0.02),
            execution_buffer_pct: dec!(0.005),
            max_book_age_ms: 1000,
            max_leg_snapshot_skew_ms: 500,
            shadow_delay_ms: vec![100, 500, 1000],
            shadow_liquidity_haircuts: vec![dec!(1), dec!(0.75), dec!(0.5)],
            monitor_interval_seconds: 3600,
            monitor_duration_seconds: 0,
            monitor_results_file: "monitor.ndjson".into(),
            monitor_state_file: "state.json".into(),
            monitor_alert_on_new_candidate: true,
            monitor_alert_on_rule_drift: true,
        };
        assert!(config.validate().is_ok());
    }
}
