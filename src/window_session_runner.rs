use crate::config::Config;
use crate::market::MarketInfo;
use crate::monitor::arbitrage::{
    ArbitrageDetector, ArbitrageOpportunity, ArbitrageScan, ArbitrageShadowCandidate,
};
use crate::monitor::orderbook::{best_ask, OrderBookPair};
use crate::opportunity_admission::{AdmissionContext, OpportunityAdmissionService};
use crate::runtime_metrics::RuntimeMetrics;
use crate::strategy_gate::{validate_book_timestamps, SignalDeduplicator};
use dashmap::DashMap;
use polymarket_client_sdk::types::{Decimal, B256};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub struct MarketTick {
    pub yes_best_ask: Option<(Decimal, Decimal)>,
    pub no_best_ask: Option<(Decimal, Decimal)>,
    pub total_ask_price: Option<Decimal>,
    pub yes_direction: &'static str,
    pub no_direction: &'static str,
    pub market_display: String,
    pub market_symbol: String,
    pub opportunity: Option<ArbitrageOpportunity>,
    pub shadow_candidate: Option<ArbitrageShadowCandidate>,
    pub rest_shadow_candidate: Option<ArbitrageRestShadowCandidate>,
}

#[derive(Debug, Clone)]
pub struct ArbitrageRestShadowCandidate {
    pub source: &'static str,
    pub opportunity: ArbitrageOpportunity,
    pub observed_at_ms: i64,
}

#[derive(Debug, Clone, Copy)]
struct StableCandidate {
    first_observed_at_ms: i64,
    last_yes_book_timestamp_ms: i64,
    last_no_book_timestamp_ms: i64,
    observations: u32,
}

pub struct WindowSessionRunner {
    markets: HashMap<B256, MarketInfo>,
    last_prices: DashMap<B256, (Decimal, Decimal)>,
    /// First/last observation time for a market that remains continuously in
    /// the price-positive but depth-rejected state. Quote prices may change
    /// while the underlying state persists.
    shadow_observations: DashMap<B256, (i64, i64)>,
    stable_candidates: DashMap<B256, StableCandidate>,
    detector: ArbitrageDetector,
    admission: OpportunityAdmissionService,
    deduplicator: SignalDeduplicator,
    shadow_deduplicator: SignalDeduplicator,
    signal_confirmation_ms: u64,
    max_book_age_ms: i64,
    max_book_skew_ms: i64,
    metrics: Arc<RuntimeMetrics>,
}

impl WindowSessionRunner {
    pub fn new(markets: &[MarketInfo], config: &Config, metrics: Arc<RuntimeMetrics>) -> Self {
        Self {
            markets: markets
                .iter()
                .cloned()
                .map(|market| (market.market_id, market))
                .collect(),
            last_prices: DashMap::new(),
            shadow_observations: DashMap::new(),
            stable_candidates: DashMap::new(),
            detector: ArbitrageDetector::with_min_depth(
                config.min_profit_threshold,
                config.min_ask_depth_usdc,
            ),
            admission: OpportunityAdmissionService::from_config(config),
            deduplicator: SignalDeduplicator::new(Duration::from_millis(config.signal_cooldown_ms)),
            shadow_deduplicator: SignalDeduplicator::new(Duration::from_millis(
                config.signal_cooldown_ms,
            )),
            signal_confirmation_ms: config.arbitrage_signal_confirmation_ms,
            max_book_age_ms: config.max_book_age_ms,
            max_book_skew_ms: config.max_book_skew_ms,
            metrics,
        }
    }

