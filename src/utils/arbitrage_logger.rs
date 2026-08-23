use anyhow::Result;
use chrono::Utc;
use rust_decimal_macros::dec;
use serde::Serialize;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

use crate::monitor::arbitrage::ArbitrageShadowCandidate;
use crate::monitor::ArbitrageOpportunity;

#[derive(Serialize)]
struct ArbitrageRecord {
    timestamp: String,
    market_id: String,
    market_name: String,
    yes_token_id: String,
    no_token_id: String,
    yes_ask_price: String,
    no_ask_price: String,
    total_cost: String,
    profit_percentage: String,
    yes_size: String,
    no_size: String,
}

#[derive(Serialize)]
struct ArbitrageShadowRecord {
    action: &'static str,
    timestamp: String,
    observed_at_ms: i64,
    market_id: String,
    market_name: String,
    yes_token_id: String,
    no_token_id: String,
    yes_ask_price: String,
    no_ask_price: String,
    yes_ask_size: String,
    no_ask_size: String,
    yes_depth_usdc: String,
    no_depth_usdc: String,
    common_size: String,
    total_price: String,
    profit_percentage: String,
    yes_book_age_ms: i64,
    no_book_age_ms: i64,
    book_skew_ms: i64,
    first_seen_at_ms: i64,
    observed_duration_ms: i64,
    edge_thresholds_passed_pct: Vec<String>,
    depth_thresholds_passed_usdc: Vec<String>,
    survives_250ms: bool,
}

/// Write arbitrage opportunity to file
pub fn log_arbitrage_opportunity(
    opp: &ArbitrageOpportunity,
    market_name: &str,
    file_path: &str,
) -> Result<()> {
    let record = ArbitrageRecord {
        timestamp: Utc::now().to_rfc3339(),
        market_id: format!("{:?}", opp.market_id),
        market_name: market_name.to_string(),
        yes_token_id: opp.yes_token_id.to_string(),
        no_token_id: opp.no_token_id.to_string(),
        yes_ask_price: opp.yes_ask_price.to_string(),
        no_ask_price: opp.no_ask_price.to_string(),
        total_cost: opp.total_cost.to_string(),
        profit_percentage: opp.profit_percentage.to_string(),
        yes_size: opp.yes_size.to_string(),
        no_size: opp.no_size.to_string(),
    };

    append_record(file_path, &record)
}

/// Persist a depth-rejected candidate without changing production admission.
/// `survives_250ms` means the same market and price pair remained continuously
/// depth-rejected for at least the paper second-leg delay.
pub fn log_arbitrage_shadow_candidate(
    candidate: &ArbitrageShadowCandidate,
    market_name: &str,
    observed_at_ms: i64,
    file_path: &str,
) -> Result<()> {
    let edge_thresholds = [dec!(0.1), dec!(0.5), dec!(1), dec!(2), dec!(3)];
    let depth_thresholds = [dec!(1), dec!(2), dec!(3), dec!(5)];
    let min_depth = candidate.yes_depth_usdc.min(candidate.no_depth_usdc);
    let yes_age = observed_at_ms.saturating_sub(candidate.yes_book_timestamp_ms);
    let no_age = observed_at_ms.saturating_sub(candidate.no_book_timestamp_ms);
    let record = ArbitrageShadowRecord {
        action: "depth_rejected",
        timestamp: Utc::now().to_rfc3339(),
        observed_at_ms,
        market_id: format!("{:?}", candidate.market_id),
        market_name: market_name.to_string(),
        yes_token_id: candidate.yes_token_id.to_string(),
        no_token_id: candidate.no_token_id.to_string(),
        yes_ask_price: candidate.yes_ask_price.to_string(),
        no_ask_price: candidate.no_ask_price.to_string(),
        yes_ask_size: candidate.yes_ask_size.to_string(),
        no_ask_size: candidate.no_ask_size.to_string(),
        yes_depth_usdc: candidate.yes_depth_usdc.to_string(),
        no_depth_usdc: candidate.no_depth_usdc.to_string(),
        common_size: candidate.common_size.to_string(),
        total_price: candidate.total_price.to_string(),
        profit_percentage: candidate.profit_percentage.to_string(),
        yes_book_age_ms: yes_age,
        no_book_age_ms: no_age,
        book_skew_ms: (candidate.yes_book_timestamp_ms - candidate.no_book_timestamp_ms).abs(),
        first_seen_at_ms: candidate.first_seen_at_ms,
        observed_duration_ms: candidate.observed_duration_ms,
        edge_thresholds_passed_pct: edge_thresholds
            .into_iter()
            .filter(|threshold| candidate.profit_percentage >= *threshold)
            .map(|threshold| threshold.to_string())
            .collect(),
        depth_thresholds_passed_usdc: depth_thresholds
            .into_iter()
            .filter(|threshold| min_depth >= *threshold)
            .map(|threshold| threshold.to_string())
            .collect(),
        survives_250ms: candidate.observed_duration_ms >= 250,
    };
    append_record(file_path, &record)
}

fn append_record(file_path: &str, record: &impl Serialize) -> Result<()> {
    // Newline-delimited JSON is append-safe and easy to stream into tooling.
    let json = serde_json::to_string(record)?;

    // Append to file. Strategy result paths commonly include mode-specific
    // directories, so create the parent before the first admitted opportunity.
    if let Some(parent) = Path::new(file_path)
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(file_path)?;

    writeln!(file, "{}", json)?;
    file.flush()?; // Flush to disk

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use polymarket_client_sdk::types::{B256, U256};

    #[test]
    fn shadow_record_contains_grid_and_duration() {
        let path =
            std::env::temp_dir().join(format!("polypulse-shadow-{}.ndjson", uuid::Uuid::new_v4()));
        let candidate = ArbitrageShadowCandidate {
            market_id: B256::ZERO,
            yes_token_id: U256::from(1),
            no_token_id: U256::from(2),
            yes_ask_price: dec!(0.45),
            no_ask_price: dec!(0.50),
            yes_ask_size: dec!(10),
            no_ask_size: dec!(4),
            yes_depth_usdc: dec!(4.5),
            no_depth_usdc: dec!(2),
            common_size: dec!(4),
            total_price: dec!(0.95),
            profit_percentage: dec!(5),
            yes_book_timestamp_ms: 9_900,
            no_book_timestamp_ms: 9_800,
            first_seen_at_ms: 9_500,
            observed_duration_ms: 500,
        };
        log_arbitrage_shadow_candidate(
            &candidate,
            "btc prediction market",
            10_000,
            path.to_str().unwrap(),
        )
        .unwrap();
        let record: serde_json::Value =
            serde_json::from_str(std::fs::read_to_string(&path).unwrap().trim()).unwrap();
        assert_eq!(record["action"], "depth_rejected");
        assert_eq!(record["survives_250ms"], true);
        assert_eq!(record["book_skew_ms"], 100);
        assert_eq!(
            record["depth_thresholds_passed_usdc"],
            serde_json::json!(["1", "2"])
        );
        std::fs::remove_file(path).unwrap();
    }
}
