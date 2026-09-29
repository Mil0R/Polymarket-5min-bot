//! Independent Late-Resolution Winner Trading model based on Polymarket's official Chainlink TWAP feed.

use chrono::Utc;
use futures::{SinkExt, StreamExt};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, VecDeque};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, trace, warn};

use crate::runtime_metrics::RuntimeMetrics;

const HISTORY_MILLIS: i64 = 10 * 60 * 1_000;
// RTDS publisher timestamps have been observed a few seconds ahead of the
// local host clock. Samples are already known when received, so tolerate a
// small bounded publisher-clock skew while continuing to reject implausibly
// future-dated data.
const MAX_FUTURE_CLOCK_SKEW_MS: i64 = 10_000;
const RESEARCH_TWAP_SILENCE_LIMIT: Duration = Duration::from_secs(12);
const NORMAL_TWAP_SILENCE_LIMIT: Duration = Duration::from_secs(30);
const RTDS_FRAME_SILENCE_LIMIT: Duration = Duration::from_secs(30);
const RESEARCH_FEED_LOG_TARGET: &str = "polypulse::strategy::short_duration_digital_fair_value";

macro_rules! warn_price_feed {
    ($research:expr, $($fields:tt)*) => {{
        if $research {
            warn!(target: RESEARCH_FEED_LOG_TARGET, $($fields)*);
        } else {
            warn!($($fields)*);
        }
    }};
}