    pub fn process_pair(
        &self,
        pair: &OrderBookPair,
        now_ms: i64,
        trading_paused: bool,
        connection_healthy: bool,
        unresolved_state: bool,
    ) -> anyhow::Result<MarketTick> {
        if let Err(error) = validate_book_timestamps(
            pair.yes_book.timestamp,
            pair.no_book.timestamp,
            now_ms,
            self.max_book_age_ms,
            self.max_book_skew_ms,
        ) {
            self.stable_candidates.remove(&pair.market_id);
            self.shadow_observations.remove(&pair.market_id);
            return Err(error);
        }
        self.metrics.record_orderbook_pair();
        let yes_best_ask = best_ask(&pair.yes_book).map(|level| (level.price, level.size));
        let no_best_ask = best_ask(&pair.no_book).map(|level| (level.price, level.size));
        let total_ask_price = yes_best_ask.and_then(|(yes, _)| no_best_ask.map(|(no, _)| yes + no));
        let (yes_direction, no_direction) =
            self.directions(pair.market_id, yes_best_ask, no_best_ask);
        let market = self.markets.get(&pair.market_id);
        let market_symbol = market
            .map(|market| market.crypto_symbol.clone())
            .unwrap_or_default();
        let market_display = if market_symbol.is_empty() {
            market
                .map(|market| market.title.clone())
                .unwrap_or_else(|| "unknown market".to_string())
        } else {
            format!("{market_symbol} prediction market")
        };
        let mut shadow_candidate = None;
        let mut rest_shadow_candidate = None;
        let opportunity = (|| {
            let total_price = total_ask_price?;
            let scan = self.detector.check_arbitrage_reasoned(
                &pair.yes_book,
                &pair.no_book,
                &pair.market_id,
            );
            let opportunity = match scan {
                ArbitrageScan::Opportunity(opportunity) => {
                    self.shadow_observations.remove(&pair.market_id);
                    if self.shadow_deduplicator.admit(
                        opportunity.market_id,
                        opportunity.yes_ask_price,
                        opportunity.no_ask_price,
                        Instant::now(),
                    ) {
                        rest_shadow_candidate = Some(ArbitrageRestShadowCandidate {
                            source: "full_depth",
                            opportunity: opportunity.clone(),
                            observed_at_ms: now_ms,
                        });
                    }
                    opportunity
                }
                ArbitrageScan::DepthRejected(mut candidate) => {
                    self.stable_candidates.remove(&pair.market_id);
                    self.metrics.record_depth_rejection();
                    let first_seen_at_ms = self
                        .shadow_observations
                        .get(&pair.market_id)
                        .filter(|previous| {
                            now_ms.saturating_sub(previous.1) <= self.max_book_age_ms
                        })
                        .map(|previous| previous.0)
                        .unwrap_or(now_ms);
                    candidate.first_seen_at_ms = first_seen_at_ms;
                    candidate.observed_duration_ms = now_ms.saturating_sub(first_seen_at_ms);
                    self.shadow_observations
                        .insert(pair.market_id, (first_seen_at_ms, now_ms));
                    let opportunity = candidate.as_opportunity();
                    if self.shadow_deduplicator.admit(
                        opportunity.market_id,
                        opportunity.yes_ask_price,
                        opportunity.no_ask_price,
                        Instant::now(),
                    ) {
                        rest_shadow_candidate = Some(ArbitrageRestShadowCandidate {
                            source: "depth_rejected",
                            opportunity,
                            observed_at_ms: now_ms,
                        });
                    }
                    shadow_candidate = Some(candidate);
                    return None;
                }
                ArbitrageScan::NoEdge => {
                    self.shadow_observations.remove(&pair.market_id);
                    self.stable_candidates.remove(&pair.market_id);
                    return None;
                }
            };
            let now =
                chrono::DateTime::from_timestamp_millis(now_ms).unwrap_or_else(chrono::Utc::now);
            let result = self.admission.evaluate(
                &opportunity,
                AdmissionContext {
                    total_price,
                    trading_paused,
                    connection_healthy,
                    unresolved_state,
                    market_end: market.map(|market| market.end_date),
                    now,
                },
            );
            if let Err(reason) = result {
                self.metrics.record_admission_rejection(reason);
                self.stable_candidates.remove(&opportunity.market_id);
                return None;
            }
            if !self.confirmation_admits(&opportunity, now_ms) {
                self.metrics.record_stability_pending();
                return None;
            }
            if !self.deduplicator.admit(
                opportunity.market_id,
                opportunity.yes_ask_price,
                opportunity.no_ask_price,
                Instant::now(),
            ) {
                self.metrics.record_duplicate_signal();
                return None;
            }
            self.metrics.record_admitted();
            Some(opportunity)
        })();
        Ok(MarketTick {
            yes_best_ask,
            no_best_ask,
            total_ask_price,
            yes_direction,
            no_direction,
            market_display,
            market_symbol,
            opportunity,
            shadow_candidate,
            rest_shadow_candidate,
        })
    }

