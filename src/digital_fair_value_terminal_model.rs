//! SDDFV-only inference for a frozen, conditional terminal TWAP distribution.
//! Scale is the final/current log-return deviation in bps. No sqrt-time
//! extrapolation or changes to the shared winner-trading price estimator.
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader};
use std::{collections::BTreeMap, fs, path::Path};

const FEATURES: [&str; 5] = [
    "twap_change_30_bps",
    "twap_change_60_bps",
    "chainlink_basis_bps",
    "reference_age_secs",
    "effective_horizon_secs",
];

#[derive(Clone)]
pub struct TerminalModel {
    pub sha256: String,
    artifact: Artifact,
}

#[derive(Clone, Deserialize)]
struct Artifact {
    schema_version: u64,
    model_type: String,
    market_duration_secs: i64,
    online_trading_enabled: bool,
    feature_names: Vec<String>,
    models: BTreeMap<String, Group>,
}

#[derive(Clone, Deserialize)]
struct Group {
    selected: Option<String>,
    candidates: BTreeMap<String, Candidate>,
}

#[derive(Clone, Copy, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Kind {
    AssetGaussian,
    ConditionalGaussian,
    ConditionalEmpirical,
    MarketOffsetLogistic,
}

#[derive(Clone, Deserialize)]
struct Candidate {
    kind: Kind,
    horizon_min_secs: f64,
    horizon_max_secs: f64,
    scale_unit: String,
    reference_topic: String,
    #[serde(default)]
    mean_bps: f64,
    #[serde(default)]
    beta: Vec<f64>,
    #[serde(default)]
    means: Vec<f64>,
    #[serde(default)]
    scales: Vec<f64>,
    sigma_floor_bps: f64,
    regime_sigma_bps: [f64; 4],
    activity_cutoff_bps: Option<f64>,
    horizon_cutoff_secs: Option<f64>,
    standardized_residuals: Vec<f64>,
    #[serde(default)]
    terminal_source: Option<Box<Candidate>>,
    #[serde(default)]
    max_correction: Option<f64>,
    #[serde(default)]
    offset_feature_names: Vec<String>,
}

#[derive(Clone)]
struct SignalFeatures {
    margin_bps: f64,
    horizon_secs: f64,
    reference_age_secs: f64,
    vector: Option<[f64; 5]>,
    market: Option<f64>,
}

fn number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str()?.parse().ok())
        .filter(|v| v.is_finite())
}

fn log_bps(a: &Value, b: &Value) -> Option<f64> {
    let (a, b) = (number(a)?, number(b)?);
    (a > 0.0 && b > 0.0).then(|| (a / b).ln() * 10_000.0)
}

fn signal_features(signal: &Value, end_ms: i64) -> Option<SignalFeatures> {
    let now = number(&signal["sampled_at_ms"])?;
    let observed = number(&signal["model_audit"]["reference_observed_at_ms"])
        .or_else(|| number(&signal["reference_timing"]["observed_at_ms"]))?;
    let margin_bps = log_bps(&signal["current_twap"], &signal["opening_twap"])?;
    let age = now - observed;
    let remaining = (end_ms as f64 - now) / 1000.0;
    if !(0.0..=5000.0).contains(&age)
        || remaining <= 0.0
        || (number(&signal["seconds_remaining"])? - remaining).abs() > 0.05
    {
        return None;
    }
    let mut result = SignalFeatures {
        margin_bps,
        horizon_secs: (end_ms as f64 - observed) / 1000.0,
        reference_age_secs: age / 1000.0,
        vector: None,
        market: if matches!(
            signal["baseline"]["source"].as_str(),
            Some("up_midpoint" | "down_midpoint")
        ) {
            number(&signal["baseline"]["probability_up"]).filter(|p| (0.0..=1.0).contains(p))
        } else {
            None
        },
    };
    let vector = (|| {
        let anchors = signal["twap_anchors"]["anchors"].as_array()?;
        let mut changes = [0.0; 2];
        for (index, seconds) in [30.0, 60.0].into_iter().enumerate() {
            let anchor = anchors
                .iter()
                .find(|a| number(&a["ago_ms"]) == Some(seconds * 1000.0))?;
            let elapsed = (observed - number(&anchor["observed_at_ms"])?) / 1000.0;
            if !(seconds - 10.0..=seconds + 10.0).contains(&elapsed) {
                return None;
            }
            changes[index] = (log_bps(&signal["current_twap"], &anchor["value"])? * seconds
                / elapsed)
                .clamp(-50.0, 50.0);
        }
        let spot = &signal["spot_asof"]["streams"]
            .as_array()?
            .iter()
            .find(|s| s["topic"] == "crypto_prices_chainlink")?["sample"];
        if !(0.0..=5000.0).contains(&(now - number(&spot["observed_at_ms"])?))
            || !(0.0..=2000.0).contains(&(now - number(&spot["received_at_ms"])?))
        {
            return None;
        }
        Some([
            changes[0],
            changes[1],
            log_bps(&spot["value"], &signal["current_twap"])?.clamp(-50.0, 50.0),
            result.reference_age_secs,
            result.horizon_secs,
        ])
    })();
    result.vector = vector;
    Some(result)
}

