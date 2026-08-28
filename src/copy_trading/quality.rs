use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WalletQualitySnapshot {
    pub buy_signals: u64,
    pub fresh_buy_signals: u64,
    pub actionable_buy_signals: u64,
    pub market_evaluations: u64,
    pub market_passes: u64,
    pub proposals: u64,
    pub shadow_attempts: u64,
    pub shadow_executable: u64,
}

impl WalletQualitySnapshot {
    fn ratio(numerator: u64, denominator: u64) -> f64 {
        if denominator == 0 {
            0.0
        } else {
            numerator as f64 / denominator as f64
        }
    }

    pub fn fresh_ratio(&self) -> f64 {
        Self::ratio(self.fresh_buy_signals, self.buy_signals)
    }

    pub fn actionable_ratio(&self) -> f64 {
        Self::ratio(self.actionable_buy_signals, self.buy_signals)
    }

    pub fn market_pass_ratio(&self) -> f64 {
        Self::ratio(self.market_passes, self.market_evaluations)
    }

    pub fn shadow_executable_ratio(&self) -> f64 {
        Self::ratio(self.shadow_executable, self.shadow_attempts)
    }
}

#[derive(Debug, Clone)]
pub struct WalletQualityPolicy {
    pub min_buy_samples: u64,
    pub min_market_samples: u64,
    pub min_shadow_samples: u64,
    pub min_fresh_ratio: f64,
    pub min_actionable_ratio: f64,
    pub min_market_pass_ratio: f64,
    pub min_shadow_executable_ratio: f64,
}

impl WalletQualityPolicy {
    pub fn from_env() -> Self {
        Self {
            min_buy_samples: env_u64("COPY_ROTATION_MIN_BUY_SAMPLES", 50),
            min_market_samples: env_u64("COPY_ROTATION_MIN_MARKET_SAMPLES", 10),
            min_shadow_samples: env_u64("COPY_ROTATION_MIN_SHADOW_SAMPLES", 10),
            min_fresh_ratio: env_ratio("COPY_ROTATION_MIN_FRESH_RATIO", 0.25),
            min_actionable_ratio: env_ratio("COPY_ROTATION_MIN_ACTIONABLE_RATIO", 0.05),
            min_market_pass_ratio: env_ratio("COPY_ROTATION_MIN_MARKET_PASS_RATIO", 0.15),
            min_shadow_executable_ratio: env_ratio(
                "COPY_ROTATION_MIN_SHADOW_EXECUTABLE_RATIO",
                0.15,
            ),
        }
    }

    pub fn rejection_reasons(&self, sample: &WalletQualitySnapshot) -> Vec<String> {
        if sample.buy_signals < self.min_buy_samples {
            return Vec::new();
        }
        let mut reasons = Vec::new();
        if sample.fresh_ratio() < self.min_fresh_ratio {
            reasons.push(format!(
                "fresh signal ratio {:.3} < {:.3}",
                sample.fresh_ratio(),
                self.min_fresh_ratio
            ));
        }
        if sample.actionable_ratio() < self.min_actionable_ratio {
            reasons.push(format!(
                "actionable signal ratio {:.3} < {:.3}",
                sample.actionable_ratio(),
                self.min_actionable_ratio
            ));
        }
        if sample.market_evaluations >= self.min_market_samples
            && sample.market_pass_ratio() < self.min_market_pass_ratio
        {
            reasons.push(format!(
                "market pass ratio {:.3} < {:.3}",
                sample.market_pass_ratio(),
                self.min_market_pass_ratio
            ));
        }
        if sample.shadow_attempts >= self.min_shadow_samples
            && sample.shadow_executable_ratio() < self.min_shadow_executable_ratio
        {
            reasons.push(format!(
                "shadow executable ratio {:.3} < {:.3}",
                sample.shadow_executable_ratio(),
                self.min_shadow_executable_ratio
            ));
        }
        reasons
    }
}

#[derive(Debug, Clone, Default)]
pub struct WalletQualityTracker {
    samples: Arc<DashMap<String, WalletQualitySnapshot>>,
    state_path: Arc<Option<PathBuf>>,
}

impl WalletQualityTracker {
    pub fn new_with_state(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let samples = std::fs::read(&path)
            .ok()
            .and_then(|raw| {
                serde_json::from_slice::<HashMap<String, WalletQualitySnapshot>>(&raw).ok()
            })
            .unwrap_or_default()
            .into_iter()
            .collect::<DashMap<_, _>>();
        Self {
            samples: Arc::new(samples),
            state_path: Arc::new(Some(path)),
        }
    }

