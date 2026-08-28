use anyhow::Result;
use chrono::Utc;
use rust_decimal_macros::dec;
use serde::Serialize;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

use crate::monitor::arbitrage::ArbitrageShadowCandidate;
use crate::monitor::ArbitrageOpportunity;
use crate::opportunity_admission::AdmissionRejection;
use crate::trading::executor::{ArbitrageRestSnapshot, ExecutionQuote, ShadowExecutionAssessment};

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
    yes_limit_price: String,
    no_limit_price: String,
    execution_size: String,
    execution_cost: String,
    execution_gross_edge_per_share: String,
    yes_fee_per_share: String,
    no_fee_per_share: String,
    execution_edge_per_share: String,
    fee_rate: String,
    fee_exponent: u32,
    minimum_order_size: String,
    yes_book_age_ms: i64,
    no_book_age_ms: i64,
    book_skew_ms: i64,
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

#[derive(Serialize)]
struct ArbitrageRestShadowRecord {
    action: &'static str,
    source: &'static str,
    admission_rejection: Option<&'static str>,
    timestamp: String,
    signal_observed_at_ms: i64,
    scheduled_delay_ms: u64,
    actual_signal_age_ms: i64,
    schedule_lag_ms: i64,
    request_latency_ms: u64,
    market_id: String,
    market_name: String,
    yes_token_id: String,
    no_token_id: String,
    raw_yes_ask_price: String,
    raw_no_ask_price: String,
    yes_limit_price: String,
    no_limit_price: String,
    signal_gross_edge_per_share: String,
    signal_yes_fee_per_share: String,
    signal_no_fee_per_share: String,
    signal_net_edge_per_share: String,
    required_gross_edge_per_share: String,
    required_net_edge_per_share: String,
    required_dynamic_gross_edge_per_share: String,
    passes_gross_edge: bool,
    passes_net_edge: bool,
    dynamic_fee_gate_active: bool,
    passes_active_gate: bool,
    max_leg_notional_usdc: String,
    requested_size: String,
    yes_filled: String,
    no_filled: String,
    yes_average_price: String,
    no_average_price: String,
    yes_cost: String,
    no_cost: String,
    yes_fee: String,
    no_fee: String,
    paired_size: String,
    gross_edge_per_share: Option<String>,
    executable_edge_per_share: Option<String>,
    both_fillable: bool,
    full_size_fillable: bool,
    meets_minimum_order_size: bool,
    minimum_order_size: String,
    signal_yes_book_age_ms: i64,
    signal_no_book_age_ms: i64,
    signal_book_skew_ms: i64,
    rest_yes_book_updated_at_ms: Option<i64>,
    rest_no_book_updated_at_ms: Option<i64>,
    error: Option<String>,
}

#[derive(Serialize)]
struct ArbitrageAdmissionShadowRecord {
    action: &'static str,
    timestamp: String,
    signal_observed_at_ms: i64,
    admission_rejection: &'static str,
    market_id: String,
    market_name: String,
    yes_token_id: String,
    no_token_id: String,
    yes_direction: String,
    no_direction: String,
    raw_yes_ask_price: String,
    raw_no_ask_price: String,
    raw_total_price: String,
    raw_profit_percentage: String,
    yes_ask_size: String,
    no_ask_size: String,
    yes_depth_usdc: String,
    no_depth_usdc: String,
    yes_limit_price: String,
    no_limit_price: String,
    quote_size: String,
    quote_total_cost: String,
    signal_gross_edge_per_share: String,
    signal_yes_fee_per_share: String,
    signal_no_fee_per_share: String,
    signal_net_edge_per_share: String,
    required_gross_edge_per_share: String,
    required_net_edge_per_share: String,
    required_dynamic_gross_edge_per_share: String,
    passes_gross_edge: bool,
    passes_net_edge: bool,
    dynamic_fee_gate_active: bool,
    passes_active_gate: bool,
    fee_rate: String,
    fee_exponent: u32,
    minimum_order_size: String,
    signal_yes_book_age_ms: i64,
    signal_no_book_age_ms: i64,
    signal_book_skew_ms: i64,
}