impl Candidate {
    fn validate(&self) -> Result<()> {
        if self.kind == Kind::MarketOffsetLogistic {
            let source = self
                .terminal_source
                .as_ref()
                .context("offset terminal source missing")?;
            if source.kind == Kind::MarketOffsetLogistic {
                bail!("recursive offset model rejected");
            }
            source.validate()?;
            if self.beta.len() != 5
                || self.means.len() != 5
                || self.scales.len() != 5
                || self.beta.iter().chain(&self.means).any(|v| !v.is_finite())
                || self.scales.iter().any(|v| !v.is_finite() || *v <= 0.0)
                || !self
                    .max_correction
                    .is_some_and(|v| v.is_finite() && (0.0..=0.10).contains(&v))
                || self.offset_feature_names
                    != [
                        "intercept",
                        "terminal_minus_market",
                        "signed_boundary_sigma",
                        "absolute_boundary_sigma",
                        "reference_age_fraction",
                    ]
            {
                bail!("invalid market offset coefficients or correction limit");
            }
            return Ok(());
        }
        if self.scale_unit != "terminal_return_bps"
            || self.reference_topic != "crypto_prices_twap_sixty"
            || !self.horizon_min_secs.is_finite()
            || !self.horizon_max_secs.is_finite()
            || self.horizon_min_secs <= 0.0
            || self.horizon_max_secs < self.horizon_min_secs
            || !self.mean_bps.is_finite()
            || !self.sigma_floor_bps.is_finite()
            || self.sigma_floor_bps <= 0.0
            || self
                .regime_sigma_bps
                .iter()
                .any(|v| !v.is_finite() || *v <= 0.0)
            || self.standardized_residuals.is_empty()
            || self.standardized_residuals.iter().any(|v| !v.is_finite())
            || self.standardized_residuals.windows(2).any(|p| p[0] > p[1])
        {
            bail!("invalid terminal model scale, horizon or residuals");
        }
        if self.kind != Kind::AssetGaussian
            && (self.beta.len() != FEATURES.len() + 1
                || self.means.len() != FEATURES.len()
                || self.scales.len() != FEATURES.len()
                || self.beta.iter().chain(&self.means).any(|v| !v.is_finite())
                || self.scales.iter().any(|v| !v.is_finite() || *v <= 0.0)
                || !self
                    .activity_cutoff_bps
                    .is_some_and(|v| v.is_finite() && v >= 0.0)
                || !self
                    .horizon_cutoff_secs
                    .is_some_and(|v| v.is_finite() && v > 0.0))
        {
            bail!("invalid conditional terminal coefficients");
        }
        Ok(())
    }

