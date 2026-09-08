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
    run_id: String,
    candidate_stage: String,
    asset: String,
    ask_bucket: String,
    seconds_bucket: String,
    research_cohorts: Vec<String>,
    probability_gap: Decimal,
    model_probability: Option<f64>,
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
    shadow_shares: Decimal,
    shadow_pnl: Decimal,
}

#[derive(Debug, Default)]
struct CalibrationStats {
    evaluations: u64,
    model_below_market: u64,
    probability_gap_sum: Decimal,
}

#[derive(Debug, Default)]
struct OrderLifecycle {
    stage: String,
    attempted: bool,
    filled: Decimal,
    cost: Decimal,
    settled: bool,
    realized_pnl: Decimal,
}

#[derive(Debug, Default)]
struct OrderStageStats {
    attempts: u64,
    fills: u64,
    settled: u64,
    wins: u64,
    capital: Decimal,
    realized_pnl: Decimal,
}

impl CalibrationStats {
    fn record(&mut self, gap: Decimal) {
        self.evaluations += 1;
        self.model_below_market += u64::from(gap < Decimal::ZERO);
        self.probability_gap_sum += gap;
    }
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
            self.shadow_shares += shadow.filled;
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
        "{label:<18} markets={:<4} wins={:<4} win_rate={:>7.2}% lower95={:>7.2}% cf_pnl/share={:>10} shadow={:<4} shadow_wins={:<4} shadow_pnl={:>10} shadow_return={:>7.3}%",
        stats.markets,
        stats.wins,
        win_rate * 100.0,
        wilson_lower_bound(stats.wins, stats.markets) * 100.0,
        stats.counterfactual_pnl_per_share,
        stats.shadow_fills,
        stats.shadow_wins,
        stats.shadow_pnl,
        shadow_return * 100.0,
    );
    let break_even = if stats.shadow_shares > Decimal::ZERO {
        (stats.shadow_capital / stats.shadow_shares)
            .to_f64()
            .unwrap_or_default()
    } else {
        0.0
    };
    println!("  filled_win_lower95={:.2}% approximate_break_even={:.2}% (cost/share; assumes comparable win probabilities)",
        wilson_lower_bound(stats.shadow_wins, stats.shadow_fills) * 100.0, break_even * 100.0);
}

fn load_order_stage_stats(path: &PathBuf) -> Result<BTreeMap<String, OrderStageStats>> {
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let reader = BufReader::new(
        File::open(path).with_context(|| format!("cannot open {}", path.display()))?,
    );
    let mut positions = HashMap::<String, OrderLifecycle>::new();
    for (index, line) in reader.lines().enumerate() {
        let line = line.with_context(|| format!("cannot read {}:{}", path.display(), index + 1))?;
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(&line)
            .with_context(|| format!("invalid JSON at {}:{}", path.display(), index + 1))?;
        let position = &value["position"];
        let Some(position_id) = position.get("position_id").and_then(Value::as_str) else {
            continue;
        };
        let lifecycle = positions.entry(position_id.to_string()).or_default();
        if lifecycle.stage.is_empty() {
            lifecycle.stage = position
                .get("candidate_stage")
                .and_then(Value::as_str)
                .filter(|stage| !stage.is_empty())
                .unwrap_or("legacy")
                .to_string();
        }
        match value.get("action").and_then(Value::as_str) {
            Some("attempted") => lifecycle.attempted = true,
            Some("pending") => {
                lifecycle.filled = decimal(position.get("filled"));
                lifecycle.cost = lifecycle.filled * decimal(position.get("price"));
            }
            Some("settled") => {
                lifecycle.settled = true;
                lifecycle.realized_pnl = decimal(value.get("realized_pnl"));
            }
            _ => {}
        }
    }
    let mut by_stage = BTreeMap::<String, OrderStageStats>::new();
    for lifecycle in positions.values() {
        let stats = by_stage.entry(lifecycle.stage.clone()).or_default();
        stats.attempts += u64::from(lifecycle.attempted);
        stats.fills += u64::from(lifecycle.filled > Decimal::ZERO);
        stats.settled += u64::from(lifecycle.settled);
        stats.wins += u64::from(lifecycle.settled && lifecycle.realized_pnl > Decimal::ZERO);
        stats.capital += lifecycle.cost;
        stats.realized_pnl += lifecycle.realized_pnl;
    }
    Ok(by_stage)
}