/// Write only an opportunity that survived the exact slippage/tick/fee quote.
pub fn log_arbitrage_opportunity(
    opp: &ArbitrageOpportunity,
    quote: &ExecutionQuote,
    market_name: &str,
    observed_at_ms: i64,
    file_path: &str,
) -> Result<()> {
    let yes_age = observed_at_ms.saturating_sub(opp.yes_book_timestamp_ms);
    let no_age = observed_at_ms.saturating_sub(opp.no_book_timestamp_ms);
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
        yes_limit_price: quote.yes_price.to_string(),
        no_limit_price: quote.no_price.to_string(),
        execution_size: quote.size.to_string(),
        execution_cost: quote.total_cost.to_string(),
        execution_gross_edge_per_share: quote.gross_edge_per_share.to_string(),
        yes_fee_per_share: quote.yes_fee_per_share.to_string(),
        no_fee_per_share: quote.no_fee_per_share.to_string(),
        execution_edge_per_share: quote.edge_per_share.to_string(),
        fee_rate: quote.fee_rate.to_string(),
        fee_exponent: quote.fee_exponent,
        minimum_order_size: quote.minimum_order_size.to_string(),
        yes_book_age_ms: yes_age,
        no_book_age_ms: no_age,
        book_skew_ms: (opp.yes_book_timestamp_ms - opp.no_book_timestamp_ms).abs(),
    };

    append_record(file_path, &record)
}

#[allow(clippy::too_many_arguments)]
pub fn log_arbitrage_rest_shadow(
    source: &'static str,
    admission_rejection: Option<AdmissionRejection>,
    opp: &ArbitrageOpportunity,
    assessment: &ShadowExecutionAssessment,
    market_name: &str,
    signal_observed_at_ms: i64,
    scheduled_delay_ms: u64,
    max_leg_notional_usdc: rust_decimal::Decimal,
    snapshot: &ArbitrageRestSnapshot,
    file_path: &str,
) -> Result<()> {
    let quote = &assessment.quote;
    let paired_size = snapshot.yes_fill.filled.min(snapshot.no_fill.filled);
    let gross_edge_per_share = (paired_size > dec!(0)).then(|| {
        (dec!(1) - snapshot.yes_fill.average_price - snapshot.no_fill.average_price).to_string()
    });
    let executable_edge_per_share = (paired_size > dec!(0)).then(|| {
        quote
            .net_edge_at(
                snapshot.yes_fill.average_price,
                snapshot.no_fill.average_price,
            )
            .to_string()
    });
    let actual_signal_age_ms = snapshot
        .observed_at_ms
        .saturating_sub(signal_observed_at_ms);
    let record = ArbitrageRestShadowRecord {
        action: "rest_shadow",
        source,
        admission_rejection: admission_rejection.map(AdmissionRejection::as_str),
        timestamp: Utc::now().to_rfc3339(),
        signal_observed_at_ms,
        scheduled_delay_ms,
        actual_signal_age_ms,
        schedule_lag_ms: actual_signal_age_ms
            .saturating_sub(i64::try_from(scheduled_delay_ms).unwrap_or(i64::MAX)),
        request_latency_ms: snapshot.request_latency_ms,
        market_id: format!("{:?}", opp.market_id),
        market_name: market_name.to_string(),
        yes_token_id: opp.yes_token_id.to_string(),
        no_token_id: opp.no_token_id.to_string(),
        raw_yes_ask_price: opp.yes_ask_price.to_string(),
        raw_no_ask_price: opp.no_ask_price.to_string(),
        yes_limit_price: quote.yes_price.to_string(),
        no_limit_price: quote.no_price.to_string(),
        signal_gross_edge_per_share: quote.gross_edge_per_share.to_string(),
        signal_yes_fee_per_share: quote.yes_fee_per_share.to_string(),
        signal_no_fee_per_share: quote.no_fee_per_share.to_string(),
        signal_net_edge_per_share: quote.edge_per_share.to_string(),
        required_gross_edge_per_share: assessment.required_gross_edge_per_share.to_string(),
        required_net_edge_per_share: assessment.required_net_edge_per_share.to_string(),
        required_dynamic_gross_edge_per_share: assessment
            .required_dynamic_gross_edge_per_share
            .to_string(),
        passes_gross_edge: assessment.passes_gross_edge,
        passes_net_edge: assessment.passes_net_edge,
        dynamic_fee_gate_active: assessment.dynamic_fee_gate_active,
        passes_active_gate: assessment.passes_active_gate,
        max_leg_notional_usdc: max_leg_notional_usdc.to_string(),
        requested_size: snapshot.yes_fill.requested.to_string(),
        yes_filled: snapshot.yes_fill.filled.to_string(),
        no_filled: snapshot.no_fill.filled.to_string(),
        yes_average_price: snapshot.yes_fill.average_price.to_string(),
        no_average_price: snapshot.no_fill.average_price.to_string(),
        yes_cost: snapshot.yes_fill.cost.to_string(),
        no_cost: snapshot.no_fill.cost.to_string(),
        yes_fee: snapshot.yes_fill.fee.to_string(),
        no_fee: snapshot.no_fill.fee.to_string(),
        paired_size: paired_size.to_string(),
        gross_edge_per_share,
        executable_edge_per_share,
        both_fillable: snapshot.yes_fill.filled > dec!(0) && snapshot.no_fill.filled > dec!(0),
        full_size_fillable: snapshot.yes_fill.requested >= quote.minimum_order_size
            && snapshot.yes_fill.filled >= snapshot.yes_fill.requested
            && snapshot.no_fill.filled >= snapshot.no_fill.requested,
        meets_minimum_order_size: snapshot.yes_fill.requested >= quote.minimum_order_size,
        minimum_order_size: quote.minimum_order_size.to_string(),
        signal_yes_book_age_ms: signal_observed_at_ms.saturating_sub(opp.yes_book_timestamp_ms),
        signal_no_book_age_ms: signal_observed_at_ms.saturating_sub(opp.no_book_timestamp_ms),
        signal_book_skew_ms: (opp.yes_book_timestamp_ms - opp.no_book_timestamp_ms).abs(),
        rest_yes_book_updated_at_ms: snapshot.yes_book_updated_at_ms,
        rest_no_book_updated_at_ms: snapshot.no_book_updated_at_ms,
        error: snapshot.error.clone(),
    };
    append_record(file_path, &record)
}

