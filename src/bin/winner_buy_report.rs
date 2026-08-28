use anyhow::{Context, Result};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::str::FromStr;

#[derive(Debug, Clone)]
struct EvaluationMeta {
    asset: String,
    ask_bucket: String,
    seconds_bucket: String,
    research_cohorts: Vec<String>,
}

#[derive(Debug, Clone)]
struct ShadowFill {
    evaluation_id: String,
    simulated_at: String,
    candidate_stage: String,
    research_cohorts: Vec<String>,
    outcome: String,
    filled: Decimal,
    cost: Decimal,
}

#[derive(Debug, Default)]
struct GroupStats {
    markets: u64,
    wins: u64,
    counterfactual_pnl_per_share: Decimal,
    shadow_fills: u64,
    shadow_wins: u64,
    shadow_capital: Decimal,
    shadow_pnl: Decimal,
}

impl GroupStats {
    fn record(&mut self, won: bool, counterfactual_pnl: Decimal, shadow: Option<&ShadowFill>) {
        self.markets += 1;
        self.wins += u64::from(won);
        self.counterfactual_pnl_per_share += counterfactual_pnl;
        if let Some(shadow) = shadow.filter(|fill| fill.outcome == "filled") {
            self.shadow_fills += 1;
            self.shadow_wins += u64::from(won);
            self.shadow_capital += shadow.cost;
            self.shadow_pnl += if won {
                shadow.filled - shadow.cost
            } else {
                -shadow.cost
            };
        }
    }
}

fn decimal(value: Option<&Value>) -> Decimal {
    value
        .and_then(Value::as_str)
        .and_then(|raw| Decimal::from_str(raw).ok())
        .unwrap_or_default()
}

fn price_bucket(evaluation: &Value) -> String {
    evaluation
        .get("ask_price_bucket")
        .and_then(Value::as_str)
        .filter(|bucket| !bucket.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| decimal(evaluation.get("ask_price")).normalize().to_string())
}

fn time_bucket(evaluation: &Value) -> String {
    if let Some(bucket) = evaluation
        .get("seconds_remaining_bucket")
        .and_then(Value::as_str)
        .filter(|bucket| !bucket.is_empty())
    {
        return bucket.to_string();
    }
    match evaluation
        .get("seconds_remaining")
        .and_then(Value::as_i64)
        .unwrap_or_default()
    {
        ..=20 => "00-20s",
        21..=40 => "21-40s",
        41..=60 => "41-60s",
        _ => "61s+",
    }
    .to_string()
}

fn string_list(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(ToOwned::to_owned)
        .collect()
}

fn legacy_cohorts(asset: &str, ask_bucket: &str) -> Vec<String> {
    let mut cohorts = vec![
        "all_candidates".to_string(),
        format!("asset_{}", asset.trim().to_ascii_lowercase()),
        format!("price_{}", ask_bucket.replace('.', "_")),
    ];
    if matches!(ask_bucket, "0.96" | "0.97") {
        cohorts.push("price_096_097".to_string());
    }
    cohorts
}

fn wilson_lower_bound(wins: u64, total: u64) -> f64 {
    if total == 0 {
        return 0.0;
    }
    let n = total as f64;
    let p = wins as f64 / n;
    let z = 1.959_963_984_540_054;
    let denominator = 1.0 + z * z / n;
    let centre = p + z * z / (2.0 * n);
    let adjustment = z * ((p * (1.0 - p) + z * z / (4.0 * n)) / n).sqrt();
    ((centre - adjustment) / denominator).max(0.0)
}

fn print_group(label: &str, stats: &GroupStats) {
    let win_rate = if stats.markets > 0 {
        stats.wins as f64 / stats.markets as f64
    } else {
        0.0
    };
    let shadow_return = if stats.shadow_capital > Decimal::ZERO {
        (stats.shadow_pnl / stats.shadow_capital)
            .to_f64()
            .unwrap_or_default()
    } else {
        0.0
    };
    println!(
        "{label:<18} markets={:<4} wins={:<4} win_rate={:>7.2}% lower95={:>7.2}% cf_pnl/share={:>10} shadow={:<4} shadow_pnl={:>10} shadow_return={:>7.3}%",
        stats.markets,
        stats.wins,
        win_rate * 100.0,
        wilson_lower_bound(stats.wins, stats.markets) * 100.0,
        stats.counterfactual_pnl_per_share,
        stats.shadow_fills,
        stats.shadow_pnl,
        shadow_return * 100.0,
    );
}

