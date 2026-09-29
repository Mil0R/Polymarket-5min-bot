//! Read-only inference for the model selected by the offline probability study.
//! It is deliberately separate from the live decision model until a fresh
//! paired-book and execution study establishes an advantage.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

#[derive(Clone)]
pub struct ResearchModel {
    pub sha256: String,
    groups: BTreeMap<String, SelectedGroup>,
}

#[derive(Clone)]
struct SelectedGroup {
    name: String,
    candidate: Candidate,
}

#[derive(Debug, Clone, Copy)]
pub struct Forecast {
    pub probability_up: f64,
    pub selected_kind: &'static str,
}

#[derive(Deserialize)]
struct Artifact {
    schema_version: u64,
    models: BTreeMap<String, ArtifactGroup>,
}

#[derive(Deserialize)]
struct ArtifactGroup {
    status: String,
    selected: Option<String>,
    candidates: Option<BTreeMap<String, Candidate>>,
}

#[derive(Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Candidate {
    Current,
    Floor { scale: f64 },
    Empirical { residuals_bps: Vec<f64> },
}

#[derive(Debug, Clone, Copy)]
pub struct Features {
    pub opening: f64,
    pub current: f64,
    pub remaining_secs: f64,
    pub observed_volatility_bps: f64,
    pub minimum_volatility_bps: f64,
    pub current_probability_up: f64,
}

impl ResearchModel {
    pub fn load(path: &Path, expected_sha256: &str) -> Result<Self> {
        if expected_sha256.len() != 64 || !expected_sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("research model requires a 64-character SHA-256 digest");
        }
        let bytes = fs::read(path).with_context(|| format!("read model {}", path.display()))?;
        let sha256 = format!("{:x}", Sha256::digest(&bytes));
        if !sha256.eq_ignore_ascii_case(expected_sha256) {
            bail!("research model SHA-256 mismatch for {}", path.display());
        }
        let artifact: Artifact = serde_json::from_slice(&bytes)
            .with_context(|| format!("parse model {}", path.display()))?;
        if artifact.schema_version != 1 {
            bail!(
                "unsupported research model schema version {}",
                artifact.schema_version
            );
        }
        let mut groups = BTreeMap::new();
        for (key, group) in artifact.models {
            if group.status != "fitted" {
                continue;
            }
            let name = group
                .selected
                .context("fitted model has no selected candidate")?;
            let candidate = group
                .candidates
                .context("fitted model has no candidates")?
                .remove(&name)
                .with_context(|| format!("selected candidate {name} missing for {key}"))?;
            candidate.validate(&key)?;
            groups.insert(key, SelectedGroup { name, candidate });
        }
        if groups.is_empty() {
            bail!("research model has no fitted groups");
        }
        Ok(Self { sha256, groups })
    }

    pub fn has_group(&self, asset: &str, checkpoint_secs: i64) -> bool {
        self.groups
            .contains_key(&format!("{asset}/{checkpoint_secs}s"))
    }

    pub fn selected_name(&self, asset: &str, checkpoint_secs: i64) -> Option<&str> {
        self.groups
            .get(&format!("{asset}/{checkpoint_secs}s"))
            .map(|group| group.name.as_str())
    }

    pub fn forecast(&self, asset: &str, checkpoint_secs: i64, f: Features) -> Option<Forecast> {
        let group = self.groups.get(&format!("{asset}/{checkpoint_secs}s"))?;
        let probability_up = group.candidate.predict(f)?;
        Some(Forecast {
            probability_up,
            selected_kind: group.candidate.kind(),
        })
    }
}