#[allow(clippy::too_many_arguments)]
pub fn log_arbitrage_admission_shadow(
    rejection: AdmissionRejection,
    opp: &ArbitrageOpportunity,
    assessment: &ShadowExecutionAssessment,
    market_name: &str,
    yes_direction: &str,
    no_direction: &str,
    signal_observed_at_ms: i64,
    file_path: &str,
) -> Result<()> {
    let quote = &assessment.quote;
    let record = ArbitrageAdmissionShadowRecord {
        action: "admission_rejected",
        timestamp: Utc::now().to_rfc3339(),
        signal_observed_at_ms,
        admission_rejection: rejection.as_str(),
        market_id: format!("{:?}", opp.market_id),
        market_name: market_name.to_string(),
        yes_token_id: opp.yes_token_id.to_string(),
        no_token_id: opp.no_token_id.to_string(),
        yes_direction: yes_direction.to_string(),
        no_direction: no_direction.to_string(),
        raw_yes_ask_price: opp.yes_ask_price.to_string(),
        raw_no_ask_price: opp.no_ask_price.to_string(),
        raw_total_price: (opp.yes_ask_price + opp.no_ask_price).to_string(),
        raw_profit_percentage: opp.profit_percentage.to_string(),
        yes_ask_size: opp.yes_size.to_string(),
        no_ask_size: opp.no_size.to_string(),
        yes_depth_usdc: (opp.yes_ask_price * opp.yes_size).to_string(),
        no_depth_usdc: (opp.no_ask_price * opp.no_size).to_string(),
        yes_limit_price: quote.yes_price.to_string(),
        no_limit_price: quote.no_price.to_string(),
        quote_size: quote.size.to_string(),
        quote_total_cost: quote.total_cost.to_string(),
        signal_gross_edge_per_share: quote.gross_edge_per_share.to_string(),
        signal_yes_fee_per_share: quote.yes_fee_per_share.to_string(),
        signal_no_fee_per_share: quote.no_fee_per_share.to_string(),
        signal_net_edge_per_share: quote.edge_per_share.to_string(),
        required_gross_edge_per_share: assessment.required_gross_edge_per_share.to_string(),
        required_net_edge_per_share: assessment.required_net_edge_per_share.to_string(),
        required_dynamic_gross_edge_per_share: assessment
            .required_dynamic_gross_edge_per_share
            .to_string(),
        passes_gross_edge: assessment.passes_gross_edge,
        passes_net_edge: assessment.passes_net_edge,
        dynamic_fee_gate_active: assessment.dynamic_fee_gate_active,
        passes_active_gate: assessment.passes_active_gate,
        fee_rate: quote.fee_rate.to_string(),
        fee_exponent: quote.fee_exponent,
        minimum_order_size: quote.minimum_order_size.to_string(),
        signal_yes_book_age_ms: signal_observed_at_ms.saturating_sub(opp.yes_book_timestamp_ms),
        signal_no_book_age_ms: signal_observed_at_ms.saturating_sub(opp.no_book_timestamp_ms),
        signal_book_skew_ms: (opp.yes_book_timestamp_ms - opp.no_book_timestamp_ms).abs(),
    };
    append_record(file_path, &record)
}

