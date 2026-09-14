//! Settlement-linked research observations. Never creates orders or changes model parameters.
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashSet},
    fs::File,
    io::{BufRead, BufReader},
    path::PathBuf,
};

fn number(v: &Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_str()?.parse().ok())
        .filter(|x| x.is_finite())
}

fn terminal(event: &Value) -> Option<Value> {
    let markets = event["markets"].as_array()?;
    // Slug lookup must be one binary Up/Down market, with explicit labels.
    if markets.len() != 1 {
        return None;
    }
    let m = &markets[0];
    if m["closed"] != true {
        return None;
    }
    fn array(v: &Value) -> Option<Vec<Value>> {
        v.as_array()
            .cloned()
            .or_else(|| serde_json::from_str(v.as_str()?).ok())
    }
    let labels = array(&m["outcomes"])?;
    let prices = array(&m["outcomePrices"])?;
    if labels.len() != 2 || prices.len() != 2 {
        return None;
    }
    let up = labels
        .iter()
        .position(|x| x.as_str().is_some_and(|s| s.eq_ignore_ascii_case("up")))?;
    let down = labels
        .iter()
        .position(|x| x.as_str().is_some_and(|s| s.eq_ignore_ascii_case("down")))?;
    let p = number(&prices[up])?;
    let q = number(&prices[down])?;
    let won = if p == 1.0 && q == 0.0 {
        true
    } else if p == 0.0 && q == 1.0 {
        false
    } else {
        return None;
    };
    Some(
        json!({"winner_up":won,"price_to_beat":number(&event["eventMetadata"]["priceToBeat"]),
        "final_price":number(&event["eventMetadata"]["finalPrice"]),"verified_at":chrono::Utc::now(),
        "confirmation":"closed_terminal_prices"}),
    )
}

#[derive(Default)]
struct Scores {
    n: usize,
    wins: usize,
    p: f64,
    brier: f64,
    log_loss: f64,
}
impl Scores {
    fn add(&mut self, p: f64, y: bool) {
        let target = if y { 1.0 } else { 0.0 };
        self.n += 1;
        self.wins += usize::from(y);
        self.p += p;
        self.brier += (p - target).powi(2);
        let p = p.clamp(1e-12, 1.0 - 1e-12);
        self.log_loss -= target * p.ln() + (1.0 - target) * (1.0 - p).ln();
    }
    fn value(&self) -> Value {
        let n = self.n.max(1) as f64;
        json!({"samples":self.n,"wins":self.wins,
        "mean_probability":self.p/n,"observed_win_rate":self.wins as f64/n,
        "brier_score":self.brier/n,"log_loss":self.log_loss/n})
    }
}