    fn predict(&self, f: &SignalFeatures) -> Option<Value> {
        if self.kind == Kind::MarketOffsetLogistic {
            let market = f.market?;
            let source = self.terminal_source.as_ref()?.predict(f)?;
            let terminal = source["probability_up"].as_f64()?;
            let sigma = source["terminal_sigma_bps"].as_f64()?;
            let x = [
                1.0,
                terminal - market,
                (f.margin_bps / sigma).clamp(-10.0, 10.0),
                (f.margin_bps.abs() / sigma).clamp(0.0, 10.0),
                f.reference_age_secs / 5.0,
            ];
            let clipped_market = market.clamp(1e-6, 1.0 - 1e-6);
            let offset = self
                .beta
                .iter()
                .zip(x)
                .zip(&self.means)
                .zip(&self.scales)
                .map(|(((b, x), m), s)| b * (x - m) / s)
                .sum::<f64>();
            let z = ((clipped_market / (1.0 - clipped_market)).ln() + offset).clamp(-35.0, 35.0);
            let p = (1.0 / (1.0 + (-z).exp()))
                .clamp(market - self.max_correction?, market + self.max_correction?)
                .clamp(1e-6, 1.0 - 1e-6);
            return p.is_finite().then(|| {
                json!({"probability_up":p,"market_probability_up":market,
                "terminal_probability_up":terminal,"terminal_sigma_bps":sigma,
                "terminal_mean_bps":source["terminal_mean_bps"],"scale_unit":"terminal_return_bps",
                "max_correction":self.max_correction,"decision_role":"research_only"})
            });
        }
        if !(self.horizon_min_secs..=self.horizon_max_secs).contains(&f.horizon_secs) {
            return None;
        }
        let (mu, regime) = if self.kind == Kind::AssetGaussian {
            (self.mean_bps, 0)
        } else {
            let x = f.vector?;
            let mu = self.beta[0]
                + x.into_iter()
                    .zip(&self.means)
                    .zip(&self.scales)
                    .zip(self.beta.iter().skip(1))
                    .map(|(((x, mean), scale), beta)| (x - mean) / scale * beta)
                    .sum::<f64>();
            (
                mu,
                usize::from(x[0].abs() + x[2].abs() > self.activity_cutoff_bps?)
                    + 2 * usize::from(f.horizon_secs > self.horizon_cutoff_secs?),
            )
        };
        let sigma = self
            .sigma_floor_bps
            .max(self.regime_sigma_bps[regime])
            .max(1e-6);
        let z = (f.margin_bps + mu) / sigma;
        let p = if self.kind == Kind::ConditionalEmpirical {
            let first = self.standardized_residuals.partition_point(|r| *r < -z);
            (1 + self.standardized_residuals.len() - first) as f64
                / (self.standardized_residuals.len() + 2) as f64
        } else {
            normal_cdf(z)
        };
        if !mu.is_finite() || !p.is_finite() {
            return None;
        }
        Some(json!({"probability_up":p.clamp(1e-6,1.0-1e-6),
            "terminal_mean_bps":mu,"terminal_sigma_bps":sigma,
            "effective_horizon_secs":f.horizon_secs,"reference_age_secs":f.reference_age_secs,
            "scale_unit":"terminal_return_bps"}))
    }
}

impl TerminalModel {
    pub fn has_group(&self, asset: &str, checkpoint: i64) -> bool {
        self.artifact
            .models
            .get(&format!("{asset}/{checkpoint}s"))
            .is_some_and(|group| !group.candidates.is_empty())
    }