    fn confirmation_admits(&self, opportunity: &ArbitrageOpportunity, now_ms: i64) -> bool {
        if self.signal_confirmation_ms == 0 {
            return true;
        }
        if let Some(mut previous) = self.stable_candidates.get_mut(&opportunity.market_id) {
            let independent_update = previous.last_yes_book_timestamp_ms
                != opportunity.yes_book_timestamp_ms
                || previous.last_no_book_timestamp_ms != opportunity.no_book_timestamp_ms;
            if independent_update {
                previous.last_yes_book_timestamp_ms = opportunity.yes_book_timestamp_ms;
                previous.last_no_book_timestamp_ms = opportunity.no_book_timestamp_ms;
                previous.observations = previous.observations.saturating_add(1);
            }
            return previous.observations >= 2
                && now_ms.saturating_sub(previous.first_observed_at_ms)
                    >= i64::try_from(self.signal_confirmation_ms).unwrap_or(i64::MAX);
        }
        self.stable_candidates.insert(
            opportunity.market_id,
            StableCandidate {
                first_observed_at_ms: now_ms,
                last_yes_book_timestamp_ms: opportunity.yes_book_timestamp_ms,
                last_no_book_timestamp_ms: opportunity.no_book_timestamp_ms,
                observations: 1,
            },
        );
        false
    }

    /// A transport reconnect invalidates all cross-message assumptions. Fresh
    /// books from both legs must arrive before a candidate can be rebuilt.
    pub fn invalidate_transient_state(&self) {
        self.last_prices.clear();
        self.shadow_observations.clear();
        self.stable_candidates.clear();
        self.deduplicator.clear();
        self.shadow_deduplicator.clear();
    }

    fn directions(
        &self,
        market_id: B256,
        yes: Option<(Decimal, Decimal)>,
        no: Option<(Decimal, Decimal)>,
    ) -> (&'static str, &'static str) {
        let (Some((yes_price, _)), Some((no_price, _))) = (yes, no) else {
            return ("", "");
        };
        let previous = self
            .last_prices
            .get(&market_id)
            .map(|value| (value.0, value.1));
        self.last_prices.insert(market_id, (yes_price, no_price));
        previous
            .map(|(old_yes, old_no)| (direction(yes_price, old_yes), direction(no_price, old_no)))
            .unwrap_or(("", ""))
    }
}