fn reference_audit(path: &std::path::Path, evaluations: &HashMap<String, Value>) -> Result<()> {
    let cache_path = path.with_file_name("reference-audit.json");
    let mut cache: BTreeMap<String, Value> = if cache_path.exists() {
        serde_json::from_reader(File::open(&cache_path)?)?
    } else {
        BTreeMap::new()
    };
    if std::env::args().any(|arg| arg == "--reference-audit") {
        let slugs = evaluations
            .values()
            .filter_map(|e| e["slug"].as_str())
            .collect::<std::collections::BTreeSet<_>>();
        tokio::runtime::Runtime::new()?.block_on(async {
            let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(15)).build()?;
            for slug in slugs {
                if cache.get(slug).is_some_and(|v| v["closed"] == true && number(&v["eventMetadata"]["finalPrice"]).is_some()) { continue; }
                let result = async {
                    let events: Value = client.get("https://gamma-api.polymarket.com/events")
                        .query(&[("slug", slug)]).send().await?.error_for_status()?.json().await?;
                    let event = events.as_array().and_then(|a| a.iter().find(|v| v["slug"] == slug))
                        .context("Gamma returned no matching slug")?;
                    Ok::<_, anyhow::Error>(serde_json::json!({"slug":slug,"closed":event["closed"],
                        "description":event["description"],"eventMetadata":event["eventMetadata"],"fetched_at":chrono::Utc::now()}))
                }.await;
                match result { Ok(event) => { cache.insert(slug.into(), event); }, Err(error) => eprintln!("reference audit {slug}: {error}") }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            Ok::<_, anyhow::Error>(())
        })?;
        let tmp = cache_path.with_extension("json.tmp");
        serde_json::to_writer_pretty(File::create(&tmp)?, &cache)?;
        std::fs::rename(tmp, &cache_path)?;
    }
    println!("REFERENCE AUDIT: cached Gamma priceToBeat/finalPrice; refresh missing values with --reference-audit");
    let mut selected = BTreeMap::<(String, String, String), &Value>::new();
    for e in evaluations.values() {
        let key = (
            e["run_id"].as_str().unwrap_or("legacy").into(),
            e["slug"].as_str().unwrap_or("unknown").into(),
            time_bucket(e),
        );
        if selected
            .get(&key)
            .is_none_or(|old| e["evaluated_at"].as_str() < old["evaluated_at"].as_str())
        {
            selected.insert(key, e);
        }
    }
    let mut groups = BTreeMap::<String, (u64, f64, f64, f64)>::new();
    let mut missing = 0;
    let mut source_unknown = 0;
    for ((_, slug, bucket), e) in selected {
        let Some(reference) = cache.get(&slug).filter(|r| r["closed"] == true) else {
            missing += 1;
            continue;
        };
        let description = reference["description"].as_str().unwrap_or("");
        if !description.contains("twap-60s") {
            source_unknown += 1;
        }
        let (Some(final_price), Some(strike), Some(open), Some(current), Some(volatility)) = (
            number(&reference["eventMetadata"]["finalPrice"]),
            number(&reference["eventMetadata"]["priceToBeat"]),
            number(&e["opening_twap"]),
            number(&e["current_twap"]),
            number(&e["volatility_bps_per_sqrt_second"]),
        ) else {
            missing += 1;
            continue;
        };
        if strike <= 0.0 || current <= 0.0 || volatility <= 0.0 {
            missing += 1;
            continue;
        }
        let seconds = e["seconds_remaining"].as_i64().unwrap_or(0);
        if seconds <= 0 {
            missing += 1;
            continue;
        }
        let residual = (final_price / current - 1.0) * 10000.0;
        let normalized = residual / (volatility * (seconds as f64).sqrt());
        let stats = groups
            .entry(format!(
                "{} {bucket}",
                e["asset"].as_str().unwrap_or("unknown")
            ))
            .or_default();
        stats.0 += 1;
        stats.1 += residual * residual;
        stats.2 += normalized * normalized;
        stats.3 = stats.3.max(((open / strike - 1.0) * 10000.0).abs());
    }
    for (group, (n, sum, normalized, opening)) in groups {
        println!("  {group}: markets={n} terminal_error_rms_bps={:.5} normalized_error_rms={:.5} max_open_vs_official_strike_bps={opening:.5}",
            (sum / n as f64).sqrt(), (normalized / n as f64).sqrt());
    }
    println!("  missing_or_unusable={missing} source_not_explicit_60s={source_unknown}; candidate-selected samples, cross-horizon groups overlap; no automatic calibration");
    Ok(())
}