pub async fn run(refresh: bool) -> Result<PathBuf> {
    dotenvy::dotenv().ok();
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();
    let input = PathBuf::from(std::env::var(
        "SHORT_DURATION_DIGITAL_FAIR_VALUE_RESULTS_FILE",
    )?);
    let root = input.parent().context("journal has no parent")?;
    let cache_path = root.join("reference-audit.json");
    let mut cache: BTreeMap<String, Value> = if cache_path.exists() {
        serde_json::from_reader(File::open(&cache_path)?)?
    } else {
        BTreeMap::new()
    };
    let mut observations = Vec::new();
    let mut seen = HashSet::new();
    let mut duplicates = 0;
    for line in BufReader::new(File::open(&input)?).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let e: Value = serde_json::from_str(&line)?;
        if e["event"] != "model_observation" {
            continue;
        }
        let slug = e["slug"].as_str().context("observation missing slug")?;
        let checkpoint = e["checkpoint_secs"]
            .as_i64()
            .context("observation missing checkpoint")?;
        // Across restarts retain the first observation for each market/checkpoint.
        if !seen.insert((slug.to_owned(), checkpoint)) {
            duplicates += 1;
            continue;
        }
        observations.push(e);
    }
    let mut fetch_errors = Vec::new();
    if refresh {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()?;
        let slugs = observations
            .iter()
            .filter_map(|e| e["slug"].as_str())
            .collect::<std::collections::BTreeSet<_>>();
        let total = slugs.len();
        for (index, slug) in slugs.into_iter().enumerate() {
            if cache.get(slug).is_some_and(|v| {
                v["winner_up"].is_boolean()
                    && number(&v["final_price"]).is_some()
                    && number(&v["price_to_beat"]).is_some()
            }) {
                continue;
            }
            let fetched = async {
                let events: Vec<Value> = client
                    .get("https://gamma-api.polymarket.com/events")
                    .query(&[("slug", slug)])
                    .send()
                    .await?
                    .error_for_status()?
                    .json()
                    .await?;
                Ok::<_, anyhow::Error>(events.iter().find(|e| e["slug"] == slug).and_then(terminal))
            }
            .await;
            match fetched {
                Ok(Some(value)) => {
                    cache.insert(slug.into(), value);
                    let temp = cache_path.with_extension("json.tmp");
                    serde_json::to_writer_pretty(File::create(&temp)?, &cache)?;
                    std::fs::rename(temp, &cache_path)?;
                }
                Ok(None) => {
                    fetch_errors.push(json!({"slug":slug,"reason":"not_terminal_or_missing"}))
                }
                Err(e) => fetch_errors.push(json!({"slug":slug,"reason":format!("{e:#}")})),
            }
            println!(
                "reference audit {}/{} cached={}",
                index + 1,
                total,
                cache.len()
            );
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }
    let mut scores: BTreeMap<String, Scores> = BTreeMap::new();
    let mut residuals: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    let mut samples = Vec::new();
    let mut missing_labels = 0;
    let mut missing_probabilities = 0;
    for e in &observations {
        let slug = e["slug"].as_str().unwrap();
        let Some(label) = cache.get(slug).filter(|v| v["winner_up"].is_boolean()) else {
            missing_labels += 1;
            continue;
        };
        let won = label["winner_up"].as_bool().unwrap();
        let age = e["model_audit"]["feed_age_ms"].as_i64();
        let age_bucket = match age {
            Some(0..=1000) => "le_1s",
            Some(1001..=2000) => "1_2s",
            Some(2001..) => "gt_2s",
            _ => "unknown",
        };
        let group = format!(
            "{}/{}s/{}",
            e["asset"].as_str().unwrap_or("unknown"),
            e["checkpoint_secs"],
            age_bucket
        );
        for (name, value) in [
            ("model", &e["probability_up"]),
            ("market_midpoint", &e["market_probability_up"]),
            (
                "without_floor",
                &e["model_audit"]["probability_without_floor"],
            ),
        ] {
            let Some(p) = number(value).filter(|p| (0.0..=1.0).contains(p)) else {
                if name == "model" {
                    missing_probabilities += 1;
                }
                continue;
            };
            scores
                .entry(format!("{name}/{group}"))
                .or_default()
                .add(p, won);
            scores
                .entry(format!(
                    "{name}/{group}/probability_bin_{}",
                    (p * 10.0).floor().min(9.0) as u32
                ))
                .or_default()
                .add(p, won);
        }
        if let (Some(p), Some(q)) = (
            number(&e["probability_up"]),
            number(&e["market_probability_up"]),
        ) {
            if (0.0..=1.0).contains(&p) && (0.0..=1.0).contains(&q) {
                scores
                    .entry(format!("paired_model/{group}"))
                    .or_default()
                    .add(p, won);
                scores
                    .entry(format!("paired_market/{group}"))
                    .or_default()
                    .add(q, won);
            }
        }
        let residual = number(&label["final_price"])
            .zip(number(&e["current_twap"]))
            .filter(|(_, current)| *current > 0.0)
            .map(|(end, current)| (end / current - 1.0) * 10000.0);
        let opening_error = number(&e["opening_twap"])
            .zip(number(&label["price_to_beat"]))
            .filter(|(_, official)| *official > 0.0)
            .map(|(local, official)| (local / official - 1.0) * 10000.0);
        if let Some(error) = residual {
            residuals.entry(group.clone()).or_default().push(error);
        }
        samples.push(json!({"slug":slug,"asset":e["asset"],"checkpoint_secs":e["checkpoint_secs"],
            "run_id":e["run_id"],"seconds_remaining":e["seconds_remaining"],"feed_age_ms":age,
            "probability_up":e["probability_up"],"winner_up":won,"terminal_residual_bps":residual,"opening_error_bps":opening_error}));
    }
    let output = root.join("calibration-report.json");
    serde_json::to_writer_pretty(
        File::create(&output)?,
        &json!({"generated_at":chrono::Utc::now(),"source":input,
        "observations":observations.len(),"duplicates_excluded":duplicates,"missing_labels":missing_labels,
        "missing_model_probabilities":missing_probabilities,"fetch_errors":fetch_errors,
        "scores":scores.iter().map(|(k,v)|(k.clone(),v.value())).collect::<BTreeMap<_,_>>(),"samples":samples,
        "terminal_residuals":residuals.iter().map(|(k,v)|(k.clone(),json!({"samples":v.len(),
            "mean_bps":v.iter().sum::<f64>()/v.len() as f64,
            "rms_bps":(v.iter().map(|x|x*x).sum::<f64>()/v.len() as f64).sqrt()}))).collect::<BTreeMap<_,_>>(),
        "limitations":["Model-ready windows only; not all scheduled windows. Missing data never means zero probability.",
            "First observation per market/checkpoint across restarts. Checkpoint groups overlap; do not pool as independent trades.",
            "In-sample diagnostics only; no parameter fitting or automatic live promotion.",
            "Legacy observations lack baseline probability/current TWAP; no synthetic reconstruction."]}),
    )?;
    println!(
        "calibration report={} observations={} missing_labels={}",
        output.display(),
        observations.len(),
        missing_labels
    );
    Ok(output)
}