impl Candidate {
    fn kind(&self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Floor { .. } => "floor",
            Self::Empirical { .. } => "empirical",
        }
    }

    fn validate(&self, group: &str) -> Result<()> {
        match self {
            Self::Current => Ok(()),
            Self::Floor { scale } if scale.is_finite() && *scale > 0.0 => Ok(()),
            Self::Empirical { residuals_bps }
                if !residuals_bps.is_empty()
                    && residuals_bps.iter().all(|r| r.is_finite())
                    && residuals_bps.windows(2).all(|pair| pair[0] <= pair[1]) =>
            {
                Ok(())
            }
            _ => bail!("invalid selected research candidate for {group}"),
        }
    }

    fn predict(&self, f: Features) -> Option<f64> {
        if !(f.opening.is_finite()
            && f.current.is_finite()
            && f.remaining_secs.is_finite()
            && f.observed_volatility_bps.is_finite()
            && f.minimum_volatility_bps.is_finite()
            && f.current_probability_up.is_finite())
            || f.opening <= 0.0
            || f.current <= 0.0
            || f.remaining_secs <= 0.0
            || f.observed_volatility_bps < 0.0
            || f.minimum_volatility_bps <= 0.0
            || !(0.0..=1.0).contains(&f.current_probability_up)
        {
            return None;
        }
        let p = match self {
            Self::Current => f.current_probability_up,
            Self::Floor { scale } => {
                // The study's directional_margin_bps is the arithmetic price
                // distance, not a log return (the empirical threshold is log).
                let margin = (f.current / f.opening - 1.0) * 10_000.0;
                let sigma = f
                    .observed_volatility_bps
                    .max(f.minimum_volatility_bps * scale)
                    * f.remaining_secs.sqrt();
                normal_cdf(margin / sigma.max(1e-6))
            }
            Self::Empirical { residuals_bps } => {
                let threshold = (f.opening / f.current).ln() * 10_000.0;
                let first_matching = residuals_bps.partition_point(|r| *r < threshold);
                (1 + residuals_bps.len() - first_matching) as f64 / (residuals_bps.len() + 2) as f64
            }
        };
        Some(p.clamp(1e-6, 1.0 - 1e-6))
    }
}

// Same normal-CDF approximation used by the live Gaussian model. The offline
// study uses math.erf; parity tests allow only the approximation's error.
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

#[cfg(test)]
mod tests {
    use super::{Candidate, Features, ResearchModel};
    use sha2::{Digest, Sha256};

    fn features() -> Features {
        Features {
            opening: 100.0,
            current: 100.01,
            remaining_secs: 30.0,
            observed_volatility_bps: 0.1,
            minimum_volatility_bps: 0.8,
            current_probability_up: 0.6,
        }
    }

    #[test]
    fn empirical_prediction_matches_add_one_study_rule() {
        let p = Candidate::Empirical {
            residuals_bps: vec![-2.0, 0.0, 2.0],
        }
        .predict(features())
        .unwrap();
        assert!((p - 0.6).abs() < 1e-12);
    }

    #[test]
    fn floor_prediction_matches_gaussian_rule() {
        let p = Candidate::Floor { scale: 0.5 }.predict(features()).unwrap();
        let expected = 0.6759615659304088;
        assert!((p - expected).abs() < 1e-5);
    }

    #[test]
    fn invalid_features_fail_closed() {
        let mut f = features();
        f.current = f64::NAN;
        assert!(Candidate::Current.predict(f).is_none());
    }

    #[test]
    fn frozen_artifact_identity_is_verified_before_forecast() {
        let path = std::env::temp_dir().join(format!(
            "sddfv-research-model-{}.json",
            uuid::Uuid::new_v4()
        ));
        let bytes = br#"{"schema_version":1,"models":{"btc/30s":{"status":"fitted","selected":"empirical","candidates":{"empirical":{"kind":"empirical","residuals_bps":[-2,0,2]}}}}}"#;
        std::fs::write(&path, bytes).unwrap();
        let sha = format!("{:x}", Sha256::digest(bytes));
        assert!(ResearchModel::load(&path, &"0".repeat(64)).is_err());
        let model = ResearchModel::load(&path, &sha).unwrap();
        assert!(model.has_group("btc", 30));
        assert_eq!(model.selected_name("btc", 30), Some("empirical"));
        assert!(
            (model
                .forecast("btc", 30, features())
                .unwrap()
                .probability_up
                - 0.6)
                .abs()
                < 1e-12
        );
        std::fs::remove_file(path).unwrap();
    }
}