fn number(v: &Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_str()?.parse().ok())
        .filter(|v| v.is_finite())
}

fn counterfactual_entry(evaluation: &Value) -> Decimal {
    decimal(evaluation.get("ask_price")) + decimal(evaluation.get("fee_per_share"))
}

fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    let path = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("LATE_RESOLUTION_WINNER_TRADING_EVALUATION_JOURNAL_FILE")
                .map(PathBuf::from)
        })
        .unwrap_or_else(|| {
            PathBuf::from("results/late-resolution-winner-trading/paper/evaluations.ndjson")
        });
    let reader = BufReader::new(
        File::open(&path).with_context(|| format!("cannot open {}", path.display()))?,
    );
    let mut evaluations = HashMap::new();
    let mut runs = BTreeMap::<String, (String, String, bool)>::new();
    let mut shadows = HashMap::new();
    let mut latest_shadow_by_slug = HashMap::<String, ShadowFill>::new();
    let mut sensitivity_shadows = HashMap::<(String, String, String), ShadowFill>::new();
    let mut sensitivity_counts = BTreeMap::<String, (u64, u64, u64)>::new();
    let mut raw_evaluations = HashMap::<String, Value>::new();
    let mut first_candidates = BTreeMap::<(String, String), String>::new();
    let mut last_candidates = BTreeMap::<(String, String), String>::new();
    let mut first_confirmed = BTreeMap::<(String, String), String>::new();
    let mut directions = BTreeMap::<(String, String), std::collections::HashSet<bool>>::new();
    let mut resolved_by_evaluation = HashMap::<String, bool>::new();
    let mut resolutions = Vec::new();
    let mut failed_checks = BTreeMap::<String, u64>::new();
    let mut audit_samples = 0_u64;
    let mut floor_active = 0_u64;
    let mut max_opening_skew_ms = 0_u64;
    let mut horizon_stats = BTreeMap::<String, (u64, f64)>::new();
    for (index, line) in reader.lines().enumerate() {
        let line = line.with_context(|| format!("cannot read line {}", index + 1))?;
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(&line)
            .with_context(|| format!("invalid JSON at {}:{}", path.display(), index + 1))?;
        match value.get("action").and_then(Value::as_str) {
            Some("sensitivity_observed") => {
                if let Some(groups) = value["decision"]["groups"].as_array() {
                    for group in groups {
                        let stats = sensitivity_counts
                            .entry(group["group"].as_str().unwrap_or("unknown").into())
                            .or_default();
                        stats.0 += 1;
                        stats.1 += u64::from(group["admitted"] == true);
                        stats.2 += u64::from(group["confirmed"] == true);
                    }
                }
            }
            Some("run_started") => {
                let run_id = value
                    .get("run_id")
                    .and_then(Value::as_str)
                    .unwrap_or("legacy")
                    .to_string();
                runs.insert(
                    run_id,
                    (
                        value
                            .get("started_at")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        value
                            .get("mode")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown")
                            .to_string(),
                        value
                            .get("hybrid_paper_enabled")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    ),
                );
            }
            Some("evaluated") => {
                let evaluation = &value["evaluation"];
                if let Some(id) = evaluation["evaluation_id"].as_str() {
                    let key = (
                        evaluation["run_id"]
                            .as_str()
                            .unwrap_or("legacy")
                            .to_string(),
                        evaluation["slug"].as_str().unwrap_or("unknown").to_string(),
                    );
                    first_candidates
                        .entry(key.clone())
                        .or_insert_with(|| id.to_string());
                    last_candidates.insert(key.clone(), id.to_string());
                    if let Some(direction) = evaluation["buy_yes"].as_bool() {
                        directions.entry(key).or_default().insert(direction);
                    }
                    raw_evaluations.insert(id.to_string(), evaluation.clone());
                }
                for reason in string_list(evaluation.get("failed_checks")) {
                    *failed_checks.entry(reason).or_default() += 1;
                }
                if let Some(audit) = evaluation.get("model_audit").filter(|v| v.is_object()) {
                    if let Some(horizons) = audit["horizon_volatility"].as_array() {
                        for h in horizons {
                            if let Some(v) = number(&h["bps_per_sqrt_second"]) {
                                let stats = horizon_stats
                                    .entry(format!(
                                        "{} {}s",
                                        evaluation["asset"].as_str().unwrap_or("unknown"),
                                        h["seconds"]
                                    ))
                                    .or_default();
                                stats.0 += 1;
                                stats.1 += v;
                            }
                        }
                    }
                    audit_samples += 1;
                    floor_active +=
                        u64::from(audit["volatility_floor_active"].as_bool().unwrap_or(false));
                    max_opening_skew_ms = max_opening_skew_ms.max(
                        audit["opening_skew_ms"]
                            .as_i64()
                            .unwrap_or(0)
                            .unsigned_abs(),
                    );
                }
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
                            model_probability: evaluation
                                .get("opening_twap")
                                .filter(|v| !v.is_null())
                                .and_then(|_| {
                                    decimal(evaluation.get("model_probability")).to_f64()
                                }),
                            run_id: evaluation
                                .get("run_id")
                                .and_then(Value::as_str)
                                .unwrap_or("legacy")
                                .to_string(),
                            candidate_stage: evaluation
                                .get("candidate_stage")
                                .and_then(Value::as_str)
                                .filter(|stage| !stage.is_empty())
                                .unwrap_or("legacy")
                                .to_string(),
                            asset,
                            ask_bucket,
                            seconds_bucket: time_bucket(evaluation),
                            research_cohorts,
                            probability_gap: evaluation
                                .get("model_market_probability_gap")
                                .map(|value| decimal(Some(value)))
                                .unwrap_or_else(|| {
                                    decimal(evaluation.get("model_probability"))
                                        - decimal(evaluation.get("ask_price"))
                                }),
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
                    if shadow.candidate_stage == "sensitivity_confirmed" {
                        for group in &shadow.research_cohorts {
                            let key = (
                                execution["run_id"].as_str().unwrap_or("legacy").to_string(),
                                slug.clone(),
                                group.clone(),
                            );
                            sensitivity_shadows
                                .entry(key)
                                .or_insert_with(|| shadow.clone());
                        }
                        continue;
                    }
                    shadows.insert(id.to_string(), shadow.clone());
                    let replace = latest_shadow_by_slug
                        .get(&slug)
                        .is_none_or(|existing| shadow.simulated_at >= existing.simulated_at);
                    if replace {
                        latest_shadow_by_slug.insert(slug, shadow);
                    }
                }
            }
            Some("confirmation_observed") => {
                let observation = &value["observation"];
                if observation["confirmed"] == true {
                    if let Some(id) = observation["evaluation_id"].as_str() {
                        let key = (
                            observation["run_id"]
                                .as_str()
                                .unwrap_or("legacy")
                                .to_string(),
                            observation["slug"]
                                .as_str()
                                .unwrap_or("unknown")
                                .to_string(),
                        );
                        first_confirmed.entry(key).or_insert_with(|| id.into());
                    }
                }
                if let (Some(id), Some(stage)) = (
                    observation.get("evaluation_id").and_then(Value::as_str),
                    observation
                        .get("candidate_stage")
                        .and_then(Value::as_str)
                        .filter(|stage| !stage.is_empty()),
                ) {
                    if let Some(meta) = evaluations.get_mut(id) {
                        meta.candidate_stage = stage.to_string();
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

    reference_audit(&path, &raw_evaluations)?;
    println!("HISTORICAL TWAP HORIZONS (correlated candidate observations; not settlement-calibrated volatility):");
    for (label, (n, sum)) in horizon_stats {
        println!(
            "  {label} observations={n} mean_bps_per_sqrt_second={:.6}",
            sum / n as f64
        );
    }
    println!("ENTRY SELECTION AUDIT (one record per run/market; no retrospective direction replacement):");
    println!(
        "direction_flip_markets={}",
        directions.values().filter(|d| d.len() > 1).count()
    );
    for (label, selection) in [
        ("first_candidate", &first_candidates),
        ("first_confirmed", &first_confirmed),
        ("last_candidate", &last_candidates),
    ] {
        let mut stats = GroupStats::default();
        let mut pending = 0;
        for id in selection.values() {
            if let Some(won) = resolved_by_evaluation.get(id) {
                // Model-unavailable records contain a zero placeholder in effective_entry_price.
                let raw = &raw_evaluations[id];
                let entry = counterfactual_entry(raw);
                stats.record(*won, if *won { Decimal::ONE - entry } else { -entry }, None);
            } else {
                pending += 1;
            }
        }
        print_group(label, &stats);
        println!("  pending={pending}; counterfactual only, no execution assumption");
    }
    println!(
        "\nSENSITIVITY SHADOW (first attempt per run/market/group; groups overlap; never sum PnL):"
    );
    let mut sensitivity_stats = BTreeMap::<String, GroupStats>::new();
    let mut sensitivity_pending = BTreeMap::<String, usize>::new();
    for ((_, _, group), shadow) in &sensitivity_shadows {
        if let Some(won) = resolved_by_evaluation.get(&shadow.evaluation_id) {
            sensitivity_stats.entry(group.clone()).or_default().record(
                *won,
                Decimal::ZERO,
                Some(shadow),
            );
        } else {
            *sensitivity_pending.entry(group.clone()).or_default() += 1;
        }
    }
    for (group, (evaluations, admitted, confirmed)) in &sensitivity_counts {
        println!("{group} evaluations={evaluations} admitted_evaluations={admitted} confirmed_evaluations={confirmed} pending_attempts={}", sensitivity_pending.get(group).unwrap_or(&0));
        print_group(
            group,
            sensitivity_stats
                .get(group)
                .unwrap_or(&GroupStats::default()),
        );
    }
    for (group, stats) in &sensitivity_stats {
        if !sensitivity_counts.contains_key(group) {
            print_group(group, stats);
        }
    }
    let mut overall = GroupStats::default();
    let mut by_stage = BTreeMap::<String, GroupStats>::new();
    let mut by_asset_price = BTreeMap::<String, GroupStats>::new();
    let mut by_time = BTreeMap::<String, GroupStats>::new();
    let mut missing_evaluations = 0_u64;
    let mut outcome_calibration = BTreeMap::<String, (u64, u64, f64, f64)>::new();
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
        if let Some(probability) = meta.model_probability {
            let bucket = if probability >= 0.9 {
                "0.90+"
            } else {
                "below_0.90"
            };
            let stats = outcome_calibration
                .entry(format!("{} model={bucket}", meta.asset))
                .or_default();
            stats.0 += 1;
            stats.1 += u64::from(won);
            stats.2 += probability;
            stats.3 += (probability - if won { 1.0 } else { 0.0 }).powi(2);
        }
        let shadow = shadows.get(id);
        overall.record(won, pnl, shadow);
        by_stage
            .entry(meta.candidate_stage.clone())
            .or_default()
            .record(won, pnl, shadow);
        by_asset_price
            .entry(format!("{} @{}", meta.asset, meta.ask_bucket))
            .or_default()
            .record(won, pnl, shadow);
        by_time
            .entry(meta.seconds_bucket.clone())
            .or_default()
            .record(won, pnl, shadow);
    }

    println!(
        "Late-Resolution Winner Trading market-level report: {}",
        path.display()
    );
    println!("Runs recorded: {}", runs.len());
    for (run_id, (started_at, mode, hybrid_enabled)) in &runs {
        println!(
            "  run_id={run_id} started_at={started_at} mode={mode} hybrid_paper_enabled={hybrid_enabled}"
        );
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
    println!("\nPRIMARY RESEARCH RESULT: confirmation-consistent shadow cohorts (latest attempt per market; simulated, NOT account PnL):");
    println!("Wins/win_rate describe all resolved attempts, including no-fill; shadow counts and PnL describe fills only. Cohorts overlap; do not sum them.");
    for (label, stats) in &by_cohort {
        print_group(label, stats);
    }
    println!("\nSECONDARY counterfactual outcomes: linked shadow is a selected-evaluation SUBSET, NOT the complete shadow result or strategy ROI:");
    print_group("all_stages", &overall);
    for (stage, stats) in &by_stage {
        print_group(stage, stats);
    }
    println!("\nCounterfactual by asset and ask price:");
    for (label, stats) in &by_asset_price {
        print_group(label, stats);
    }
    println!("\nCounterfactual by seconds remaining:");
    for (label, stats) in &by_time {
        print_group(label, stats);
    }
    println!("\nAll failed checks (overlapping; primary rejection counters omit later failures):");
    for (reason, count) in &failed_checks {
        println!("  {reason}: {count}");
    }
    println!("\nModel audit: samples={audit_samples} volatility_floor_active={floor_active} max_abs_opening_skew_ms={max_opening_skew_ms}");
    if audit_samples == 0 {
        println!(
            "  Legacy data: volatility floor/reference diagnostics unavailable; collect a new run."
        );
    }
    println!("\nResolved selected-market probability calibration (selection-biased research sample, not out-of-sample validation):");
    for (label, (n, wins, probability_sum, squared_error)) in outcome_calibration {
        println!(
            "  {label} markets={n} mean_probability={:.4} observed_win_rate={:.4} brier={:.4}",
            probability_sum / n as f64,
            wins as f64 / n as f64,
            squared_error / n as f64
        );
    }
    let mut calibration_by_stage = BTreeMap::<String, CalibrationStats>::new();
    let mut evaluations_by_run = BTreeMap::<String, u64>::new();
    let mut unpriced_evaluations = 0_u64;
    for meta in evaluations.values() {
        if meta.model_probability.is_some() {
            calibration_by_stage
                .entry(meta.candidate_stage.clone())
                .or_default()
                .record(meta.probability_gap);
        } else {
            unpriced_evaluations += 1;
        }
        *evaluations_by_run.entry(meta.run_id.clone()).or_default() += 1;
    }
    println!("\nModel-vs-market probability calibration:");
    println!("  missing_model_inputs_excluded={unpriced_evaluations} (placeholder zero is not a probability forecast)");
    for (stage, stats) in &calibration_by_stage {
        let average_gap = if stats.evaluations > 0 {
            stats.probability_gap_sum / Decimal::from(stats.evaluations)
        } else {
            Decimal::ZERO
        };
        println!(
            "{stage:<18} evaluations={:<4} model_below_market={:<4} avg_model_minus_market={average_gap}",
            stats.evaluations, stats.model_below_market
        );
    }
    println!("\nEvaluations by run:");
    for (run_id, count) in &evaluations_by_run {
        println!("  run_id={run_id} evaluations={count}");
    }
    let order_path = path
        .parent()
        .map(|parent| parent.join("orders.ndjson"))
        .unwrap_or_else(|| PathBuf::from("orders.ndjson"));
    let order_stages = load_order_stage_stats(&order_path)?;
    println!(
        "\nActual/paper order lifecycle by stage: {}",
        order_path.display()
    );
    if order_stages.is_empty() {
        println!("  no order journal found or no lifecycle records");
    }
    for (stage, stats) in &order_stages {
        let roi = if stats.capital > Decimal::ZERO {
            stats.realized_pnl / stats.capital * Decimal::from(100_u64)
        } else {
            Decimal::ZERO
        };
        println!(
            "{stage:<18} attempts={:<4} fills={:<4} settled={:<4} wins={:<4} capital={:>10} realized_pnl={:>10} roi={roi}%",
            stats.attempts,
            stats.fills,
            stats.settled,
            stats.wins,
            stats.capital,
            stats.realized_pnl,
        );
    }
    let strict_settled = order_stages
        .get("production")
        .map(|stats| stats.settled)
        .unwrap_or_default();
    println!(
        "\nStrict-model execution readiness: {strict_settled}/100 fully settled production fills ({})",
        if strict_settled >= 100 {
            "minimum reached; review confidence intervals and tail losses before live mode"
        } else {
            "insufficient; keep dry-run fail-closed"
        }
    );
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
    fn no_fill_outcome_does_not_change_filled_cohort_pnl() {
        let mut stats = GroupStats::default();
        let fill = ShadowFill {
            evaluation_id: "filled".into(),
            simulated_at: String::new(),
            candidate_stage: "rejected_confirmed".into(),
            research_cohorts: vec![],
            outcome: "filled".into(),
            filled: Decimal::from(5),
            cost: Decimal::from_str("4.9").unwrap(),
        };
        stats.record(true, Decimal::ZERO, Some(&fill));
        let no_fill = ShadowFill {
            outcome: "no_fill".into(),
            ..fill.clone()
        };
        stats.record(false, Decimal::ZERO, Some(&no_fill));
        assert_eq!(stats.markets, 2);
        assert_eq!(stats.wins, 1);
        assert_eq!(stats.shadow_fills, 1);
        assert_eq!(stats.shadow_wins, 1);
        assert_eq!(stats.shadow_pnl, Decimal::from_str("0.1").unwrap());
        stats.record(false, Decimal::ZERO, Some(&fill));
        assert_eq!(stats.shadow_pnl, Decimal::from_str("-4.8").unwrap());
    }

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

    #[test]
    fn missing_model_does_not_create_free_counterfactual_entry() {
        let evaluation = serde_json::json!({"ask_price":"0.98", "fee_per_share":"0.001372", "effective_entry_price":"0"});
        assert_eq!(
            counterfactual_entry(&evaluation),
            Decimal::from_str("0.981372").unwrap()
        );
    }
}