/// Evaluate probability candidates on chronological train/validation/holdout
/// partitions.  This is deliberately read-only: it does not fit or promote
/// live parameters, and it never contacts Gamma.
pub fn run_split_validation() -> Result<PathBuf> {
    dotenvy::dotenv().ok();
    let input = PathBuf::from(std::env::var(
        "SHORT_DURATION_DIGITAL_FAIR_VALUE_RESULTS_FILE",
    )?);
    let root = input.parent().context("journal has no parent")?;
    let cache_path = root.join("reference-audit.json");
    if !cache_path.exists() {
        anyhow::bail!(
            "reference audit cache is missing: {}. Run `short_duration_digital_fair_value validate --refresh` once while Gamma network access is available, then rerun validate",
            cache_path.display()
        );
    }
    let cache: BTreeMap<String, Value> = serde_json::from_reader(
        File::open(&cache_path).with_context(|| format!("open {}", cache_path.display()))?,
    )?;

    let mut observations = Vec::new();
    let mut seen = HashSet::new();
    for line in BufReader::new(File::open(&input)?).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let e: Value = serde_json::from_str(&line)?;
        if e["event"] != "model_observation" {
            continue;
        }
        let slug = e["slug"].as_str().context("observation missing slug")?;
        let checkpoint = e["checkpoint_secs"].as_i64().unwrap_or_default();
        if seen.insert((slug.to_owned(), checkpoint)) {
            observations.push(e);
        }
    }

    // A slug's final component is the market's UTC start timestamp. Sorting
    // unique slugs by it makes the holdout genuinely forward-looking while
    // keeping all assets from one five-minute market in the same partition.
    let mut slugs = observations
        .iter()
        .filter_map(|e| e["slug"].as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    slugs.sort_by_key(|slug| {
        slug.rsplit('-')
            .next()
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(i64::MAX)
    });
    slugs.dedup();
    let mut market_times = slugs
        .iter()
        .filter_map(|slug| slug.rsplit('-').next().and_then(|v| v.parse::<i64>().ok()))
        .collect::<Vec<_>>();
    market_times.sort_unstable();
    market_times.dedup();
    let n = market_times.len();
    let train_end = n.saturating_mul(60) / 100;
    let validation_end = n.saturating_mul(80) / 100;
    let split_for = |slug: &str| {
        let timestamp = slug
            .rsplit('-')
            .next()
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(i64::MAX);
        let index = market_times.binary_search(&timestamp).unwrap_or(0);
        if index < train_end {
            "train"
        } else if index < validation_end {
            "validation"
        } else {
            "holdout"
        }
    };

    let mut scores: BTreeMap<String, Scores> = BTreeMap::new();
    let mut sample_counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut missing_labels = 0usize;
    let mut high_probability: BTreeMap<String, Scores> = BTreeMap::new();
    for e in &observations {
        let slug = e["slug"].as_str().unwrap_or_default();
        let split = split_for(slug);
        let Some(label) = cache.get(slug).filter(|v| v["winner_up"].is_boolean()) else {
            missing_labels += 1;
            continue;
        };
        let won = label["winner_up"].as_bool().unwrap_or(false);
        let asset = e["asset"].as_str().unwrap_or("unknown");
        let checkpoint = e["checkpoint_secs"].as_i64().unwrap_or_default();
        let base = format!("{split}/{asset}/{checkpoint}s");
        *sample_counts.entry(base.clone()).or_default() += 1;
        for (source, value) in [
            ("model", &e["probability_up"]),
            (
                "without_floor",
                &e["model_audit"]["probability_without_floor"],
            ),
            ("market_midpoint", &e["market_probability_up"]),
        ] {
            let Some(probability) = number(value).filter(|p| (0.0..=1.0).contains(p)) else {
                continue;
            };
            scores
                .entry(format!("{base}/{source}"))
                .or_default()
                .add(probability, won);
            if probability >= 0.9 {
                high_probability
                    .entry(format!("{base}/{source}"))
                    .or_default()
                    .add(probability, won);
            }
        }
    }

    let output = std::env::var("SHORT_DURATION_DIGITAL_FAIR_VALUE_CALIBRATION_SPLIT_REPORT_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| root.join("calibration-split-report.json"));
    let score_values = scores
        .iter()
        .map(|(key, score)| (key.clone(), score.value()))
        .collect::<BTreeMap<_, _>>();
    let high_values = high_probability
        .iter()
        .map(|(key, score)| (key.clone(), score.value()))
        .collect::<BTreeMap<_, _>>();
    serde_json::to_writer_pretty(
        File::create(&output)?,
        &json!({
            "schema_version": 1,
            "generated_at": chrono::Utc::now(),
            "source": input,
            "reference_audit": cache_path,
            "observations": observations.len(),
            "unique_markets": n,
            "missing_labels": missing_labels,
            "split_policy": {"train_percent": 60, "validation_percent": 20, "holdout_percent": 20,
                "ordering": "market_start_timestamp_ascending", "market_is_atomic": true},
            "sample_counts": sample_counts,
            "scores": score_values,
            "high_probability_ge_0_9": high_values,
            "limitations": [
                "Scores are diagnostics; no parameter is fitted or promoted automatically.",
                "Each market is assigned to one split, but checkpoints within a market remain correlated.",
                "Market midpoint is a reference baseline, not a guaranteed executable price.",
                "A small holdout or high-probability group must not be treated as proof of profitability."
            ]
        }),
    )?;
    println!(
        "split calibration report={} observations={} unique_markets={} missing_labels={}",
        output.display(),
        observations.len(),
        n,
        missing_labels
    );
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scores_and_terminal_labels() {
        let mut s = Scores::default();
        s.add(0.8, true);
        s.add(0.2, false);
        assert!((s.value()["brier_score"].as_f64().unwrap() - 0.04).abs() < 1e-10);
        let e = json!({"markets":[{"closed":true,"outcomes":["Down","Up"],"outcomePrices":[0,1]}]});
        assert_eq!(terminal(&e).unwrap()["winner_up"], true);
        assert!(terminal(
            &json!({"markets":[{"closed":false,"outcomes":["Up","Down"],"outcomePrices":[1,0]}]})
        )
        .is_none());
    }
}