fn direction(current: Decimal, previous: Decimal) -> &'static str {
    if current > previous {
        "↑"
    } else if current < previous {
        "↓"
    } else {
        "−"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use polymarket_client_sdk::clob::ws::types::response::{BookUpdate, OrderBookLevel};
    use polymarket_client_sdk::types::U256;
    use rust_decimal_macros::dec;

    fn book(asset_id: u64, market: B256, timestamp: i64, ask: Decimal) -> BookUpdate {
        BookUpdate::builder()
            .asset_id(U256::from(asset_id))
            .market(market)
            .timestamp(timestamp)
            .bids(Vec::new())
            .asks(vec![OrderBookLevel::builder()
                .price(ask)
                .size(dec!(10))
                .build()])
            .build()
    }

    fn runner(market_id: B256, metrics: Arc<RuntimeMetrics>) -> WindowSessionRunner {
        let market = MarketInfo {
            market_id,
            slug: "btc-updown-5m-fixture".into(),
            yes_token_id: U256::from(1),
            no_token_id: U256::from(2),
            title: "BTC Up or Down".into(),
            end_date: Utc::now() + chrono::Duration::minutes(2),
            crypto_symbol: "btc".into(),
            neg_risk: false,
            outcome_prices: Some((dec!(0.5), dec!(0.5))),
        };
        WindowSessionRunner {
            markets: HashMap::from([(market_id, market)]),
            last_prices: DashMap::new(),
            shadow_observations: DashMap::new(),
            stable_candidates: DashMap::new(),
            detector: ArbitrageDetector::new(0.001),
            admission: OpportunityAdmissionService::new(dec!(0.99), dec!(0), dec!(0), 0, false),
            deduplicator: SignalDeduplicator::new(Duration::from_secs(3)),
            shadow_deduplicator: SignalDeduplicator::new(Duration::from_secs(3)),
            signal_confirmation_ms: 0,
            max_book_age_ms: 2_000,
            max_book_skew_ms: 1_000,
            metrics,
        }
    }

    #[test]
    fn direction_tracks_up_down_and_flat() {
        assert_eq!(direction(Decimal::ONE, Decimal::ZERO), "↑");
        assert_eq!(direction(Decimal::ZERO, Decimal::ONE), "↓");
        assert_eq!(direction(Decimal::ONE, Decimal::ONE), "−");
    }

    #[test]
    fn processes_safe_pair_and_suppresses_duplicate_opportunity() {
        let market_id = B256::repeat_byte(7);
        let metrics = Arc::new(RuntimeMetrics::default());
        let runner = runner(market_id, metrics.clone());
        let now_ms = Utc::now().timestamp_millis();
        let pair = OrderBookPair {
            yes_book: book(1, market_id, now_ms, dec!(0.45)),
            no_book: book(2, market_id, now_ms, dec!(0.50)),
            market_id,
        };
        let first = runner
            .process_pair(&pair, now_ms, false, true, false)
            .unwrap();
        assert!(first.opportunity.is_some());
        assert_eq!(first.market_display, "btc prediction market");
        let duplicate = runner
            .process_pair(&pair, now_ms, false, true, false)
            .unwrap();
        assert!(duplicate.opportunity.is_none());
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.orderbook_pairs, 2);
        assert_eq!(snapshot.opportunities_admitted, 1);
        assert_eq!(snapshot.duplicate_signals, 1);
    }

    #[test]
    fn confirms_continuous_edge_across_distinct_book_updates() {
        let market_id = B256::repeat_byte(10);
        let metrics = Arc::new(RuntimeMetrics::default());
        let mut runner = runner(market_id, metrics.clone());
        runner.signal_confirmation_ms = 100;
        let now_ms = Utc::now().timestamp_millis();
        let pair = OrderBookPair {
            yes_book: book(1, market_id, now_ms, dec!(0.45)),
            no_book: book(2, market_id, now_ms, dec!(0.50)),
            market_id,
        };

        let first = runner
            .process_pair(&pair, now_ms, false, true, false)
            .unwrap();
        assert!(first.opportunity.is_none());
        assert!(first.rest_shadow_candidate.is_some());
        let early_pair = OrderBookPair {
            yes_book: book(1, market_id, now_ms + 50, dec!(0.44)),
            no_book: book(2, market_id, now_ms + 50, dec!(0.50)),
            market_id,
        };
        let early = runner
            .process_pair(&early_pair, now_ms + 50, false, true, false)
            .unwrap();
        assert!(early.opportunity.is_none());
        let confirmed_pair = OrderBookPair {
            yes_book: book(1, market_id, now_ms + 100, dec!(0.43)),
            no_book: book(2, market_id, now_ms + 100, dec!(0.51)),
            market_id,
        };
        let confirmed = runner
            .process_pair(&confirmed_pair, now_ms + 100, false, true, false)
            .unwrap();
        assert!(confirmed.opportunity.is_some());
        assert_eq!(metrics.snapshot().stability_pending, 2);
    }

    #[test]
    fn reconnect_invalidation_requires_fresh_confirmation() {
        let market_id = B256::repeat_byte(11);
        let metrics = Arc::new(RuntimeMetrics::default());
        let mut runner = runner(market_id, metrics);
        runner.signal_confirmation_ms = 100;
        let now_ms = Utc::now().timestamp_millis();
        let first = OrderBookPair {
            yes_book: book(1, market_id, now_ms, dec!(0.45)),
            no_book: book(2, market_id, now_ms, dec!(0.50)),
            market_id,
        };
        assert!(runner
            .process_pair(&first, now_ms, false, true, false)
            .unwrap()
            .opportunity
            .is_none());
        runner.invalidate_transient_state();
        let after_reconnect = OrderBookPair {
            yes_book: book(1, market_id, now_ms + 200, dec!(0.45)),
            no_book: book(2, market_id, now_ms + 200, dec!(0.50)),
            market_id,
        };
        assert!(runner
            .process_pair(&after_reconnect, now_ms + 200, false, true, false)
            .unwrap()
            .opportunity
            .is_none());
    }

    #[test]
    fn rejects_stale_pair_before_counting_it() {
        let market_id = B256::repeat_byte(8);
        let metrics = Arc::new(RuntimeMetrics::default());
        let runner = runner(market_id, metrics.clone());
        let now_ms = Utc::now().timestamp_millis();
        let stale = OrderBookPair {
            yes_book: book(1, market_id, now_ms - 5_000, dec!(0.45)),
            no_book: book(2, market_id, now_ms - 5_000, dec!(0.50)),
            market_id,
        };
        assert!(runner
            .process_pair(&stale, now_ms, false, true, false)
            .is_err());
        assert_eq!(metrics.snapshot().orderbook_pairs, 0);
    }

    #[test]
    fn shadow_duration_tracks_continuous_state_across_quote_changes() {
        let market_id = B256::repeat_byte(9);
        let metrics = Arc::new(RuntimeMetrics::default());
        let mut runner = runner(market_id, metrics);
        runner.detector = ArbitrageDetector::with_min_depth(0.001, 10.0);
        let now_ms = Utc::now().timestamp_millis();

        let first = OrderBookPair {
            yes_book: book(1, market_id, now_ms, dec!(0.45)),
            no_book: book(2, market_id, now_ms, dec!(0.50)),
            market_id,
        };
        let first_tick = runner
            .process_pair(&first, now_ms, false, true, false)
            .unwrap();
        let first_shadow = first_tick.shadow_candidate.unwrap();
        assert_eq!(first_shadow.observed_duration_ms, 0);

        let changed = OrderBookPair {
            yes_book: book(1, market_id, now_ms + 300, dec!(0.44)),
            no_book: book(2, market_id, now_ms + 300, dec!(0.50)),
            market_id,
        };
        let changed_tick = runner
            .process_pair(&changed, now_ms + 300, false, true, false)
            .unwrap();
        let changed_shadow = changed_tick.shadow_candidate.unwrap();
        assert_eq!(changed_shadow.first_seen_at_ms, now_ms);
        assert_eq!(changed_shadow.observed_duration_ms, 300);

        let no_edge = OrderBookPair {
            yes_book: book(1, market_id, now_ms + 600, dec!(0.60)),
            no_book: book(2, market_id, now_ms + 600, dec!(0.50)),
            market_id,
        };
        assert!(runner
            .process_pair(&no_edge, now_ms + 600, false, true, false)
            .unwrap()
            .shadow_candidate
            .is_none());

        let restarted = OrderBookPair {
            yes_book: book(1, market_id, now_ms + 900, dec!(0.45)),
            no_book: book(2, market_id, now_ms + 900, dec!(0.50)),
            market_id,
        };
        let restarted_shadow = runner
            .process_pair(&restarted, now_ms + 900, false, true, false)
            .unwrap()
            .shadow_candidate
            .unwrap();
        assert_eq!(restarted_shadow.first_seen_at_ms, now_ms + 900);
        assert_eq!(restarted_shadow.observed_duration_ms, 0);
    }
}