/// Persist a depth-rejected candidate without changing production admission.
/// `survives_250ms` means the same market remained continuously price-positive
/// and depth-rejected for at least the paper second-leg delay. The quote may
/// change during that interval; a no-edge or depth-admitted observation resets
/// the duration.
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
    use crate::trading::paper::PaperFill;
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

    #[test]
    fn rest_shadow_record_contains_actual_timing_and_both_leg_state() {
        let path = std::env::temp_dir().join(format!(
            "polypulse-rest-shadow-{}.ndjson",
            uuid::Uuid::new_v4()
        ));
        let opportunity = ArbitrageOpportunity {
            market_id: B256::ZERO,
            yes_token_id: U256::from(1),
            no_token_id: U256::from(2),
            yes_ask_price: dec!(0.45),
            no_ask_price: dec!(0.50),
            total_cost: dec!(9.5),
            profit_percentage: dec!(5),
            yes_size: dec!(10),
            no_size: dec!(10),
            yes_book_timestamp_ms: 9_900,
            no_book_timestamp_ms: 9_850,
        };
        let assessment = ShadowExecutionAssessment {
            quote: ExecutionQuote {
                yes_price: dec!(0.45),
                no_price: dec!(0.50),
                size: dec!(10),
                total_cost: dec!(9.5),
                gross_edge_per_share: dec!(0.05),
                yes_fee_per_share: dec!(0),
                no_fee_per_share: dec!(0),
                edge_per_share: dec!(0.05),
                fee_rate: dec!(0),
                fee_exponent: 1,
                minimum_order_size: dec!(1),
            },
            required_gross_edge_per_share: dec!(0.03),
            required_net_edge_per_share: dec!(0.003),
            required_dynamic_gross_edge_per_share: dec!(0.003),
            passes_gross_edge: true,
            passes_net_edge: true,
            dynamic_fee_gate_active: true,
            passes_active_gate: true,
        };
        let snapshot = ArbitrageRestSnapshot {
            observed_at_ms: 10_180,
            request_latency_ms: 80,
            yes_book_updated_at_ms: Some(10_100),
            no_book_updated_at_ms: Some(10_110),
            yes_fill: PaperFill {
                requested: dec!(10),
                filled: dec!(10),
                cost: dec!(4.5),
                fee: dec!(0),
                average_price: dec!(0.45),
            },
            no_fill: PaperFill {
                requested: dec!(10),
                filled: dec!(8),
                cost: dec!(4),
                fee: dec!(0),
                average_price: dec!(0.50),
            },
            error: None,
        };
        log_arbitrage_rest_shadow(
            "full_depth",
            None,
            &opportunity,
            &assessment,
            "btc prediction market",
            10_000,
            100,
            dec!(5),
            &snapshot,
            path.to_str().unwrap(),
        )
        .unwrap();
        let record: serde_json::Value =
            serde_json::from_str(std::fs::read_to_string(&path).unwrap().trim()).unwrap();
        assert_eq!(record["action"], "rest_shadow");
        assert_eq!(record["actual_signal_age_ms"], 180);
        assert_eq!(record["schedule_lag_ms"], 80);
        assert_eq!(record["both_fillable"], true);
        assert_eq!(record["full_size_fillable"], false);
        assert_eq!(record["meets_minimum_order_size"], true);
        assert_eq!(record["minimum_order_size"], "1");
        assert_eq!(record["gross_edge_per_share"], "0.05");
        assert_eq!(record["executable_edge_per_share"], "0.05");
        assert_eq!(record["yes_fee"], "0");
        assert_eq!(record["no_fee"], "0");
        assert_eq!(record["max_leg_notional_usdc"], "5");
        assert_eq!(record["paired_size"], "8");
        assert_eq!(record["signal_net_edge_per_share"], "0.05");
        assert_eq!(record["required_gross_edge_per_share"], "0.03");
        assert_eq!(record["required_dynamic_gross_edge_per_share"], "0.003");
        assert_eq!(record["dynamic_fee_gate_active"], true);
        assert_eq!(record["passes_active_gate"], true);
        assert_eq!(record["passes_net_edge"], true);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn admission_shadow_records_counterfactual_net_edge() {
        let path = std::env::temp_dir().join(format!(
            "polypulse-admission-shadow-{}.ndjson",
            uuid::Uuid::new_v4()
        ));
        let opportunity = ArbitrageOpportunity {
            market_id: B256::ZERO,
            yes_token_id: U256::from(1),
            no_token_id: U256::from(2),
            yes_ask_price: dec!(0.49),
            no_ask_price: dec!(0.49),
            total_cost: dec!(9.8),
            profit_percentage: dec!(2),
            yes_size: dec!(10),
            no_size: dec!(10),
            yes_book_timestamp_ms: 9_900,
            no_book_timestamp_ms: 9_850,
        };
        let assessment = ShadowExecutionAssessment {
            quote: ExecutionQuote {
                yes_price: dec!(0.49),
                no_price: dec!(0.49),
                size: dec!(10),
                total_cost: dec!(10.1),
                gross_edge_per_share: dec!(0.02),
                yes_fee_per_share: dec!(0.015),
                no_fee_per_share: dec!(0.015),
                edge_per_share: dec!(-0.01),
                fee_rate: dec!(0.25),
                fee_exponent: 2,
                minimum_order_size: dec!(5),
            },
            required_gross_edge_per_share: dec!(0.03),
            required_net_edge_per_share: dec!(0.003),
            required_dynamic_gross_edge_per_share: dec!(0.033),
            passes_gross_edge: false,
            passes_net_edge: false,
            dynamic_fee_gate_active: true,
            passes_active_gate: false,
        };
        log_arbitrage_admission_shadow(
            AdmissionRejection::ExecutionSpread,
            &opportunity,
            &assessment,
            "btc prediction market",
            "↑",
            "↓",
            10_000,
            path.to_str().unwrap(),
        )
        .unwrap();
        let record: serde_json::Value =
            serde_json::from_str(std::fs::read_to_string(&path).unwrap().trim()).unwrap();
        assert_eq!(record["action"], "admission_rejected");
        assert_eq!(record["admission_rejection"], "execution_spread");
        assert_eq!(record["signal_net_edge_per_share"], "-0.01");
        assert_eq!(record["passes_gross_edge"], false);
        assert_eq!(record["yes_depth_usdc"], "4.90");
        std::fs::remove_file(path).unwrap();
    }
}