fn effective_sample_age_ms(now_ms: i64, observed_at_ms: i64) -> Option<i64> {
    let age_ms = now_ms.checked_sub(observed_at_ms)?;
    if age_ms < -MAX_FUTURE_CLOCK_SKEW_MS {
        None
    } else {
        Some(age_ms.max(0))
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TwapSample {
    pub observed_at_ms: i64,
    pub value: Decimal,
}

/// Public RTDS reference-price update. A spot update is a research feature,
/// never a substitute for the market's Chainlink TWAP settlement feed.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RtdsReferenceSample {
    pub topic: String,
    pub symbol: String,
    pub asset: String,
    pub observed_at_ms: i64,
    pub publisher_at_ms: Option<i64>,
    pub received_at_ms: i64,
    pub value: Decimal,
}

pub type PriceFeedObserver = Arc<dyn Fn(RtdsReferenceSample) + Send + Sync>;

#[derive(Default)]
pub struct ChainlinkTwapStore {
    samples: RwLock<HashMap<String, VecDeque<TwapSample>>>,
    /// Latest transport timestamps for the settlement TWAP.  Keep these
    /// separate from event-time price history so model math cannot
    /// accidentally substitute receive time for the settlement timestamp.
    twap_reference_timing: RwLock<HashMap<String, RtdsReferenceSample>>,
    research_spot_samples: RwLock<HashMap<String, VecDeque<RtdsReferenceSample>>>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FairValueTwapSnapshot {
    pub opening_observed_at_ms: i64,
    pub observed_volatility_bps: f64,
    pub minimum_volatility_bps: f64,
    pub observation_count: usize,
    pub opening: Decimal,
    pub current: Decimal,
    pub observed_at_ms: i64,
    pub publisher_at_ms: Option<i64>,
    pub received_at_ms: Option<i64>,
    pub volatility_bps_per_sqrt_second: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FairValueSnapshotReadiness {
    Ready(FairValueTwapSnapshot),
    NoPriceHistory,
    StaleFeed,
    NoOpeningReference,
    InsufficientObservations,
}

impl FairValueSnapshotReadiness {
    pub fn reason(self) -> Option<&'static str> {
        match self {
            Self::Ready(_) => None,
            Self::NoPriceHistory => Some("no_price_history"),
            Self::StaleFeed => Some("stale_feed"),
            Self::NoOpeningReference => Some("no_opening_reference"),
            Self::InsufficientObservations => Some("insufficient_observations"),
        }
    }
}

impl ChainlinkTwapStore {
    pub fn ingest(&self, symbol: &str, sample: TwapSample) {
        if sample.value <= dec!(0) || sample.observed_at_ms <= 0 {
            return;
        }
        let asset = normalize_asset(symbol);
        let mut all = self.samples.write().expect("TWAP store lock poisoned");
        let samples = all.entry(asset).or_default();
        if let Some(last) = samples.back_mut() {
            if sample.observed_at_ms < last.observed_at_ms {
                return;
            }
            if sample.observed_at_ms == last.observed_at_ms {
                *last = sample;
                return;
            }
        }
        samples.push_back(sample);
        let cutoff = sample.observed_at_ms - HISTORY_MILLIS;
        while samples
            .front()
            .is_some_and(|oldest| oldest.observed_at_ms < cutoff)
        {
            samples.pop_front();
        }
    }

    fn ingest_research_spot(&self, sample: RtdsReferenceSample) {
        let key = format!("{}:{}", sample.topic, sample.asset);
        let Ok(mut all) = self.research_spot_samples.write() else {
            return;
        };
        let samples = all.entry(key).or_default();
        if samples
            .back()
            .is_some_and(|last| sample.received_at_ms < last.received_at_ms)
        {
            return;
        }
        let received_at_ms = sample.received_at_ms;
        samples.push_back(sample);
        let cutoff = received_at_ms.saturating_sub(HISTORY_MILLIS);
        while samples
            .front()
            .is_some_and(|oldest| oldest.received_at_ms < cutoff)
        {
            samples.pop_front();
        }
    }

    pub fn ingest_twap_reference_timing(&self, sample: RtdsReferenceSample) {
        let Ok(mut all) = self.twap_reference_timing.write() else {
            return;
        };
        let replace = all.get(&sample.asset).is_none_or(|current| {
            sample.observed_at_ms > current.observed_at_ms
                || (sample.observed_at_ms == current.observed_at_ms
                    && sample.received_at_ms >= current.received_at_ms)
        });
        if replace {
            all.insert(sample.asset.clone(), sample);
        }
    }

    /// Only prices received locally before `now_ms` are eligible for a
    /// research snapshot. This prevents as-of joins from using future data.
    pub fn fair_value_recent_spot_samples(
        &self,
        topic: &str,
        asset: &str,
        now_ms: i64,
        lookback_ms: i64,
    ) -> Vec<RtdsReferenceSample> {
        let key = format!("{}:{}", topic, normalize_asset(asset));
        let Ok(all) = self.research_spot_samples.read() else {
            return Vec::new();
        };
        all.get(&key)
            .into_iter()
            .flat_map(|samples| samples.iter())
            .filter(|sample| {
                sample.received_at_ms >= now_ms.saturating_sub(lookback_ms)
                    && sample.received_at_ms <= now_ms
                    && sample.observed_at_ms <= now_ms
            })
            .cloned()
            .collect()
    }

    /// Read-only state for independent fair-value research. The opening sample
    /// must be close to the window boundary and every volatility observation is
    /// known by `now_ms`; callers cannot obtain future samples through this API.
    #[allow(dead_code)]
    pub fn fair_value_snapshot(
        &self,
        asset: &str,
        window_start_ms: i64,
        now_ms: i64,
        opening_max_skew_ms: i64,
        max_feed_age_ms: i64,
        volatility_lookback_secs: u64,
        min_observations: usize,
        min_volatility_bps_per_sqrt_second: f64,
    ) -> Option<FairValueTwapSnapshot> {
        match self.fair_value_snapshot_readiness(
            asset,
            window_start_ms,
            now_ms,
            opening_max_skew_ms,
            max_feed_age_ms,
            volatility_lookback_secs,
            min_observations,
            min_volatility_bps_per_sqrt_second,
        ) {
            FairValueSnapshotReadiness::Ready(snapshot) => Some(snapshot),
            _ => None,
        }
    }

    pub fn fair_value_snapshot_readiness(
        &self,
        asset: &str,
        window_start_ms: i64,
        now_ms: i64,
        opening_max_skew_ms: i64,
        max_feed_age_ms: i64,
        volatility_lookback_secs: u64,
        min_observations: usize,
        min_volatility_bps_per_sqrt_second: f64,
    ) -> FairValueSnapshotReadiness {
        let asset = normalize_asset(asset);
        let Ok(all) = self.samples.read() else {
            return FairValueSnapshotReadiness::NoPriceHistory;
        };
        let Some(samples) = all.get(&asset) else {
            return FairValueSnapshotReadiness::NoPriceHistory;
        };
        let Some(current) = samples.back().copied() else {
            return FairValueSnapshotReadiness::NoPriceHistory;
        };
        let Some(age_ms) = effective_sample_age_ms(now_ms, current.observed_at_ms) else {
            return FairValueSnapshotReadiness::StaleFeed;
        };
        if age_ms > max_feed_age_ms {
            return FairValueSnapshotReadiness::StaleFeed;
        }
        let Some(opening) = samples
            .iter()
            .filter(|sample| (sample.observed_at_ms - window_start_ms).abs() <= opening_max_skew_ms)
            .min_by_key(|sample| (sample.observed_at_ms - window_start_ms).abs())
            .copied()
        else {
            return FairValueSnapshotReadiness::NoOpeningReference;
        };
        let Ok(lookback_ms) = i64::try_from(volatility_lookback_secs) else {
            return FairValueSnapshotReadiness::InsufficientObservations;
        };
        let cutoff = now_ms - lookback_ms * 1_000;
        let history = samples
            .iter()
            .copied()
            .filter(|sample| sample.observed_at_ms >= cutoff)
            .collect::<Vec<_>>();
        if history.len() < min_observations {
            return FairValueSnapshotReadiness::InsufficientObservations;
        }
        let timing = self
            .twap_reference_timing
            .read()
            .ok()
            .and_then(|all| all.get(&asset).cloned())
            .filter(|sample| sample.observed_at_ms == current.observed_at_ms);
        FairValueSnapshotReadiness::Ready(FairValueTwapSnapshot {
            opening_observed_at_ms: opening.observed_at_ms,
            observed_volatility_bps: volatility_bps_per_sqrt_second(&history),
            minimum_volatility_bps: min_volatility_bps_per_sqrt_second,
            observation_count: history.len(),
            opening: opening.value,
            current: current.value,
            observed_at_ms: current.observed_at_ms,
            publisher_at_ms: timing.as_ref().and_then(|sample| sample.publisher_at_ms),
            received_at_ms: timing.as_ref().map(|sample| sample.received_at_ms),
            volatility_bps_per_sqrt_second: volatility_bps_per_sqrt_second(&history)
                .max(min_volatility_bps_per_sqrt_second),
        })
    }

    pub fn latest_observed_at_ms(&self, asset: &str) -> Option<i64> {
        let asset = normalize_asset(asset);
        self.samples
            .read()
            .ok()?
            .get(&asset)?
            .back()
            .map(|sample| sample.observed_at_ms)
    }

    /// Observations already available at `now_ms`, in event-time order. The
    /// fair-value research journal uses this to preserve the rolling TWAP
    /// path without making any assumption about the stream's weighting rule.
    pub fn fair_value_recent_twap_samples(
        &self,
        asset: &str,
        now_ms: i64,
        lookback_ms: i64,
    ) -> Vec<TwapSample> {
        let asset = normalize_asset(asset);
        let Ok(all) = self.samples.read() else {
            return Vec::new();
        };
        all.get(&asset)
            .into_iter()
            .flat_map(|samples| samples.iter().copied())
            .filter(|sample| {
                sample.observed_at_ms >= now_ms.saturating_sub(lookback_ms)
                    && sample.observed_at_ms <= now_ms
            })
            .collect()
    }

    pub fn evaluate(&self, input: WinnerModelInput, policy: WinnerModelPolicy) -> ModelEvaluation {
        let asset = normalize_asset(input.asset);
        let all = self.samples.read().expect("TWAP store lock poisoned");
        let Some(samples) = all.get(&asset) else {
            return ModelEvaluation::rejected(ModelRejection::NoPriceHistory);
        };
        let Some(current) = samples.back().copied() else {
            return ModelEvaluation::rejected(ModelRejection::NoPriceHistory);
        };
        let Some(age_ms) = effective_sample_age_ms(input.now_ms, current.observed_at_ms) else {
            return ModelEvaluation::rejected(ModelRejection::StalePrice);
        };
        if age_ms > policy.max_feed_age_ms {
            return ModelEvaluation::rejected(ModelRejection::StalePrice);
        }
        let Some(open) = samples
            .iter()
            .filter(|sample| {
                (sample.observed_at_ms - input.window_start_ms).abs() <= policy.max_opening_skew_ms
            })
            .min_by_key(|sample| (sample.observed_at_ms - input.window_start_ms).abs())
            .copied()
        else {
            return ModelEvaluation::rejected(ModelRejection::NoOpeningReference);
        };
        let spread = input.ask_price - input.bid_price;
        if input.bid_price <= dec!(0) || spread < dec!(0) {
            return ModelEvaluation::rejected(ModelRejection::SpreadTooWide);
        }
        let depth_notional = input.ask_price * input.ask_size;
        let mut failed_checks = Vec::new();
        if spread > policy.max_spread {
            failed_checks.push(ModelRejection::SpreadTooWide);
        }
        if depth_notional < policy.min_depth_usdc {
            failed_checks.push(ModelRejection::InsufficientDepth);
        }

        let cutoff = input.now_ms - policy.volatility_lookback_secs as i64 * 1_000;
        let history: Vec<_> = samples
            .iter()
            .copied()
            .filter(|sample| sample.observed_at_ms >= cutoff)
            .collect();
        if history.len() < policy.min_observations {
            return ModelEvaluation::rejected(ModelRejection::InsufficientObservations);
        }
        let observed_volatility = volatility_bps_per_sqrt_second(&history);
        let volatility = observed_volatility.max(policy.min_volatility_bps_per_sqrt_second);
        let remaining = input.seconds_remaining.max(1) as f64;
        let remaining_sigma_bps = volatility * remaining.sqrt();
        let open_value = open.value.to_f64().unwrap_or(0.0);
        let current_value = current.value.to_f64().unwrap_or(0.0);
        if open_value <= 0.0 || current_value <= 0.0 {
            return ModelEvaluation::rejected(ModelRejection::InvalidPrice);
        }
        let raw_margin_bps = (current_value / open_value - 1.0) * 10_000.0;
        let directional_margin_bps = if input.buy_yes {
            raw_margin_bps
        } else {
            -raw_margin_bps
        };
        let reversal_boundary_bps =
            policy.min_margin_bps + policy.reversal_sigma_multiplier * remaining_sigma_bps;
        let z = directional_margin_bps / remaining_sigma_bps.max(0.000_001);
        // An outcome moving against the reference must be allowed below 50%.
        let probability =
            Decimal::try_from(normal_cdf(z).clamp(0.0001, 0.9999)).unwrap_or(dec!(0.5));
        let effective_price = (input.ask_price + policy.fee_buffer).min(dec!(1));
        let probability_edge_before_risk = probability - effective_price;
        let required_probability = effective_price + policy.model_risk_buffer + policy.min_net_ev;
        let net_ev = probability - effective_price - policy.model_risk_buffer;
        let mut evaluation = ModelEvaluation {
            audit: Some(WinnerModelAudit {
                version: 1,
                opening_observed_at_ms: open.observed_at_ms,
                opening_skew_ms: open.observed_at_ms - input.window_start_ms,
                current_age_ms: (input.now_ms - current.observed_at_ms).max(0),
                observation_count: history.len(),
                observed_volatility_bps: observed_volatility,
                minimum_volatility_bps: policy.min_volatility_bps_per_sqrt_second,
                volatility_floor_active: observed_volatility
                    < policy.min_volatility_bps_per_sqrt_second,
                probability_without_volatility_floor: normal_cdf(
                    directional_margin_bps
                        / (observed_volatility * remaining.sqrt()).max(0.000_001),
                ),
                reference_topic: "crypto_prices_twap_sixty".into(),
                horizon_volatility: [10, 20, 40, 60]
                    .into_iter()
                    .map(|seconds| horizon_volatility(&history, seconds))
                    .collect(),
            }),
            rejection: failed_checks.first().copied(),
            failed_checks,
            opening_twap: Some(open.value),
            current_twap: Some(current.value),
            observed_at_ms: Some(current.observed_at_ms),
            directional_margin_bps,
            reversal_boundary_bps,
            volatility_bps_per_sqrt_second: volatility,
            probability,
            effective_price,
            probability_edge_before_risk,
            required_probability,
            net_ev,
            spread,
            depth_notional,
        };
        if directional_margin_bps <= reversal_boundary_bps {
            evaluation
                .failed_checks
                .push(ModelRejection::ReversalBoundary);
        }
        if net_ev < policy.min_net_ev {
            evaluation.failed_checks.push(ModelRejection::NetEvTooLow);
        }
        evaluation.rejection = evaluation.failed_checks.first().copied();
        evaluation
    }
}

#[derive(Debug, Clone, Copy)]
pub struct WinnerModelInput<'a> {
    pub asset: &'a str,
    pub window_start_ms: i64,
    pub now_ms: i64,
    pub seconds_remaining: i64,
    pub buy_yes: bool,
    pub bid_price: Decimal,
    pub ask_price: Decimal,
    pub ask_size: Decimal,
}

#[derive(Debug, Clone, Copy)]
pub struct WinnerModelPolicy {
    pub max_feed_age_ms: i64,
    pub max_opening_skew_ms: i64,
    pub volatility_lookback_secs: u64,
    pub min_observations: usize,
    pub min_volatility_bps_per_sqrt_second: f64,
    pub min_margin_bps: f64,
    pub reversal_sigma_multiplier: f64,
    pub max_spread: Decimal,
    pub min_depth_usdc: Decimal,
    pub fee_buffer: Decimal,
    pub model_risk_buffer: Decimal,
    pub min_net_ev: Decimal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelRejection {
    NoPriceHistory,
    StalePrice,
    NoOpeningReference,
    InvalidPrice,
    InsufficientObservations,
    SpreadTooWide,
    InsufficientDepth,
    ReversalBoundary,
    NetEvTooLow,
}

/// Diagnostics only: the unfloored probability is never used for admission.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WinnerModelAudit {
    pub version: u32,
    pub opening_observed_at_ms: i64,
    pub opening_skew_ms: i64,
    pub current_age_ms: i64,
    pub observation_count: usize,
    pub observed_volatility_bps: f64,
    pub minimum_volatility_bps: f64,
    pub volatility_floor_active: bool,
    pub probability_without_volatility_floor: f64,
    #[serde(default)]
    pub reference_topic: String,
    /// Historical non-overlapping increments; diagnostics, not calibrated settlement variance.
    #[serde(default)]
    pub horizon_volatility: Vec<HorizonVolatility>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HorizonVolatility {
    pub seconds: u64,
    pub pairs: usize,
    pub bps_per_sqrt_second: Option<f64>,
}

fn horizon_volatility(samples: &[TwapSample], seconds: u64) -> HorizonVolatility {
    let mut sum = 0.0;
    let mut pairs = 0;
    let mut start = samples.first();
    for sample in samples.iter().skip(1) {
        let Some(previous) = start else { break };
        let dt = (sample.observed_at_ms - previous.observed_at_ms) as f64 / 1000.0;
        if dt < seconds as f64 {
            continue;
        }
        if dt <= seconds as f64 + 3.0 {
            if let (Some(a), Some(b)) = (previous.value.to_f64(), sample.value.to_f64()) {
                if a > 0.0 && b > 0.0 {
                    sum += ((b / a - 1.0) * 10000.0).powi(2) / dt;
                    pairs += 1;
                }
            }
        }
        start = Some(sample);
    }
    HorizonVolatility {
        seconds,
        pairs,
        bps_per_sqrt_second: (pairs > 0).then(|| (sum / pairs as f64).sqrt()),
    }
}

/// Reprice a recorded observation without re-reading the feed. Research only.
pub fn sensitivity_decision(
    model: &ModelEvaluation,
    policy: WinnerModelPolicy,
    seconds: i64,
    floor_scale: f64,
    risk_scale: f64,
) -> (bool, f64, f64) {
    let Some(audit) = &model.audit else {
        return (false, 0.0, 0.0);
    };
    let volatility = audit
        .observed_volatility_bps
        .max(audit.minimum_volatility_bps * floor_scale);
    let sigma = volatility * (seconds.max(1) as f64).sqrt();
    let probability =
        normal_cdf(model.directional_margin_bps / sigma.max(0.000001)).clamp(0.0001, 0.9999);
    let risk = policy.model_risk_buffer.to_f64().unwrap_or(1.0) * risk_scale;
    let ev = probability - model.effective_price.to_f64().unwrap_or(1.0) - risk;
    let market_ok = model.failed_checks.iter().all(|r| {
        matches!(
            r,
            ModelRejection::ReversalBoundary | ModelRejection::NetEvTooLow
        )
    });
    (
        market_ok
            && model.directional_margin_bps
                > policy.min_margin_bps + policy.reversal_sigma_multiplier * sigma
            && ev >= policy.min_net_ev.to_f64().unwrap_or(1.0),
        probability,
        ev,
    )
}

#[derive(Debug, Clone, PartialEq)]
pub struct ModelEvaluation {
    pub audit: Option<WinnerModelAudit>,
    pub rejection: Option<ModelRejection>,
    /// All failed checks in policy order. `rejection` remains the primary
    /// reason for backwards-compatible admission and aggregate counters.
    pub failed_checks: Vec<ModelRejection>,
    pub opening_twap: Option<Decimal>,
    pub current_twap: Option<Decimal>,
    pub observed_at_ms: Option<i64>,
    pub directional_margin_bps: f64,
    pub reversal_boundary_bps: f64,
    pub volatility_bps_per_sqrt_second: f64,
    pub probability: Decimal,
    pub effective_price: Decimal,
    pub probability_edge_before_risk: Decimal,
    pub required_probability: Decimal,
    pub net_ev: Decimal,
    pub spread: Decimal,
    pub depth_notional: Decimal,
}

impl ModelEvaluation {
    fn rejected(rejection: ModelRejection) -> Self {
        Self {
            audit: None,
            rejection: Some(rejection),
            failed_checks: vec![rejection],
            opening_twap: None,
            current_twap: None,
            observed_at_ms: None,
            directional_margin_bps: 0.0,
            reversal_boundary_bps: 0.0,
            volatility_bps_per_sqrt_second: 0.0,
            probability: dec!(0),
            effective_price: dec!(0),
            probability_edge_before_risk: dec!(0),
            required_probability: dec!(0),
            net_ev: dec!(0),
            spread: dec!(0),
            depth_notional: dec!(0),
        }
    }

    pub fn admitted(&self) -> bool {
        self.rejection.is_none()
    }
}

#[derive(Default)]
pub struct WinnerConfirmationTracker {
    states: HashMap<String, ConfirmationState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WinnerConfirmationProgress {
    pub confirmations: u32,
    pub required: u32,
    pub confirmed: bool,
}

pub fn attempt_allowed(
    total_attempts: u32,
    market_attempts: u32,
    max_per_window: u32,
    max_per_market: u32,
) -> bool {
    (max_per_window == 0 || total_attempts < max_per_window) && market_attempts < max_per_market
}

struct ConfirmationState {
    buy_yes: bool,
    last_observed_at_ms: i64,
    confirmations: u32,
}

impl WinnerConfirmationTracker {
    pub fn clear(&mut self) {
        self.states.clear();
    }

    pub fn reject(&mut self, slug: &str) {
        self.states.remove(slug);
    }

    pub fn observe(
        &mut self,
        slug: &str,
        buy_yes: bool,
        observed_at_ms: i64,
        required: u32,
        ttl_ms: i64,
    ) -> WinnerConfirmationProgress {
        let required = required.max(1);
        let state = match self.states.entry(slug.to_string()) {
            Entry::Vacant(entry) => entry.insert(ConfirmationState {
                buy_yes,
                last_observed_at_ms: observed_at_ms,
                confirmations: 1,
            }),
            Entry::Occupied(entry) => entry.into_mut(),
        };
        if state.buy_yes != buy_yes
            || observed_at_ms.saturating_sub(state.last_observed_at_ms) > ttl_ms.max(1)
        {
            *state = ConfirmationState {
                buy_yes,
                last_observed_at_ms: observed_at_ms,
                confirmations: 1,
            };
        } else if observed_at_ms > state.last_observed_at_ms {
            state.last_observed_at_ms = observed_at_ms;
            state.confirmations = state.confirmations.saturating_add(1);
        }
        WinnerConfirmationProgress {
            confirmations: state.confirmations,
            required,
            confirmed: state.confirmations >= required,
        }
    }
}

pub async fn run_chainlink_twap_feed(
    url: String,
    store: Arc<ChainlinkTwapStore>,
    shutdown: Arc<AtomicBool>,
    metrics: Arc<RuntimeMetrics>,
) {
    run_price_feed(url, store, shutdown, metrics, None).await;
}

/// Research-only variant: also subscribes to Chainlink and Binance spot
/// updates and exposes local receive timestamps through `observer`.
pub async fn run_chainlink_twap_feed_with_research(
    url: String,
    store: Arc<ChainlinkTwapStore>,
    shutdown: Arc<AtomicBool>,
    metrics: Arc<RuntimeMetrics>,
    observer: PriceFeedObserver,
) {
    run_price_feed(url, store, shutdown, metrics, Some(observer)).await;
}

async fn run_price_feed(
    url: String,
    store: Arc<ChainlinkTwapStore>,
    shutdown: Arc<AtomicBool>,
    metrics: Arc<RuntimeMetrics>,
    observer: Option<PriceFeedObserver>,
) {
    while !shutdown.load(Ordering::Relaxed) {
        match timeout(
            Duration::from_secs(10),
            tokio_tungstenite::connect_async(&url),
        )
        .await
        {
            Ok(Ok((mut socket, _))) => {
                metrics.record_late_resolution_winner_trading_rtds_connection();
                let subscription = price_feed_subscription(observer.is_some());
                if let Err(error) = socket
                    .send(Message::Text(subscription.to_string().into()))
                    .await
                {
                    warn_price_feed!(observer.is_some(), %error, "Chainlink TWAP subscription failed");
                } else {
                    if observer.is_some() {
                        info!(target: RESEARCH_FEED_LOG_TARGET,
                            research_spot = true, "Connected to Polymarket RTDS Chainlink 60s TWAP feed");
                    } else {
                        info!(
                            research_spot = false,
                            "Connected to Polymarket RTDS Chainlink 60s TWAP feed"
                        );
                    }
                    let mut heartbeat = tokio::time::interval(Duration::from_secs(5));
                    let mut last_frame_at = Instant::now();
                    let mut last_twap_at = Instant::now();
                    loop {
                        tokio::select! {
                            _ = heartbeat.tick() => {
                                if let Some(reason) = rtds_silence_reason(
                                    last_frame_at.elapsed(),
                                    last_twap_at.elapsed(),
                                    observer.is_some(),
                                ) {
                                    metrics.record_late_resolution_winner_trading_rtds_disconnect();
                                    warn_price_feed!(observer.is_some(), reason, frame_silence_secs = last_frame_at.elapsed().as_secs(),
                                        twap_silence_secs = last_twap_at.elapsed().as_secs(),
                                        "Chainlink TWAP feed stale; reconnecting");
                                    break;
                                }
                                if let Err(error) = socket.send(Message::Text("PING".into())).await {
                                    metrics.record_late_resolution_winner_trading_rtds_disconnect();
                                    warn_price_feed!(observer.is_some(), %error, "Chainlink TWAP heartbeat failed; reconnecting");
                                    break;
                                }
                            }
                            message = socket.next() => {
                                if message.is_some() {
                                    last_frame_at = Instant::now();
                                }
                                match message {
                                    Some(Ok(Message::Text(text))) => {
                                        if ingest_rtds_message(&store, &metrics, text.as_str(), observer.as_ref()) {
                                            last_twap_at = Instant::now();
                                        }
                                    }
                                    Some(Ok(Message::Binary(bytes))) => {
                                        if let Ok(text) = std::str::from_utf8(&bytes) {
                                            if ingest_rtds_message(&store, &metrics, text, observer.as_ref()) {
                                                last_twap_at = Instant::now();
                                            }
                                        }
                                    }
                                    Some(Ok(Message::Close(frame))) => {
                                        metrics.record_late_resolution_winner_trading_rtds_disconnect();
                                        warn_price_feed!(observer.is_some(), ?frame, "Chainlink TWAP feed closed; reconnecting");
                                        break;
                                    }
                                    Some(Err(error)) => {
                                        metrics.record_late_resolution_winner_trading_rtds_disconnect();
                                        warn_price_feed!(observer.is_some(), %error, "Chainlink TWAP feed read failed; reconnecting");
                                        break;
                                    }
                                    None => {
                                        metrics.record_late_resolution_winner_trading_rtds_disconnect();
                                        warn_price_feed!(observer.is_some(), "Chainlink TWAP feed ended; reconnecting");
                                        break;
                                    }
                                    _ => {}
                                }
                            }
                        }
                        if shutdown.load(Ordering::Relaxed) {
                            let _ = socket.close(None).await;
                            return;
                        }
                    }
                }
            }
            Ok(Err(error)) => {
                metrics.record_late_resolution_winner_trading_rtds_connect_failure();
                warn_price_feed!(observer.is_some(), %error, "Chainlink TWAP feed connection failed");
            }
            Err(_) => {
                metrics.record_late_resolution_winner_trading_rtds_connect_failure();
                warn_price_feed!(observer.is_some(), url = %url, "Chainlink TWAP feed connection timed out");
            }
        }
        if !shutdown.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }
}

fn rtds_silence_reason(
    frame_silence: Duration,
    twap_silence: Duration,
    research_mode: bool,
) -> Option<&'static str> {
    let twap_limit = if research_mode {
        RESEARCH_TWAP_SILENCE_LIMIT
    } else {
        NORMAL_TWAP_SILENCE_LIMIT
    };
    if twap_silence >= twap_limit {
        Some("twap_updates_stale")
    } else if frame_silence >= RTDS_FRAME_SILENCE_LIMIT {
        Some("websocket_frames_stale")
    } else {
        None
    }
}

fn price_feed_subscription(research_spot: bool) -> Value {
    let mut subscriptions = vec![serde_json::json!({
        "topic":"crypto_prices_twap_sixty","type":"update"
    })];
    if research_spot {
        subscriptions.push(serde_json::json!({
            "topic":"crypto_prices_chainlink","type":"*"
        }));
        subscriptions.push(serde_json::json!({
            "topic":"crypto_prices","type":"update"
        }));
    }
    serde_json::json!({"action":"subscribe","subscriptions":subscriptions})
}

fn ingest_rtds_message(
    store: &ChainlinkTwapStore,
    metrics: &RuntimeMetrics,
    text: &str,
    observer: Option<&PriceFeedObserver>,
) -> bool {
    let received_at_ms = Utc::now().timestamp_millis();
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        debug!(message = text, "Ignored non-JSON RTDS message");
        return false;
    };
    let Some(topic) = value.get("topic").and_then(Value::as_str) else {
        return false;
    };
    if !matches!(
        topic,
        "crypto_prices_twap_sixty" | "crypto_prices_chainlink" | "crypto_prices"
    ) {
        return false;
    }
    let Some(payload) = value.get("payload") else {
        return false;
    };
    let Some(symbol) = payload.get("symbol").and_then(Value::as_str) else {
        return false;
    };
    let Some(observed_at_ms) = payload
        .get("timestamp")
        .and_then(|value| value.as_i64().or_else(|| value.as_str()?.parse().ok()))
    else {
        return false;
    };
    if observed_at_ms <= 0 {
        return false;
    }
    let price = parse_rtds_price(topic, payload);
    let Some(price) = price.filter(|price| *price > Decimal::ZERO) else {
        return false;
    };
    let asset = if topic == "crypto_prices" {
        symbol
            .to_ascii_lowercase()
            .strip_suffix("usdt")
            .unwrap_or("")
            .to_owned()
    } else {
        normalize_asset(symbol)
    };
    if asset.is_empty() {
        return false;
    }
    let sample = RtdsReferenceSample {
        topic: topic.to_owned(),
        symbol: symbol.to_owned(),
        asset,
        observed_at_ms,
        publisher_at_ms: value
            .get("timestamp")
            .and_then(|value| value.as_i64().or_else(|| value.as_str()?.parse().ok())),
        received_at_ms,
        value: price,
    };
    let mut is_new_twap = false;
    if topic == "crypto_prices_twap_sixty" {
        is_new_twap = store
            .latest_observed_at_ms(&sample.asset)
            .is_none_or(|previous| observed_at_ms > previous);
        trace!(
            symbol,
            observed_at_ms,
            received_at_ms,
            age_ms = received_at_ms.saturating_sub(observed_at_ms),
            "Received Chainlink TWAP observation"
        );
        metrics.record_late_resolution_winner_trading_rtds_observation(observed_at_ms);
        store.ingest(
            symbol,
            TwapSample {
                observed_at_ms,
                value: price,
            },
        );
        store.ingest_twap_reference_timing(sample.clone());
    } else {
        store.ingest_research_spot(sample.clone());
    }
    if let Some(observer) = observer {
        observer(sample);
    }
    is_new_twap
}

fn parse_rtds_price(topic: &str, payload: &Value) -> Option<Decimal> {
    let human_value = || {
        payload.get("value").and_then(|value| match value {
            Value::String(raw) => Decimal::from_str(raw).ok(),
            Value::Number(raw) => Decimal::from_str(&raw.to_string()).ok(),
            _ => None,
        })
    };
    let full_accuracy = || {
        payload
            .get("full_accuracy_value")
            .and_then(Value::as_str)
            .and_then(|raw| Decimal::from_str(raw).ok())
    };
    if topic == "crypto_prices" {
        // Binance RTDS prices are already human-readable USD values. In
        // particular, its full_accuracy_value must never be divided by 1e18.
        human_value().or_else(full_accuracy)
    } else {
        // Chainlink spot and the settlement TWAP expose E18 raw values.
        full_accuracy()
            .map(|raw| raw / dec!(1_000_000_000_000_000_000))
            .or_else(human_value)
    }
}

fn normalize_asset(symbol: &str) -> String {
    symbol
        .trim()
        .split('/')
        .next()
        .unwrap_or(symbol)
        .to_ascii_lowercase()
}

fn volatility_bps_per_sqrt_second(samples: &[TwapSample]) -> f64 {
    let mut squared = 0.0;
    let mut count = 0_u32;
    for pair in samples.windows(2) {
        let previous = pair[0];
        let current = pair[1];
        let dt = (current.observed_at_ms - previous.observed_at_ms) as f64 / 1_000.0;
        let previous_price = previous.value.to_f64().unwrap_or(0.0);
        let current_price = current.value.to_f64().unwrap_or(0.0);
        if dt <= 0.0 || previous_price <= 0.0 || current_price <= 0.0 {
            continue;
        }
        let normalized = ((current_price / previous_price - 1.0) * 10_000.0) / dt.sqrt();
        squared += normalized * normalized;
        count += 1;
    }
    if count == 0 {
        0.0
    } else {
        (squared / f64::from(count)).sqrt()
    }
}

fn normal_cdf(value: f64) -> f64 {
    let x = value.abs();
    let t = 1.0 / (1.0 + 0.231_641_9 * x);
    let density = (-0.5 * x * x).exp() / (2.0 * std::f64::consts::PI).sqrt();
    let polynomial = t
        * (0.319_381_530
            + t * (-0.356_563_782
                + t * (1.781_477_937 + t * (-1.821_255_978 + t * 1.330_274_429))));
    let upper = 1.0 - density * polynomial;
    if value >= 0.0 {
        upper
    } else {
        1.0 - upper
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fair_value_snapshot_is_past_only_and_uses_window_opening() {
        let store = ChainlinkTwapStore::default();
        for second in 0..30 {
            store.ingest(
                "btc",
                TwapSample {
                    observed_at_ms: 1_000_000 + second * 1_000,
                    value: dec!(100) + Decimal::from(second) / dec!(100),
                },
            );
        }
        let snapshot = store
            .fair_value_snapshot("btc", 1_000_000, 1_029_000, 1_000, 2_000, 30, 20, 0.1)
            .unwrap();
        assert_eq!(snapshot.opening, dec!(100));
        assert_eq!(snapshot.current, dec!(100.29));
        assert_eq!(snapshot.observed_at_ms, 1_029_000);
        assert!(snapshot.volatility_bps_per_sqrt_second >= 0.1);
        assert!(store
            .fair_value_snapshot("btc", 1_000_000, 1_040_000, 1_000, 2_000, 30, 20, 0.1)
            .is_none());
    }

    fn policy() -> WinnerModelPolicy {
        WinnerModelPolicy {
            max_feed_age_ms: 2_000,
            max_opening_skew_ms: 1_000,
            volatility_lookback_secs: 120,
            min_observations: 5,
            min_volatility_bps_per_sqrt_second: 0.5,
            min_margin_bps: 2.0,
            reversal_sigma_multiplier: 1.0,
            max_spread: dec!(0.03),
            min_depth_usdc: dec!(10),
            fee_buffer: dec!(0.002),
            model_risk_buffer: dec!(0.005),
            min_net_ev: dec!(0.001),
        }
    }

    fn seeded_store(current_price: Decimal) -> ChainlinkTwapStore {
        let store = ChainlinkTwapStore::default();
        for index in 0..=10_i64 {
            let value = if index == 10 {
                current_price
            } else {
                dec!(100) + Decimal::from(index) * dec!(0.01)
            };
            store.ingest(
                "btc/usd",
                TwapSample {
                    observed_at_ms: index * 1_000,
                    value,
                },
            );
        }
        store
    }

    #[test]
    fn admits_only_when_chainlink_margin_probability_and_book_all_pass() {
        let store = seeded_store(dec!(101));
        let evaluation = store.evaluate(
            WinnerModelInput {
                asset: "BTC",
                window_start_ms: 0,
                now_ms: 10_500,
                seconds_remaining: 2,
                buy_yes: true,
                bid_price: dec!(0.95),
                ask_price: dec!(0.96),
                ask_size: dec!(20),
            },
            policy(),
        );
        assert!(evaluation.admitted(), "{evaluation:?}");
        assert!(evaluation.probability > evaluation.effective_price);
        assert_eq!(
            evaluation.probability_edge_before_risk,
            evaluation.probability - evaluation.effective_price
        );
        assert_eq!(evaluation.required_probability, dec!(0.968));
        assert!(evaluation.net_ev > dec!(0));
    }

    #[test]
    fn adverse_direction_is_below_half_and_records_audit() {
        let evaluation = seeded_store(dec!(99.9)).evaluate(
            WinnerModelInput {
                asset: "btc",
                window_start_ms: 0,
                now_ms: 10_500,
                seconds_remaining: 30,
                buy_yes: true,
                bid_price: dec!(0.95),
                ask_price: dec!(0.96),
                ask_size: dec!(20),
            },
            policy(),
        );
        assert!(evaluation.probability < dec!(0.5));
        assert!(!evaluation.admitted());
        let audit = evaluation.audit.unwrap();
        // The store rejects timestamp zero; the nearest valid sample is at 1s.
        assert_eq!(audit.opening_skew_ms, 1_000);
        assert_eq!(audit.current_age_ms, 500);
        assert_eq!(audit.observation_count, 10);
        assert!(audit.probability_without_volatility_floor < 0.5);
    }

    #[test]
    fn sensitivity_baseline_matches_model_and_keeps_market_rejections() {
        for price in [dec!(99.9), dec!(100.1), dec!(101)] {
            let model = seeded_store(price).evaluate(
                WinnerModelInput {
                    asset: "btc",
                    window_start_ms: 0,
                    now_ms: 10_500,
                    seconds_remaining: 30,
                    buy_yes: true,
                    bid_price: dec!(0.95),
                    ask_price: dec!(0.96),
                    ask_size: dec!(20),
                },
                policy(),
            );
            let (admitted, probability, _) = sensitivity_decision(&model, policy(), 30, 1.0, 1.0);
            assert_eq!(admitted, model.admitted());
            assert!((probability - model.probability.to_f64().unwrap()).abs() < 1e-12);
            let mut thin = model.clone();
            thin.failed_checks.push(ModelRejection::InsufficientDepth);
            assert!(!sensitivity_decision(&thin, policy(), 30, 0.1, 0.5).0);
        }
        assert!(
            !sensitivity_decision(
                &ModelEvaluation::rejected(ModelRejection::StalePrice),
                policy(),
                30,
                0.1,
                0.5
            )
            .0
        );
    }

    #[test]
    fn horizon_estimate_rejects_gaps_and_does_not_overlap() {
        let samples = (0..=60)
            .map(|i| TwapSample {
                observed_at_ms: 1000 + i * 1000,
                value: dec!(100) + Decimal::from(i) * dec!(0.01),
            })
            .collect::<Vec<_>>();
        let ten = horizon_volatility(&samples, 10);
        assert_eq!(ten.pairs, 6);
        assert!(ten.bps_per_sqrt_second.unwrap() > volatility_bps_per_sqrt_second(&samples));
        assert_eq!(horizon_volatility(&[samples[0], samples[60]], 10).pairs, 0);
        assert_eq!(horizon_volatility(&[], 10).bps_per_sqrt_second, None);
    }

    #[test]
    fn volatility_floor_audit_does_not_bypass_admission() {
        let mut conservative = policy();
        conservative.min_volatility_bps_per_sqrt_second = 100.0;
        let evaluation = seeded_store(dec!(100.1)).evaluate(
            WinnerModelInput {
                asset: "btc",
                window_start_ms: 0,
                now_ms: 10_500,
                seconds_remaining: 30,
                buy_yes: true,
                bid_price: dec!(0.95),
                ask_price: dec!(0.96),
                ask_size: dec!(20),
            },
            conservative,
        );
        let audit = evaluation.audit.as_ref().unwrap();
        assert!(audit.volatility_floor_active);
        assert_eq!(evaluation.volatility_bps_per_sqrt_second, 100.0);
        assert!(
            audit.probability_without_volatility_floor > evaluation.probability.to_f64().unwrap()
        );
        assert!(!evaluation.admitted());
    }

    #[test]
    fn rejects_missing_opening_reference_stale_feed_and_reversible_direction() {
        let store = seeded_store(dec!(100.03));
        let base = WinnerModelInput {
            asset: "btc",
            window_start_ms: 0,
            now_ms: 10_500,
            seconds_remaining: 30,
            buy_yes: true,
            bid_price: dec!(0.95),
            ask_price: dec!(0.96),
            ask_size: dec!(20),
        };
        assert_eq!(
            store.evaluate(base, policy()).rejection,
            Some(ModelRejection::ReversalBoundary)
        );
        assert_eq!(
            store
                .evaluate(
                    WinnerModelInput {
                        window_start_ms: 50_000,
                        ..base
                    },
                    policy(),
                )
                .rejection,
            Some(ModelRejection::NoOpeningReference)
        );
        assert_eq!(
            store
                .evaluate(
                    WinnerModelInput {
                        now_ms: 20_000,
                        ..base
                    },
                    policy(),
                )
                .rejection,
            Some(ModelRejection::StalePrice)
        );
    }

    #[test]
    fn fair_value_readiness_explains_missing_stale_and_insufficient_data() {
        let store = ChainlinkTwapStore::default();
        let readiness = |window_start_ms, now_ms, max_age_ms, minimum| {
            store.fair_value_snapshot_readiness(
                "btc",
                window_start_ms,
                now_ms,
                100,
                max_age_ms,
                120,
                minimum,
                0.8,
            )
        };
        assert_eq!(
            readiness(1_000, 2_000, 5_000, 2),
            FairValueSnapshotReadiness::NoPriceHistory
        );
        store.ingest(
            "btc",
            TwapSample {
                observed_at_ms: 1_000,
                value: dec!(100),
            },
        );
        store.ingest(
            "btc",
            TwapSample {
                observed_at_ms: 2_000,
                value: dec!(100.01),
            },
        );
        assert_eq!(
            readiness(1_000, 2_000, 5_000, 3),
            FairValueSnapshotReadiness::InsufficientObservations
        );
        assert_eq!(
            readiness(50_000, 2_000, 5_000, 2),
            FairValueSnapshotReadiness::NoOpeningReference
        );
        assert_eq!(
            readiness(1_000, 10_000, 100, 2),
            FairValueSnapshotReadiness::StaleFeed
        );
        assert!(matches!(
            readiness(1_000, 2_000, 5_000, 2),
            FairValueSnapshotReadiness::Ready(_)
        ));
    }

    #[test]
    fn fair_value_research_path_excludes_future_samples() {
        let store = ChainlinkTwapStore::default();
        for observed_at_ms in [1_000, 2_000, 3_000] {
            store.ingest(
                "btc",
                TwapSample {
                    observed_at_ms,
                    value: dec!(100),
                },
            );
        }
        let path = store.fair_value_recent_twap_samples("btc", 2_500, 1_000);
        assert_eq!(path.len(), 1);
        assert_eq!(path[0].observed_at_ms, 2_000);
    }

    #[test]
    fn tolerates_bounded_rtds_publisher_clock_skew_but_rejects_far_future_samples() {
        let store = ChainlinkTwapStore::default();
        for index in 0..5_i64 {
            store.ingest(
                "btc",
                TwapSample {
                    observed_at_ms: 10_000 + index * 1_000,
                    value: dec!(100) + Decimal::from(index) / dec!(100),
                },
            );
        }

        assert!(matches!(
            store.fair_value_snapshot_readiness("btc", 10_000, 10_000, 1_000, 5_000, 120, 5, 0.8,),
            FairValueSnapshotReadiness::Ready(_)
        ));
        assert_eq!(
            store.fair_value_snapshot_readiness("btc", 10_000, 3_999, 1_000, 5_000, 120, 5, 0.8,),
            FairValueSnapshotReadiness::StaleFeed
        );
    }

    #[test]
    fn records_all_market_boundary_and_ev_failures_without_changing_primary_order() {
        let store = seeded_store(dec!(100.03));
        let evaluation = store.evaluate(
            WinnerModelInput {
                asset: "btc",
                window_start_ms: 0,
                now_ms: 10_500,
                seconds_remaining: 30,
                buy_yes: true,
                bid_price: dec!(0.91),
                ask_price: dec!(0.96),
                ask_size: dec!(5),
            },
            policy(),
        );

        assert_eq!(evaluation.rejection, Some(ModelRejection::SpreadTooWide));
        assert!(evaluation
            .failed_checks
            .contains(&ModelRejection::InsufficientDepth));
        assert!(evaluation
            .failed_checks
            .contains(&ModelRejection::ReversalBoundary));
        assert!(evaluation
            .failed_checks
            .contains(&ModelRejection::NetEvTooLow));
        assert!(evaluation.opening_twap.is_some());
    }

    #[test]
    fn confirmation_requires_distinct_feed_observations_and_resets_direction() {
        let mut tracker = WinnerConfirmationTracker::default();
        assert!(!tracker.observe("slug", true, 1_000, 2, 5_000).confirmed);
        assert!(!tracker.observe("slug", true, 1_000, 2, 5_000).confirmed);
        assert!(tracker.observe("slug", true, 2_000, 2, 5_000).confirmed);
        assert!(!tracker.observe("slug", false, 3_000, 2, 5_000).confirmed);
        tracker.reject("slug");
        assert!(!tracker.observe("slug", false, 4_000, 2, 5_000).confirmed);
    }

    #[test]
    fn confirmation_progress_reports_distinct_observation_count() {
        let mut tracker = WinnerConfirmationTracker::default();
        assert_eq!(
            tracker.observe("slug", true, 1_000, 2, 5_000),
            WinnerConfirmationProgress {
                confirmations: 1,
                required: 2,
                confirmed: false,
            }
        );
        assert_eq!(
            tracker.observe("slug", true, 2_000, 2, 5_000),
            WinnerConfirmationProgress {
                confirmations: 2,
                required: 2,
                confirmed: true,
            }
        );
    }

    #[test]
    fn confirmation_expires_after_ttl() {
        let mut tracker = WinnerConfirmationTracker::default();
        assert!(!tracker.observe("slug", true, 1_000, 2, 2_000).confirmed);
        assert!(!tracker.observe("slug", true, 4_000, 2, 2_000).confirmed);
        assert!(tracker.observe("slug", true, 5_000, 2, 2_000).confirmed);
    }

    #[test]
    fn parses_exact_e18_rtds_value() {
        let store = ChainlinkTwapStore::default();
        assert!(ingest_rtds_message(
            &store,
            &RuntimeMetrics::default(),
            r#"{"topic":"crypto_prices_twap_sixty","payload":{"symbol":"btc/usd","full_accuracy_value":"65000500000000000000000","timestamp":1000}}"#,
            None,
        ));
        let samples = store.samples.read().unwrap();
        assert_eq!(samples["btc"].back().unwrap().value, dec!(65000.5));
    }

    #[test]
    fn repeated_twap_timestamp_does_not_refresh_feed_watchdog() {
        let store = ChainlinkTwapStore::default();
        let message = r#"{"topic":"crypto_prices_twap_sixty","payload":{"symbol":"btc/usd","value":65000,"timestamp":1000}}"#;
        assert!(ingest_rtds_message(
            &store,
            &RuntimeMetrics::default(),
            message,
            None
        ));
        assert!(!ingest_rtds_message(
            &store,
            &RuntimeMetrics::default(),
            message,
            None
        ));
    }

    #[test]
    fn binance_full_accuracy_is_not_e18_and_value_takes_priority() {
        let store = ChainlinkTwapStore::default();
        let now = Utc::now().timestamp_millis();
        let event = serde_json::json!({
            "topic":"crypto_prices","timestamp":now,
            "payload":{"symbol":"btcusdt","timestamp":now,
                "full_accuracy_value":"81280.040000","value":81280.04}
        });
        assert!(!ingest_rtds_message(
            &store,
            &RuntimeMetrics::default(),
            &event.to_string(),
            None
        ));
        let samples = store.fair_value_recent_spot_samples(
            "crypto_prices",
            "btc",
            Utc::now().timestamp_millis(),
            90_000,
        );
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].value, dec!(81280.04));
        assert!(store.samples.read().unwrap().get("btc").is_none());
        assert_eq!(
            parse_rtds_price(
                "crypto_prices",
                &serde_json::json!({"full_accuracy_value":"81280.040000"})
            ),
            Some(dec!(81280.040000))
        );
    }

    #[test]
    fn pong_cannot_mask_missing_twap_updates() {
        assert_eq!(
            rtds_silence_reason(Duration::from_secs(1), Duration::from_secs(12), true),
            Some("twap_updates_stale")
        );
        assert_eq!(
            rtds_silence_reason(Duration::from_secs(1), Duration::from_secs(12), false),
            None
        );
        assert_eq!(
            rtds_silence_reason(Duration::from_secs(31), Duration::from_secs(1), true),
            Some("websocket_frames_stale")
        );
        assert_eq!(
            rtds_silence_reason(Duration::from_secs(1), Duration::from_secs(31), false),
            Some("twap_updates_stale")
        );
        let store = ChainlinkTwapStore::default();
        assert!(!ingest_rtds_message(
            &store,
            &RuntimeMetrics::default(),
            "PONG",
            None
        ));
    }

    #[test]
    fn research_subscription_adds_spot_only_for_research() {
        let normal = price_feed_subscription(false).to_string();
        let research = price_feed_subscription(true).to_string();
        assert!(normal.contains("crypto_prices_twap_sixty"));
        assert!(!normal.contains("crypto_prices_chainlink"));
        assert!(research.contains("crypto_prices_chainlink"));
        assert!(research.contains("crypto_prices"));
    }

    #[test]
    fn spot_feed_never_changes_settlement_twap_and_captures_receive_time() {
        let store = ChainlinkTwapStore::default();
        let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = observed.clone();
        let observer: PriceFeedObserver = Arc::new(move |sample| sink.lock().unwrap().push(sample));
        let now = Utc::now().timestamp_millis();
        let event = serde_json::json!({
            "topic":"crypto_prices_chainlink","timestamp":now-10,
            "payload":{"symbol":"btc/usd","timestamp":now-20,"value":"65000.5"}
        });
        ingest_rtds_message(
            &store,
            &RuntimeMetrics::default(),
            &event.to_string(),
            Some(&observer),
        );
        assert!(store.samples.read().unwrap().get("btc").is_none());
        let received = observed.lock().unwrap();
        assert_eq!(received.len(), 1);
        assert_eq!(received[0].publisher_at_ms, Some(now - 10));
        assert!(received[0].received_at_ms >= now);
        assert_eq!(received[0].value, dec!(65000.5));
        let spot = store.fair_value_recent_spot_samples(
            "crypto_prices_chainlink",
            "btc",
            received[0].received_at_ms,
            90_000,
        );
        assert_eq!(spot.len(), 1);
    }

    #[test]
    fn twap_snapshot_preserves_observed_publisher_and_receive_times() {
        let store = ChainlinkTwapStore::default();
        let now = Utc::now().timestamp_millis();
        for offset in [1_000_i64, 0] {
            let observed = now - offset;
            let event = serde_json::json!({
                "topic":"crypto_prices_twap_sixty",
                "timestamp":observed+100,
                "payload":{
                    "symbol":"btc/usd",
                    "timestamp":observed,
                    "full_accuracy_value":"65000500000000000000000"
                }
            });
            assert!(ingest_rtds_message(
                &store,
                &RuntimeMetrics::default(),
                &event.to_string(),
                None
            ));
        }
        let FairValueSnapshotReadiness::Ready(snapshot) = store.fair_value_snapshot_readiness(
            "btc",
            now - 1_000,
            now + 1_000,
            100,
            5_000,
            120,
            2,
            0.8,
        ) else {
            panic!("expected ready snapshot");
        };
        assert_eq!(snapshot.observed_at_ms, now);
        assert_eq!(snapshot.publisher_at_ms, Some(now + 100));
        assert!(snapshot.received_at_ms.is_some());
    }

    #[test]
    fn spot_asof_query_excludes_future_receive_and_source_times() {
        let store = ChainlinkTwapStore::default();
        for (received_at_ms, observed_at_ms) in [(1000, 900), (2000, 1900), (3000, 4000)] {
            store.ingest_research_spot(RtdsReferenceSample {
                topic: "crypto_prices".into(),
                symbol: "btcusdt".into(),
                asset: "btc".into(),
                observed_at_ms,
                publisher_at_ms: None,
                received_at_ms,
                value: dec!(100),
            });
        }
        assert_eq!(
            store
                .fair_value_recent_spot_samples("crypto_prices", "btc", 1500, 90_000)
                .len(),
            1
        );
        assert_eq!(
            store
                .fair_value_recent_spot_samples("crypto_prices", "btc", 3500, 90_000)
                .len(),
            2
        );
    }

    #[test]
    fn binance_symbol_maps_to_asset_without_updating_twap() {
        let store = ChainlinkTwapStore::default();
        let now = Utc::now().timestamp_millis();
        let event = serde_json::json!({
            "topic":"crypto_prices","timestamp":now,
            "payload":{"symbol":"ETHUSDT","timestamp":now,"value":3500.25}
        });
        ingest_rtds_message(&store, &RuntimeMetrics::default(), &event.to_string(), None);
        assert!(store.samples.read().unwrap().get("eth").is_none());
        let samples = store.fair_value_recent_spot_samples(
            "crypto_prices",
            "eth",
            Utc::now().timestamp_millis(),
            90_000,
        );
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].symbol, "ETHUSDT");
        assert_eq!(samples[0].value, dec!(3500.25));
    }

    #[test]
    fn attempt_limits_count_orders_not_scans() {
        assert!(attempt_allowed(0, 0, 1, 1));
        assert!(!attempt_allowed(1, 0, 1, 1));
        assert!(!attempt_allowed(0, 1, 2, 1));
        assert!(attempt_allowed(10, 0, 0, 1));
    }
}