fn main() -> Result<()> {
    let path = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("WINNER_BUY_EVALUATION_JOURNAL_FILE").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("results/winner-buy/paper/evaluations.ndjson"));
    let reader = BufReader::new(
        File::open(&path).with_context(|| format!("cannot open {}", path.display()))?,
    );
    let mut evaluations = HashMap::new();
    let mut shadows = HashMap::new();
    let mut latest_shadow_by_slug = HashMap::<String, ShadowFill>::new();
    let mut resolved_by_evaluation = HashMap::<String, bool>::new();
    let mut resolutions = Vec::new();
    for (index, line) in reader.lines().enumerate() {
        let line = line.with_context(|| format!("cannot read line {}", index + 1))?;
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(&line)
            .with_context(|| format!("invalid JSON at {}:{}", path.display(), index + 1))?;
        match value.get("action").and_then(Value::as_str) {
            Some("evaluated") => {
                let evaluation = &value["evaluation"];
                if let Some(id) = evaluation.get("evaluation_id").and_then(Value::as_str) {
                    let asset = evaluation
                        .get("asset")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                        .to_string();
                    let ask_bucket = price_bucket(evaluation);
                    let mut research_cohorts = string_list(evaluation.get("research_cohorts"));
                    if research_cohorts.is_empty() {
                        research_cohorts = legacy_cohorts(&asset, &ask_bucket);
                    }
                    evaluations.insert(
                        id.to_string(),
                        EvaluationMeta {
                            asset,
                            ask_bucket,
                            seconds_bucket: time_bucket(evaluation),
                            research_cohorts,
                        },
                    );
                }
            }
            Some("shadow_executed") => {
                let execution = &value["execution"];
                if let Some(id) = execution.get("evaluation_id").and_then(Value::as_str) {
                    let slug = execution
                        .get("slug")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                        .to_string();
                    let mut research_cohorts = string_list(execution.get("research_cohorts"));
                    if research_cohorts.is_empty() {
                        research_cohorts = evaluations
                            .get(id)
                            .map(|meta| meta.research_cohorts.clone())
                            .unwrap_or_default();
                    }
                    let shadow = ShadowFill {
                        evaluation_id: id.to_string(),
                        simulated_at: execution
                            .get("simulated_at")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        candidate_stage: execution
                            .get("candidate_stage")
                            .and_then(Value::as_str)
                            .unwrap_or("legacy")
                            .to_string(),
                        research_cohorts,
                        outcome: execution
                            .get("outcome")
                            .and_then(Value::as_str)
                            .unwrap_or("missing")
                            .to_string(),
                        filled: decimal(execution.get("filled")),
                        cost: decimal(execution.get("cost")),
                    };
                    shadows.insert(id.to_string(), shadow.clone());
                    let replace = latest_shadow_by_slug
                        .get(&slug)
                        .is_none_or(|existing| shadow.simulated_at >= existing.simulated_at);
                    if replace {
                        latest_shadow_by_slug.insert(slug, shadow);
                    }
                }
            }
            Some("resolved") => {
                if let (Some(id), Some(won)) = (
                    value.get("evaluation_id").and_then(Value::as_str),
                    value.get("won").and_then(Value::as_bool),
                ) {
                    resolved_by_evaluation.insert(id.to_string(), won);
                }
            }
            Some("market_resolved") => resolutions.push(value),
            _ => {}
        }
    }

    let mut overall = GroupStats::default();
    let mut by_asset_price = BTreeMap::<String, GroupStats>::new();
    let mut by_time = BTreeMap::<String, GroupStats>::new();
    let mut missing_evaluations = 0_u64;
    for resolution in &resolutions {
        let Some(id) = resolution
            .get("selected_evaluation_id")
            .and_then(Value::as_str)
        else {
            continue;
        };
        let Some(meta) = evaluations.get(id) else {
            missing_evaluations += 1;
            continue;
        };
        let won = resolution
            .get("won")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let pnl = decimal(resolution.get("counterfactual_pnl_per_share"));
        let shadow = shadows.get(id);
        overall.record(won, pnl, shadow);
        by_asset_price
            .entry(format!("{} @{}", meta.asset, meta.ask_bucket))
            .or_default()
            .record(won, pnl, shadow);
        by_time
            .entry(meta.seconds_bucket.clone())
            .or_default()
            .record(won, pnl, shadow);
    }

    println!("Winner Buy market-level report: {}", path.display());
    print_group("overall", &overall);
    println!("\nBy asset and ask price:");
    for (label, stats) in &by_asset_price {
        print_group(label, stats);
    }
    println!("\nBy seconds remaining:");
    for (label, stats) in &by_time {
        print_group(label, stats);
    }
    let mut by_cohort = BTreeMap::<String, GroupStats>::new();
    let mut unresolved_shadows = 0_u64;
    for shadow in latest_shadow_by_slug.values() {
        let Some(won) = resolved_by_evaluation.get(&shadow.evaluation_id).copied() else {
            unresolved_shadows += 1;
            continue;
        };
        let mut cohorts = shadow.research_cohorts.clone();
        cohorts.push(format!("stage_{}", shadow.candidate_stage));
        for cohort in cohorts {
            by_cohort
                .entry(cohort)
                .or_default()
                .record(won, Decimal::ZERO, Some(shadow));
        }
    }
    println!("\nConfirmation-consistent shadow cohorts (latest attempt per market):");
    for (label, stats) in &by_cohort {
        print_group(label, stats);
    }
    if missing_evaluations > 0 {
        eprintln!("warning: {missing_evaluations} market records lack their selected evaluation");
    }
    if unresolved_shadows > 0 {
        eprintln!("warning: {unresolved_shadows} latest shadow attempts are not resolved yet");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wilson_bound_penalizes_small_perfect_samples() {
        assert!(wilson_lower_bound(32, 32) < 0.90);
        assert!(wilson_lower_bound(500, 500) > 0.99);
    }

    #[test]
    fn legacy_evaluations_receive_derived_buckets() {
        let evaluation = serde_json::json!({"ask_price":"0.980", "seconds_remaining":32});
        assert_eq!(price_bucket(&evaluation), "0.98");
        assert_eq!(time_bucket(&evaluation), "21-40s");
    }
}