    fn update(&self, address: &str, update: impl FnOnce(&mut WalletQualitySnapshot)) {
        let mut sample = self
            .samples
            .entry(address.trim().to_ascii_lowercase())
            .or_default();
        update(&mut sample);
    }

    pub fn record_buy(&self, address: &str, fresh: bool) {
        self.update(address, |sample| {
            sample.buy_signals += 1;
            sample.fresh_buy_signals += u64::from(fresh);
        });
    }

    pub fn record_actionable(&self, address: &str) {
        self.update(address, |sample| sample.actionable_buy_signals += 1);
    }

    pub fn record_market(&self, address: &str, passed: bool) {
        self.update(address, |sample| {
            sample.market_evaluations += 1;
            sample.market_passes += u64::from(passed);
        });
    }

    pub fn record_proposal(&self, address: &str) {
        self.update(address, |sample| sample.proposals += 1);
    }

    pub fn record_shadow(&self, address: &str, executable: bool) {
        self.update(address, |sample| {
            sample.shadow_attempts += 1;
            sample.shadow_executable += u64::from(executable);
        });
    }

    pub fn snapshots(&self) -> Vec<(String, WalletQualitySnapshot)> {
        let mut rows = self
            .samples
            .iter()
            .map(|entry| (entry.key().clone(), entry.value().clone()))
            .collect::<Vec<_>>();
        rows.sort_by(|left, right| left.0.cmp(&right.0));
        rows
    }

    pub fn save(&self) -> std::io::Result<()> {
        let Some(path) = self.state_path.as_ref() else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let snapshot = self
            .samples
            .iter()
            .map(|entry| (entry.key().clone(), entry.value().clone()))
            .collect::<HashMap<_, _>>();
        let encoded = serde_json::to_vec_pretty(&snapshot).map_err(std::io::Error::other)?;
        let temporary = path.with_extension("json.tmp");
        std::fs::write(&temporary, encoded)?;
        std::fs::rename(temporary, path)
    }
}

fn env_u64(name: &str, default: u64) -> u64 {
    env::var(name)
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(default)
        .max(1)
}

fn env_ratio(name: &str, default: f64) -> f64 {
    env::var(name)
        .ok()
        .and_then(|value| value.trim().parse::<f64>().ok())
        .filter(|value| (0.0..=1.0).contains(value))
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_waits_for_samples_then_rejects_structural_staleness() {
        let policy = WalletQualityPolicy {
            min_buy_samples: 10,
            min_market_samples: 2,
            min_shadow_samples: 2,
            min_fresh_ratio: 0.25,
            min_actionable_ratio: 0.05,
            min_market_pass_ratio: 0.15,
            min_shadow_executable_ratio: 0.15,
        };
        let small = WalletQualitySnapshot {
            buy_signals: 9,
            ..Default::default()
        };
        assert!(policy.rejection_reasons(&small).is_empty());

        let poor = WalletQualitySnapshot {
            buy_signals: 20,
            fresh_buy_signals: 2,
            actionable_buy_signals: 0,
            market_evaluations: 2,
            market_passes: 0,
            shadow_attempts: 2,
            shadow_executable: 0,
            ..Default::default()
        };
        assert_eq!(policy.rejection_reasons(&poor).len(), 4);
    }

    #[test]
    fn tracker_aggregates_case_insensitive_wallets() {
        let tracker = WalletQualityTracker::default();
        tracker.record_buy("0xABC", true);
        tracker.record_buy("0xabc", false);
        tracker.record_actionable("0xAbC");
        let rows = tracker.snapshots();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1.buy_signals, 2);
        assert_eq!(rows[0].1.actionable_buy_signals, 1);
    }

    #[test]
    fn quality_samples_survive_restart() {
        let path = std::env::temp_dir().join(format!("copy-quality-{}.json", uuid::Uuid::new_v4()));
        let tracker = WalletQualityTracker::new_with_state(&path);
        tracker.record_buy("0xabc", true);
        tracker.record_shadow("0xabc", true);
        tracker.save().unwrap();
        let restored = WalletQualityTracker::new_with_state(&path);
        let rows = restored.snapshots();
        assert_eq!(rows[0].1.buy_signals, 1);
        assert_eq!(rows[0].1.shadow_executable, 1);
        std::fs::remove_file(path).unwrap();
    }
}
