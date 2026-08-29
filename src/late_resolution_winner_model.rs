//! Independent Late-Resolution Winner Trading model based on Polymarket's official Chainlink TWAP feed.

use futures::{SinkExt, StreamExt};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde_json::Value;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, VecDeque};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};

use crate::runtime_metrics::RuntimeMetrics;

const HISTORY_MILLIS: i64 = 10 * 60 * 1_000;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TwapSample {
    pub observed_at_ms: i64,
    pub value: Decimal,
}

#[derive(Default)]
pub struct ChainlinkTwapStore {
    samples: RwLock<HashMap<String, VecDeque<TwapSample>>>,
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

    pub fn evaluate(&self, input: WinnerModelInput, policy: WinnerModelPolicy) -> ModelEvaluation {
        let asset = normalize_asset(input.asset);
        let all = self.samples.read().expect("TWAP store lock poisoned");
        let Some(samples) = all.get(&asset) else {
            return ModelEvaluation::rejected(ModelRejection::NoPriceHistory);
        };
        let Some(current) = samples.back().copied() else {
            return ModelEvaluation::rejected(ModelRejection::NoPriceHistory);
        };
        let age_ms = input.now_ms.saturating_sub(current.observed_at_ms);
        if age_ms < 0 || age_ms > policy.max_feed_age_ms {
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
        let probability = Decimal::try_from(normal_cdf(z).clamp(0.5, 0.9999)).unwrap_or(dec!(0.5));
        let effective_price = (input.ask_price + policy.fee_buffer).min(dec!(1));
        let probability_edge_before_risk = probability - effective_price;
        let required_probability = effective_price + policy.model_risk_buffer + policy.min_net_ev;
        let net_ev = probability - effective_price - policy.model_risk_buffer;
        let mut evaluation = ModelEvaluation {
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

#[derive(Debug, Clone, PartialEq)]
pub struct ModelEvaluation {
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
    while !shutdown.load(Ordering::Relaxed) {
        match tokio_tungstenite::connect_async(&url).await {
            Ok((mut socket, _)) => {
                metrics.record_late_resolution_winner_trading_rtds_connection();
                let subscription = serde_json::json!({
                    "action": "subscribe",
                    "subscriptions": [{
                        "topic": "crypto_prices_twap_sixty",
                        "type": "update"
                    }]
                });
                if let Err(error) = socket
                    .send(Message::Text(subscription.to_string().into()))
                    .await
                {
                    warn!(%error, "Chainlink TWAP subscription failed");
                } else {
                    info!("Connected to Polymarket RTDS Chainlink 60s TWAP feed");
                    let mut heartbeat = tokio::time::interval(Duration::from_secs(5));
                    let mut last_message_at = Instant::now();
                    loop {
                        tokio::select! {
                            _ = heartbeat.tick() => {
                                if last_message_at.elapsed() > Duration::from_secs(30) {
                                    metrics.record_late_resolution_winner_trading_rtds_disconnect();
                                    warn!(silence_secs = last_message_at.elapsed().as_secs(), "Chainlink TWAP feed stale; reconnecting");
                                    break;
                                }
                                if let Err(error) = socket.send(Message::Text("PING".into())).await {
                                    metrics.record_late_resolution_winner_trading_rtds_disconnect();
                                    warn!(%error, "Chainlink TWAP heartbeat failed; reconnecting");
                                    break;
                                }
                            }
                            message = socket.next() => {
                                if message.is_some() {
                                    last_message_at = Instant::now();
                                }
                                match message {
                                    Some(Ok(Message::Text(text))) => ingest_rtds_message(&store, &metrics, text.as_str()),
                                    Some(Ok(Message::Binary(bytes))) => {
                                        if let Ok(text) = std::str::from_utf8(&bytes) {
                                            ingest_rtds_message(&store, &metrics, text);
                                        }
                                    }
                                    Some(Ok(Message::Close(frame))) => {
                                        metrics.record_late_resolution_winner_trading_rtds_disconnect();
                                        warn!(?frame, "Chainlink TWAP feed closed; reconnecting");
                                        break;
                                    }
                                    Some(Err(error)) => {
                                        metrics.record_late_resolution_winner_trading_rtds_disconnect();
                                        warn!(%error, "Chainlink TWAP feed read failed; reconnecting");
                                        break;
                                    }
                                    None => {
                                        metrics.record_late_resolution_winner_trading_rtds_disconnect();
                                        warn!("Chainlink TWAP feed ended; reconnecting");
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
            Err(error) => {
                metrics.record_late_resolution_winner_trading_rtds_connect_failure();
                warn!(%error, "Chainlink TWAP feed connection failed");
            }
        }
        if !shutdown.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }
}

fn ingest_rtds_message(store: &ChainlinkTwapStore, metrics: &RuntimeMetrics, text: &str) {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        debug!(message = text, "Ignored non-JSON RTDS message");
        return;
    };
    let Some(payload) = value.get("payload") else {
        return;
    };
    let Some(symbol) = payload.get("symbol").and_then(Value::as_str) else {
        return;
    };
    let Some(observed_at_ms) = payload.get("timestamp").and_then(Value::as_i64) else {
        return;
    };
    let price = payload
        .get("full_accuracy_value")
        .and_then(Value::as_str)
        .and_then(|raw| Decimal::from_str(raw).ok())
        .map(|raw| raw / dec!(1_000_000_000_000_000_000))
        .or_else(|| {
            payload.get("value").and_then(|value| match value {
                Value::String(raw) => Decimal::from_str(raw).ok(),
                Value::Number(raw) => Decimal::from_str(&raw.to_string()).ok(),
                _ => None,
            })
        });
    if let Some(value) = price {
        metrics.record_late_resolution_winner_trading_rtds_observation(observed_at_ms);
        store.ingest(
            symbol,
            TwapSample {
                observed_at_ms,
                value,
            },
        );
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
        ingest_rtds_message(
            &store,
            &RuntimeMetrics::default(),
            r#"{"topic":"crypto_prices_twap_sixty","payload":{"symbol":"btc/usd","full_accuracy_value":"65000500000000000000000","timestamp":1000}}"#,
        );
        let samples = store.samples.read().unwrap();
        assert_eq!(samples["btc"].back().unwrap().value, dec!(65000.5));
    }

    #[test]
    fn attempt_limits_count_orders_not_scans() {
        assert!(attempt_allowed(0, 0, 1, 1));
        assert!(!attempt_allowed(1, 0, 1, 1));
        assert!(!attempt_allowed(0, 1, 2, 1));
        assert!(attempt_allowed(10, 0, 0, 1));
    }
}