    pub fn load(path: &Path, expected_sha256: &str) -> Result<Self> {
        if expected_sha256.len() != 64 || !expected_sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("terminal model requires a 64-character SHA-256 digest");
        }
        let bytes =
            fs::read(path).with_context(|| format!("read terminal model {}", path.display()))?;
        let sha256 = format!("{:x}", Sha256::digest(&bytes));
        if !sha256.eq_ignore_ascii_case(expected_sha256) {
            bail!("terminal model SHA-256 mismatch");
        }
        let artifact: Artifact = serde_json::from_slice(&bytes)?;
        if !matches!(
            (artifact.schema_version, artifact.model_type.as_str()),
            (1, "sddfv_terminal_residual_v1") | (2, "sddfv_terminal_residual_v2")
        ) || artifact.market_duration_secs != 300
            || artifact.online_trading_enabled
            || artifact.feature_names != FEATURES
            || artifact.models.is_empty()
        {
            bail!("incompatible terminal model artifact; only 5m observation models are supported");
        }
        for (key, group) in &artifact.models {
            if !["btc", "eth", "sol"]
                .iter()
                .any(|a| [10, 30, 60].iter().any(|h| key == &format!("{a}/{h}s")))
                || group
                    .selected
                    .as_ref()
                    .is_some_and(|name| !group.candidates.contains_key(name))
            {
                bail!("invalid terminal model group {key}");
            }
            for candidate in group.candidates.values() {
                if artifact.schema_version == 1 && candidate.kind == Kind::MarketOffsetLogistic {
                    bail!("market offset requires schema v2");
                }
                candidate.validate()?;
            }
        }
        Ok(Self { sha256, artifact })
    }

    pub fn model_type(&self) -> &str {
        &self.artifact.model_type
    }

    /// This audit never replaces the live decision probability or permits orders.
    pub fn audit(&self, asset: &str, checkpoint: i64, signal: &Value, end_ms: i64) -> Value {
        let Some(group) = self.artifact.models.get(&format!("{asset}/{checkpoint}s")) else {
            return json!({"status":"group_unavailable","sha256":self.sha256,"decision_role":"research_only"});
        };
        let Some(features) = signal_features(signal, end_ms) else {
            return json!({"status":"invalid_signal_reference_or_horizon","sha256":self.sha256,"decision_role":"research_only"});
        };
        let candidates = group
            .candidates
            .iter()
            .map(|(name, candidate)| {
                (
                    name.clone(),
                    candidate.predict(&features).unwrap_or_else(|| {
                        let reason = if candidate.kind == Kind::MarketOffsetLogistic
                            && features.market.is_none()
                        {
                            "missing_market_midpoint"
                        } else if candidate.kind != Kind::AssetGaussian && features.vector.is_none()
                        {
                            "missing_conditional_features"
                        } else {
                            "features_or_horizon_unsupported"
                        };
                        json!({"status":reason})
                    }),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let selected_probability = group
            .selected
            .as_ref()
            .and_then(|name| candidates.get(name)?.get("probability_up")?.as_f64());
        json!({"status":"observed","schema_version":1,"sha256":self.sha256,
            "decision_role":"research_only","online_trading_enabled":false,
            "selected":group.selected,"selected_probability_up":selected_probability,
            "effective_horizon_secs":features.horizon_secs,"reference_age_secs":features.reference_age_secs,
            "features":features.vector,"feature_names":FEATURES,"candidates":candidates})
    }
}

fn normal_cdf(value: f64) -> f64 {
    let x = value.abs() / 2.0_f64.sqrt();
    let t = 1.0 / (1.0 + 0.3275911 * x);
    let erf = 1.0
        - (((((1.061405429 * t - 1.453152027) * t) + 1.421413741) * t - 0.284496736) * t
            + 0.254829592)
            * t
            * (-x * x).exp();
    (0.5 * (1.0 + value.signum() * erf)).clamp(1e-6, 1.0 - 1e-6)
}

/// Offline check of the exact inference inputs exported by the Python study.
pub fn run_parity(model_path: &Path, sha: &str, fixture: &Path, report_path: &Path) -> Result<()> {
    let model = TerminalModel::load(model_path, sha)?;
    let mut cases = 0_u64;
    let mut forecasts = 0_u64;
    let mut max_error = 0.0_f64;
    for (index, line) in BufReader::new(fs::File::open(fixture)?).lines().enumerate() {
        let case: Value = serde_json::from_str(&line?)?;
        if case["model_sha256"] != model.sha256 {
            bail!("parity fixture model hash mismatch at line {}", index + 1);
        }
        let actual = model.audit(
            case["asset"].as_str().context("fixture asset missing")?,
            case["checkpoint"]
                .as_i64()
                .context("fixture checkpoint missing")?,
            &case["signal"],
            case["window_end_ms"]
                .as_i64()
                .context("fixture window end missing")?,
        );
        let expected = case["expected"]
            .as_object()
            .context("fixture forecasts missing")?;
        for (name, value) in expected {
            let a = actual["candidates"][name]["probability_up"].as_f64();
            match (a, value.as_f64()) {
                (Some(a), Some(b)) if a.is_finite() && b.is_finite() => {
                    let delta = (a - b).abs();
                    max_error = max_error.max(delta);
                    if delta > 1e-6 {
                        bail!("terminal inference parity failed at line {} candidate {name}: delta={delta}", index + 1);
                    }
                    forecasts += 1;
                }
                (None, None) if value.is_null() => {}
                _ => bail!(
                    "terminal inference availability mismatch at line {} candidate {name}",
                    index + 1
                ),
            }
        }
        cases += 1;
    }
    if cases == 0 || forecasts == 0 {
        bail!("empty parity evidence");
    }
    if let Some(parent) = report_path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let report = json!({"passed":true,"model_sha256":model.sha256,"cases":cases,
        "forecasts":forecasts,"max_probability_error":max_error,"tolerance":1e-6,
        "online_trading_enabled":false});
    fs::write(report_path, serde_json::to_string_pretty(&report)?)?;
    println!(
        "Terminal parity: {cases} cases, {forecasts} forecasts, max error={max_error} | {}",
        report_path.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signal() -> Value {
        json!({"sampled_at_ms":100000,"seconds_remaining":30.0,
            "current_twap":100.01,"opening_twap":100.0,
            "model_audit":{"reference_observed_at_ms":98000},
            "twap_anchors":{"anchors":[
                {"ago_ms":30000,"observed_at_ms":68000,"value":100.0},
                {"ago_ms":60000,"observed_at_ms":38000,"value":100.0}]},
            "spot_asof":{"streams":[{"topic":"crypto_prices_chainlink","sample":{
                "observed_at_ms":99500,"received_at_ms":99900,"value":100.02}}]}})
    }

    fn candidate() -> Candidate {
        serde_json::from_value(json!({"kind":"conditional_gaussian","horizon_min_secs":30,
            "horizon_max_secs":35,"scale_unit":"terminal_return_bps","reference_topic":"crypto_prices_twap_sixty",
            "means":[0,0,0,0,32],"scales":[1,1,1,1,1],"beta":[0.2,0.3,0.1,0.4,0.05,0.02],
            "sigma_floor_bps":0.3,"regime_sigma_bps":[2,3,2,3],"activity_cutoff_bps":1,"horizon_cutoff_secs":32,
            "standardized_residuals":[-2,-1,0,1,2]})).unwrap()
    }

    #[test]
    fn timestamp_and_features_match_python_fixture() {
        let f = signal_features(&signal(), 130000).unwrap();
        assert_eq!(f.horizon_secs, 32.0);
        assert_eq!(f.reference_age_secs, 2.0);
        let vector = f.vector.unwrap();
        assert!((vector[0] - 0.9999500033329732).abs() < 1e-9);
        assert!((vector[2] - 0.9998500233307523).abs() < 1e-9);
        let prediction = candidate().predict(&f).unwrap();
        // Python math.erf with these exact frozen features/coefs.
        let mu = 0.2 + 0.4 * vector[0] + 0.4 * vector[2] + 0.05 * 2.0;
        assert!((prediction["terminal_mean_bps"].as_f64().unwrap() - mu).abs() < 1e-12);
        assert!((prediction["probability_up"].as_f64().unwrap() - 0.7580228180249573).abs() < 1e-6);
    }

    #[test]
    fn future_spot_is_not_available_and_wrong_horizon_is_rejected() {
        let mut value = signal();
        value["spot_asof"]["streams"][0]["sample"]["received_at_ms"] = json!(100001);
        let f = signal_features(&value, 130000).unwrap();
        assert!(f.vector.is_none());
        assert!(candidate().predict(&f).is_none());
        assert!(signal_features(&signal(), 131000).is_none());
        let mut f = signal_features(&signal(), 130000).unwrap();
        f.horizon_secs = 60.0;
        assert!(candidate().predict(&f).is_none());
    }

    #[test]
    fn empirical_ties_count_up_and_tail_is_smoothed() {
        let mut c = candidate();
        c.kind = Kind::ConditionalEmpirical;
        c.beta = vec![0.0; 6];
        let mut f = signal_features(&signal(), 130000).unwrap();
        f.margin_bps = 0.0;
        assert!(
            (c.predict(&f).unwrap()["probability_up"].as_f64().unwrap() - 4.0 / 7.0).abs() < 1e-12
        );
        f.margin_bps = 1e6;
        assert!(c.predict(&f).unwrap()["probability_up"].as_f64().unwrap() < 1.0);
    }

    #[test]
    fn offset_is_market_anchored_bounded_and_requires_real_midpoint() {
        let mut c = candidate();
        let mut source = c.clone();
        source.kind = Kind::ConditionalEmpirical;
        c.kind = Kind::MarketOffsetLogistic;
        c.terminal_source = Some(Box::new(source));
        c.max_correction = Some(0.10);
        c.offset_feature_names = [
            "intercept",
            "terminal_minus_market",
            "signed_boundary_sigma",
            "absolute_boundary_sigma",
            "reference_age_fraction",
        ]
        .map(str::to_owned)
        .to_vec();
        c.beta = vec![0.0; 5];
        c.means = vec![0.0; 5];
        c.scales = vec![1.0; 5];
        c.validate().unwrap();
        let mut value = signal();
        value["baseline"] = json!({"source":"up_midpoint","probability_up":0.3});
        let f = signal_features(&value, 130000).unwrap();
        assert!((c.predict(&f).unwrap()["probability_up"].as_f64().unwrap() - 0.3).abs() < 1e-12);
        c.beta[0] = 100.0;
        assert!((c.predict(&f).unwrap()["probability_up"].as_f64().unwrap() - 0.4).abs() < 1e-12);
        value["baseline"]["source"] = json!("complementary_ask_interval");
        assert!(c
            .predict(&signal_features(&value, 130000).unwrap())
            .is_none());
        c.max_correction = Some(0.5);
        assert!(c.validate().is_err());
    }
}
