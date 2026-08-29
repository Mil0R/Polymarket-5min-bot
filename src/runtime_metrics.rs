use crate::late_resolution_winner_model::ModelRejection;
use crate::opportunity_admission::AdmissionRejection;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Mutex;

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct LateResolutionWinnerTradingAssetMetrics {
    pub asset: String,
    pub scans: u64,
    pub signals: u64,
    pub no_book: u64,
    pub no_signal: u64,
    pub evaluations: u64,
    pub attempts: u64,
    pub fills: u64,
    pub resolved: u64,
}

type LateResolutionWinnerTradingAssetMetricAccessor =
    fn(&LateResolutionWinnerTradingAssetMetrics) -> u64;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct MetricsSnapshot {
    pub orderbook_pairs: u64,
    pub opportunities_admitted: u64,
    pub opportunities_rejected: u64,
    pub depth_rejections: u64,
    pub stability_pending: u64,
    pub duplicate_signals: u64,
    pub quote_rejections: u64,
    pub risk_rejections: u64,
    pub balance_rejections: u64,
    pub trades_started: u64,
    pub trades_succeeded: u64,
    pub trades_failed: u64,
    pub stream_reconnects: u64,
    pub stream_connect_failures: u64,
    pub stream_recoveries: u64,
    pub sdk_market_ws_reconnects: u64,
    pub sdk_market_ws_recoveries: u64,
    pub sdk_market_ws_reconnecting_transitions: u64,
    pub sdk_market_ws_disconnected_transitions: u64,
    pub sdk_market_ws_downtime_ms_total: u64,
    pub sdk_market_ws_downtime_ms_max: u64,
    pub sdk_market_ws_last_book_at_ms: i64,
    pub sdk_user_ws_reconnects: u64,
    pub sdk_user_ws_recoveries: u64,
    pub rejection_execution_spread: u64,
    pub rejection_trading_paused: u64,
    pub rejection_connection_unhealthy: u64,
    pub rejection_unresolved_state: u64,
    pub rejection_price_threshold: u64,
    pub rejection_near_market_end: u64,
    pub paper_pairs: u64,
    pub paper_one_sided: u64,
    pub paper_net_pnl_microusd: i64,
    pub arbitrage_rest_shadow_samples: u64,
    pub arbitrage_rest_shadow_both_fillable: u64,
    pub arbitrage_rest_shadow_one_sided: u64,
    pub arbitrage_rest_shadow_neither_fillable: u64,
    pub arbitrage_rest_shadow_errors: u64,
    pub arbitrage_admission_shadow_samples: u64,
    pub arbitrage_admission_shadow_gross_passes: u64,
    pub arbitrage_admission_shadow_net_passes: u64,
    pub late_resolution_winner_trading_attempts: u64,
    pub late_resolution_winner_trading_scans: u64,
    pub late_resolution_winner_trading_signals: u64,
    pub late_resolution_winner_trading_no_book: u64,
    pub late_resolution_winner_trading_no_signal: u64,
    pub late_resolution_winner_trading_book_read_errors: u64,
    pub late_resolution_winner_trading_incomplete_books: u64,
    pub late_resolution_winner_trading_stale_books: u64,
    pub late_resolution_winner_trading_stale_ws_books: u64,
    pub late_resolution_winner_trading_stale_rest_books: u64,
    pub late_resolution_winner_trading_ws_missing_books: u64,
    pub late_resolution_winner_trading_rest_fallbacks: u64,
    pub late_resolution_winner_trading_rest_successes: u64,
    pub late_resolution_winner_trading_rest_empty_books: u64,
    pub late_resolution_winner_trading_rest_latency_ms_total: u64,
    pub late_resolution_winner_trading_rest_latency_ms_max: u64,
    pub late_resolution_winner_trading_fee_read_errors: u64,
    pub late_resolution_winner_trading_evaluations: u64,
    pub late_resolution_winner_trading_evaluations_resolved: u64,
    pub late_resolution_winner_trading_model_data_rejections: u64,
    pub late_resolution_winner_trading_rejection_no_price_history: u64,
    pub late_resolution_winner_trading_rejection_stale_price: u64,
    pub late_resolution_winner_trading_rejection_no_opening_reference: u64,
    pub late_resolution_winner_trading_rejection_invalid_price: u64,
    pub late_resolution_winner_trading_rejection_insufficient_observations: u64,
    pub late_resolution_winner_trading_model_market_rejections: u64,
    pub late_resolution_winner_trading_model_boundary_rejections: u64,
    pub late_resolution_winner_trading_model_ev_rejections: u64,
    pub late_resolution_winner_trading_shadow_ev_failures: u64,
    pub late_resolution_winner_trading_shadow_execution_attempts: u64,
    pub late_resolution_winner_trading_shadow_execution_fills: u64,
    pub late_resolution_winner_trading_shadow_execution_no_fills: u64,
    pub late_resolution_winner_trading_shadow_execution_failures: u64,
    pub late_resolution_winner_trading_shadow_awaiting_confirmation: u64,
    pub late_resolution_winner_trading_shadow_confirmed: u64,
    pub late_resolution_winner_trading_awaiting_confirmation: u64,
    pub late_resolution_winner_trading_fills: u64,
    pub late_resolution_winner_trading_pending: u64,
    pub late_resolution_winner_trading_resolved: u64,
    pub late_resolution_winner_trading_redeemed: u64,
    pub late_resolution_winner_trading_settlement_query_failures: u64,
    pub late_resolution_winner_trading_settlement_empty_results: u64,
    pub late_resolution_winner_trading_settlement_not_terminal: u64,
    pub late_resolution_winner_trading_est_pnl_microusd: i64,
    pub late_resolution_winner_trading_wins: u64,
    pub late_resolution_winner_trading_losses: u64,
    pub late_resolution_winner_trading_realized_pnl_microusd: i64,
    pub late_resolution_winner_trading_rtds_connections: u64,
    pub late_resolution_winner_trading_rtds_disconnects: u64,
    pub late_resolution_winner_trading_rtds_connect_failures: u64,
    pub late_resolution_winner_trading_rtds_messages: u64,
    pub late_resolution_winner_trading_rtds_last_observed_at_ms: i64,
    pub late_resolution_winner_trading_by_asset: Vec<LateResolutionWinnerTradingAssetMetrics>,
}

#[derive(Default)]
pub struct RuntimeMetrics {
    orderbook_pairs: AtomicU64,
    opportunities_admitted: AtomicU64,
    opportunities_rejected: AtomicU64,
    depth_rejections: AtomicU64,
    stability_pending: AtomicU64,
    duplicate_signals: AtomicU64,
    quote_rejections: AtomicU64,
    risk_rejections: AtomicU64,
    balance_rejections: AtomicU64,
    trades_started: AtomicU64,
    trades_succeeded: AtomicU64,
    trades_failed: AtomicU64,
    stream_reconnects: AtomicU64,
    stream_connect_failures: AtomicU64,
    stream_recoveries: AtomicU64,
    sdk_market_ws_reconnects: AtomicU64,
    sdk_market_ws_recoveries: AtomicU64,
    sdk_market_ws_reconnecting_transitions: AtomicU64,
    sdk_market_ws_disconnected_transitions: AtomicU64,
    sdk_market_ws_downtime_ms_total: AtomicU64,
    sdk_market_ws_downtime_ms_max: AtomicU64,
    sdk_market_ws_last_book_at_ms: AtomicI64,
    sdk_user_ws_reconnects: AtomicU64,
    sdk_user_ws_recoveries: AtomicU64,
    rejection_execution_spread: AtomicU64,
    rejection_trading_paused: AtomicU64,
    rejection_connection_unhealthy: AtomicU64,
    rejection_unresolved_state: AtomicU64,
    rejection_price_threshold: AtomicU64,
    rejection_near_market_end: AtomicU64,
    paper_pairs: AtomicU64,
    paper_one_sided: AtomicU64,
    paper_net_pnl_microusd: AtomicI64,
    arbitrage_rest_shadow_samples: AtomicU64,
    arbitrage_rest_shadow_both_fillable: AtomicU64,
    arbitrage_rest_shadow_one_sided: AtomicU64,
    arbitrage_rest_shadow_neither_fillable: AtomicU64,
    arbitrage_rest_shadow_errors: AtomicU64,
    arbitrage_admission_shadow_samples: AtomicU64,
    arbitrage_admission_shadow_gross_passes: AtomicU64,
    arbitrage_admission_shadow_net_passes: AtomicU64,
    late_resolution_winner_trading_attempts: AtomicU64,
    late_resolution_winner_trading_scans: AtomicU64,
    late_resolution_winner_trading_signals: AtomicU64,
    late_resolution_winner_trading_no_book: AtomicU64,
    late_resolution_winner_trading_no_signal: AtomicU64,
    late_resolution_winner_trading_book_read_errors: AtomicU64,
    late_resolution_winner_trading_incomplete_books: AtomicU64,
    late_resolution_winner_trading_stale_books: AtomicU64,
    late_resolution_winner_trading_stale_ws_books: AtomicU64,
    late_resolution_winner_trading_stale_rest_books: AtomicU64,
    late_resolution_winner_trading_ws_missing_books: AtomicU64,
    late_resolution_winner_trading_rest_fallbacks: AtomicU64,
    late_resolution_winner_trading_rest_successes: AtomicU64,
    late_resolution_winner_trading_rest_empty_books: AtomicU64,
    late_resolution_winner_trading_rest_latency_ms_total: AtomicU64,
    late_resolution_winner_trading_rest_latency_ms_max: AtomicU64,
    late_resolution_winner_trading_fee_read_errors: AtomicU64,
    late_resolution_winner_trading_evaluations: AtomicU64,
    late_resolution_winner_trading_evaluations_resolved: AtomicU64,
    late_resolution_winner_trading_model_data_rejections: AtomicU64,
    late_resolution_winner_trading_rejection_no_price_history: AtomicU64,
    late_resolution_winner_trading_rejection_stale_price: AtomicU64,
    late_resolution_winner_trading_rejection_no_opening_reference: AtomicU64,
    late_resolution_winner_trading_rejection_invalid_price: AtomicU64,
    late_resolution_winner_trading_rejection_insufficient_observations: AtomicU64,
    late_resolution_winner_trading_model_market_rejections: AtomicU64,
    late_resolution_winner_trading_model_boundary_rejections: AtomicU64,
    late_resolution_winner_trading_model_ev_rejections: AtomicU64,
    late_resolution_winner_trading_shadow_ev_failures: AtomicU64,
    late_resolution_winner_trading_shadow_execution_attempts: AtomicU64,
    late_resolution_winner_trading_shadow_execution_fills: AtomicU64,
    late_resolution_winner_trading_shadow_execution_no_fills: AtomicU64,
    late_resolution_winner_trading_shadow_execution_failures: AtomicU64,
    late_resolution_winner_trading_shadow_awaiting_confirmation: AtomicU64,
    late_resolution_winner_trading_shadow_confirmed: AtomicU64,
    late_resolution_winner_trading_awaiting_confirmation: AtomicU64,
    late_resolution_winner_trading_fills: AtomicU64,
    late_resolution_winner_trading_pending: AtomicU64,
    late_resolution_winner_trading_resolved: AtomicU64,
    late_resolution_winner_trading_redeemed: AtomicU64,
    late_resolution_winner_trading_settlement_query_failures: AtomicU64,
    late_resolution_winner_trading_settlement_empty_results: AtomicU64,
    late_resolution_winner_trading_settlement_not_terminal: AtomicU64,
    late_resolution_winner_trading_est_pnl_microusd: AtomicI64,
    late_resolution_winner_trading_wins: AtomicU64,
    late_resolution_winner_trading_losses: AtomicU64,
    late_resolution_winner_trading_realized_pnl_microusd: AtomicI64,
    late_resolution_winner_trading_rtds_connections: AtomicU64,
    late_resolution_winner_trading_rtds_disconnects: AtomicU64,
    late_resolution_winner_trading_rtds_connect_failures: AtomicU64,
    late_resolution_winner_trading_rtds_messages: AtomicU64,
    late_resolution_winner_trading_rtds_last_observed_at_ms: AtomicI64,
    late_resolution_winner_trading_by_asset:
        Mutex<HashMap<String, LateResolutionWinnerTradingAssetMetrics>>,
}

macro_rules! incrementer {
    ($name:ident, $field:ident) => {
        pub fn $name(&self) {
            self.$field.fetch_add(1, Ordering::Relaxed);
        }
    };
}

impl RuntimeMetrics {
    incrementer!(record_orderbook_pair, orderbook_pairs);
    incrementer!(record_admitted, opportunities_admitted);
    incrementer!(record_depth_rejection, depth_rejections);
    incrementer!(record_stability_pending, stability_pending);
    incrementer!(record_duplicate_signal, duplicate_signals);
    incrementer!(record_quote_rejection, quote_rejections);
    incrementer!(record_risk_rejection, risk_rejections);
    incrementer!(record_balance_rejection, balance_rejections);

    pub fn record_arbitrage_rest_shadow(
        &self,
        yes_filled: rust_decimal::Decimal,
        no_filled: rust_decimal::Decimal,
        has_error: bool,
    ) {
        self.arbitrage_rest_shadow_samples
            .fetch_add(1, Ordering::Relaxed);
        if has_error {
            self.arbitrage_rest_shadow_errors
                .fetch_add(1, Ordering::Relaxed);
        }
        let counter = match (
            yes_filled > rust_decimal::Decimal::ZERO,
            no_filled > rust_decimal::Decimal::ZERO,
        ) {
            (true, true) => &self.arbitrage_rest_shadow_both_fillable,
            (true, false) | (false, true) => &self.arbitrage_rest_shadow_one_sided,
            (false, false) => &self.arbitrage_rest_shadow_neither_fillable,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_arbitrage_admission_shadow(&self, gross_passes: bool, net_passes: bool) {
        self.arbitrage_admission_shadow_samples
            .fetch_add(1, Ordering::Relaxed);
        if gross_passes {
            self.arbitrage_admission_shadow_gross_passes
                .fetch_add(1, Ordering::Relaxed);
        }
        if net_passes {
            self.arbitrage_admission_shadow_net_passes
                .fetch_add(1, Ordering::Relaxed);
        }
    }
    incrementer!(record_trade_started, trades_started);
    incrementer!(record_trade_succeeded, trades_succeeded);
    incrementer!(record_trade_failed, trades_failed);
    incrementer!(record_stream_reconnect, stream_reconnects);
    incrementer!(record_stream_connect_failure, stream_connect_failures);
    incrementer!(record_stream_recovery, stream_recoveries);
    incrementer!(record_sdk_market_ws_reconnect, sdk_market_ws_reconnects);
    incrementer!(record_sdk_market_ws_recovery, sdk_market_ws_recoveries);
    incrementer!(
        record_sdk_market_ws_reconnecting_transition,
        sdk_market_ws_reconnecting_transitions
    );
    incrementer!(
        record_sdk_market_ws_disconnected_transition,
        sdk_market_ws_disconnected_transitions
    );
    incrementer!(record_sdk_user_ws_reconnect, sdk_user_ws_reconnects);
    incrementer!(record_sdk_user_ws_recovery, sdk_user_ws_recoveries);

    pub fn record_sdk_market_ws_downtime(&self, downtime_ms: u64) {
        self.sdk_market_ws_downtime_ms_total
            .fetch_add(downtime_ms, Ordering::Relaxed);
        self.sdk_market_ws_downtime_ms_max
            .fetch_max(downtime_ms, Ordering::Relaxed);
    }
    incrementer!(
        record_late_resolution_winner_trading_attempt,
        late_resolution_winner_trading_attempts
    );
    incrementer!(
        record_late_resolution_winner_trading_scan,
        late_resolution_winner_trading_scans
    );
    incrementer!(
        record_late_resolution_winner_trading_signal,
        late_resolution_winner_trading_signals
    );
    incrementer!(
        record_late_resolution_winner_trading_no_book,
        late_resolution_winner_trading_no_book
    );
    incrementer!(
        record_late_resolution_winner_trading_no_signal,
        late_resolution_winner_trading_no_signal
    );
    incrementer!(
        record_late_resolution_winner_trading_book_read_error,
        late_resolution_winner_trading_book_read_errors
    );
    incrementer!(
        record_late_resolution_winner_trading_incomplete_book,
        late_resolution_winner_trading_incomplete_books
    );
    incrementer!(
        record_late_resolution_winner_trading_stale_book,
        late_resolution_winner_trading_stale_books
    );
    incrementer!(
        record_late_resolution_winner_trading_stale_ws_book,
        late_resolution_winner_trading_stale_ws_books
    );
    incrementer!(
        record_late_resolution_winner_trading_stale_rest_book,
        late_resolution_winner_trading_stale_rest_books
    );
    incrementer!(
        record_late_resolution_winner_trading_rest_fallback,
        late_resolution_winner_trading_rest_fallbacks
    );
    incrementer!(
        record_late_resolution_winner_trading_ws_missing_book,
        late_resolution_winner_trading_ws_missing_books
    );
    incrementer!(
        record_late_resolution_winner_trading_fee_read_error,
        late_resolution_winner_trading_fee_read_errors
    );

    pub fn record_late_resolution_winner_trading_rest_result(&self, latency_ms: u64, found: bool) {
        let counter = if found {
            &self.late_resolution_winner_trading_rest_successes
        } else {
            &self.late_resolution_winner_trading_rest_empty_books
        };
        counter.fetch_add(1, Ordering::Relaxed);
        self.late_resolution_winner_trading_rest_latency_ms_total
            .fetch_add(latency_ms, Ordering::Relaxed);
        self.late_resolution_winner_trading_rest_latency_ms_max
            .fetch_max(latency_ms, Ordering::Relaxed);
    }
    incrementer!(
        record_late_resolution_winner_trading_evaluation,
        late_resolution_winner_trading_evaluations
    );
    incrementer!(
        record_late_resolution_winner_trading_rtds_connection,
        late_resolution_winner_trading_rtds_connections
    );
    incrementer!(
        record_late_resolution_winner_trading_rtds_disconnect,
        late_resolution_winner_trading_rtds_disconnects
    );
    incrementer!(
        record_late_resolution_winner_trading_rtds_connect_failure,
        late_resolution_winner_trading_rtds_connect_failures
    );
    incrementer!(
        record_late_resolution_winner_trading_awaiting_confirmation,
        late_resolution_winner_trading_awaiting_confirmation
    );
    incrementer!(
        record_late_resolution_winner_trading_shadow_ev_failure,
        late_resolution_winner_trading_shadow_ev_failures
    );
    incrementer!(
        record_late_resolution_winner_trading_shadow_execution_attempt,
        late_resolution_winner_trading_shadow_execution_attempts
    );
    incrementer!(
        record_late_resolution_winner_trading_shadow_execution_fill,
        late_resolution_winner_trading_shadow_execution_fills
    );
    incrementer!(
        record_late_resolution_winner_trading_shadow_execution_no_fill,
        late_resolution_winner_trading_shadow_execution_no_fills
    );
    incrementer!(
        record_late_resolution_winner_trading_shadow_execution_failure,
        late_resolution_winner_trading_shadow_execution_failures
    );
    incrementer!(
        record_late_resolution_winner_trading_shadow_awaiting_confirmation,
        late_resolution_winner_trading_shadow_awaiting_confirmation
    );
    incrementer!(
        record_late_resolution_winner_trading_shadow_confirmed,
        late_resolution_winner_trading_shadow_confirmed
    );
    incrementer!(
        record_late_resolution_winner_trading_fill,
        late_resolution_winner_trading_fills
    );
    incrementer!(
        record_late_resolution_winner_trading_resolved,
        late_resolution_winner_trading_resolved
    );
    incrementer!(
        record_late_resolution_winner_trading_redeemed,
        late_resolution_winner_trading_redeemed
    );
    incrementer!(
        record_late_resolution_winner_trading_settlement_query_failure,
        late_resolution_winner_trading_settlement_query_failures
    );
    incrementer!(
        record_late_resolution_winner_trading_settlement_empty_result,
        late_resolution_winner_trading_settlement_empty_results
    );
    incrementer!(
        record_late_resolution_winner_trading_settlement_not_terminal,
        late_resolution_winner_trading_settlement_not_terminal
    );

    pub fn set_late_resolution_winner_trading_pending(&self, count: usize) {
        self.late_resolution_winner_trading_pending
            .store(count.try_into().unwrap_or(u64::MAX), Ordering::Relaxed);
    }

    fn record_winner_asset(
        &self,
        asset: &str,
        update: impl FnOnce(&mut LateResolutionWinnerTradingAssetMetrics),
    ) {
        let key = asset.trim().to_ascii_lowercase();
        let key = if key.is_empty() {
            "unknown".into()
        } else {
            key
        };
        let mut metrics = self
            .late_resolution_winner_trading_by_asset
            .lock()
            .expect("Late-Resolution Winner Trading asset metrics lock poisoned");
        let entry =
            metrics
                .entry(key.clone())
                .or_insert_with(|| LateResolutionWinnerTradingAssetMetrics {
                    asset: key,
                    ..LateResolutionWinnerTradingAssetMetrics::default()
                });
        update(entry);
    }

    pub fn record_late_resolution_winner_trading_asset_scan(&self, asset: &str) {
        self.record_winner_asset(asset, |metrics| metrics.scans += 1);
    }

    pub fn record_late_resolution_winner_trading_asset_signal(&self, asset: &str) {
        self.record_winner_asset(asset, |metrics| metrics.signals += 1);
    }

    pub fn record_late_resolution_winner_trading_asset_no_book(&self, asset: &str) {
        self.record_winner_asset(asset, |metrics| metrics.no_book += 1);
    }

    pub fn record_late_resolution_winner_trading_asset_no_signal(&self, asset: &str) {
        self.record_winner_asset(asset, |metrics| metrics.no_signal += 1);
    }

    pub fn record_late_resolution_winner_trading_asset_evaluation(&self, asset: &str) {
        self.record_winner_asset(asset, |metrics| metrics.evaluations += 1);
    }

    pub fn record_late_resolution_winner_trading_asset_attempt(&self, asset: &str) {
        self.record_winner_asset(asset, |metrics| metrics.attempts += 1);
    }

    pub fn record_late_resolution_winner_trading_asset_fill(&self, asset: &str) {
        self.record_winner_asset(asset, |metrics| metrics.fills += 1);
    }

    pub fn record_late_resolution_winner_trading_asset_resolved(&self, asset: &str) {
        self.record_winner_asset(asset, |metrics| metrics.resolved += 1);
    }

    pub fn record_late_resolution_winner_model_rejection(&self, rejection: ModelRejection) {
        let detail = match rejection {
            ModelRejection::NoPriceHistory => {
                Some(&self.late_resolution_winner_trading_rejection_no_price_history)
            }
            ModelRejection::StalePrice => {
                Some(&self.late_resolution_winner_trading_rejection_stale_price)
            }
            ModelRejection::NoOpeningReference => {
                Some(&self.late_resolution_winner_trading_rejection_no_opening_reference)
            }
            ModelRejection::InvalidPrice => {
                Some(&self.late_resolution_winner_trading_rejection_invalid_price)
            }
            ModelRejection::InsufficientObservations => {
                Some(&self.late_resolution_winner_trading_rejection_insufficient_observations)
            }
            _ => None,
        };
        if let Some(detail) = detail {
            detail.fetch_add(1, Ordering::Relaxed);
        }
        let counter = match rejection {
            ModelRejection::NoPriceHistory
            | ModelRejection::StalePrice
            | ModelRejection::NoOpeningReference
            | ModelRejection::InvalidPrice
            | ModelRejection::InsufficientObservations => {
                &self.late_resolution_winner_trading_model_data_rejections
            }
            ModelRejection::SpreadTooWide | ModelRejection::InsufficientDepth => {
                &self.late_resolution_winner_trading_model_market_rejections
            }
            ModelRejection::ReversalBoundary => {
                &self.late_resolution_winner_trading_model_boundary_rejections
            }
            ModelRejection::NetEvTooLow => &self.late_resolution_winner_trading_model_ev_rejections,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_late_resolution_winner_trading_rtds_observation(&self, observed_at_ms: i64) {
        self.late_resolution_winner_trading_rtds_messages
            .fetch_add(1, Ordering::Relaxed);
        self.late_resolution_winner_trading_rtds_last_observed_at_ms
            .fetch_max(observed_at_ms, Ordering::Relaxed);
    }

    pub fn record_market_ws_book(&self, observed_at_ms: i64) {
        self.sdk_market_ws_last_book_at_ms
            .fetch_max(observed_at_ms, Ordering::Relaxed);
    }

    pub fn record_late_resolution_winner_trading_evaluations_resolved(&self, count: usize) {
        self.late_resolution_winner_trading_evaluations_resolved
            .fetch_add(count.try_into().unwrap_or(u64::MAX), Ordering::Relaxed);
    }

    /// Estimate late-resolution-winner-trading profit: filled shares held to settlement (pays 1.00).
    pub fn record_late_resolution_winner_trading_pnl(
        &self,
        filled: rust_decimal::Decimal,
        price: rust_decimal::Decimal,
    ) {
        use rust_decimal::prelude::ToPrimitive;
        use rust_decimal_macros::dec;
        let pnl = filled * (dec!(1) - price);
        let microusd = (pnl * dec!(1000000)).round().to_i64().unwrap_or(0);
        self.late_resolution_winner_trading_est_pnl_microusd
            .fetch_add(microusd, Ordering::Relaxed);
    }

    /// Record the REALIZED settlement result of a late-resolution-winner-trading paper position.
    pub fn record_late_resolution_winner_trading_settlement(&self, pnl: rust_decimal::Decimal) {
        use rust_decimal::prelude::ToPrimitive;
        use rust_decimal_macros::dec;
        if pnl > dec!(0) {
            self.late_resolution_winner_trading_wins
                .fetch_add(1, Ordering::Relaxed);
        } else {
            self.late_resolution_winner_trading_losses
                .fetch_add(1, Ordering::Relaxed);
        }
        let microusd = (pnl * dec!(1000000)).round().to_i64().unwrap_or(0);
        self.late_resolution_winner_trading_realized_pnl_microusd
            .fetch_add(microusd, Ordering::Relaxed);
    }

    pub fn record_paper_result(
        &self,
        yes_filled: rust_decimal::Decimal,
        no_filled: rust_decimal::Decimal,
        yes_price: rust_decimal::Decimal,
        no_price: rust_decimal::Decimal,
    ) {
        use rust_decimal::prelude::ToPrimitive;
        use rust_decimal_macros::dec;
        self.paper_pairs.fetch_add(1, Ordering::Relaxed);
        let paired = yes_filled.min(no_filled);
        if yes_filled != no_filled {
            self.paper_one_sided.fetch_add(1, Ordering::Relaxed);
        }
        let pnl = paired * (dec!(1) - yes_price - no_price)
            - (yes_filled - paired).max(dec!(0)) * yes_price
            - (no_filled - paired).max(dec!(0)) * no_price;
        let microusd = (pnl * dec!(1000000)).round().to_i64().unwrap_or(0);
        self.paper_net_pnl_microusd
            .fetch_add(microusd, Ordering::Relaxed);
    }

    pub fn record_admission_rejection(&self, reason: AdmissionRejection) {
        self.opportunities_rejected.fetch_add(1, Ordering::Relaxed);
        let counter = match reason {
            AdmissionRejection::ExecutionSpread => &self.rejection_execution_spread,
            AdmissionRejection::TradingPaused => &self.rejection_trading_paused,
            AdmissionRejection::ConnectionUnhealthy => &self.rejection_connection_unhealthy,
            AdmissionRejection::UnresolvedState => &self.rejection_unresolved_state,
            AdmissionRejection::YesPriceTooLow | AdmissionRejection::NoPriceTooLow => {
                &self.rejection_price_threshold
            }
            AdmissionRejection::NearMarketEnd => &self.rejection_near_market_end,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> MetricsSnapshot {
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        let mut late_resolution_winner_trading_by_asset: Vec<_> = self
            .late_resolution_winner_trading_by_asset
            .lock()
            .expect("Late-Resolution Winner Trading asset metrics lock poisoned")
            .values()
            .cloned()
            .collect();
        late_resolution_winner_trading_by_asset.sort_by(|left, right| left.asset.cmp(&right.asset));
        MetricsSnapshot {
            orderbook_pairs: load(&self.orderbook_pairs),
            opportunities_admitted: load(&self.opportunities_admitted),
            opportunities_rejected: load(&self.opportunities_rejected),
            depth_rejections: load(&self.depth_rejections),
            stability_pending: load(&self.stability_pending),
            duplicate_signals: load(&self.duplicate_signals),
            quote_rejections: load(&self.quote_rejections),
            risk_rejections: load(&self.risk_rejections),
            balance_rejections: load(&self.balance_rejections),
            trades_started: load(&self.trades_started),
            trades_succeeded: load(&self.trades_succeeded),
            trades_failed: load(&self.trades_failed),
            stream_reconnects: load(&self.stream_reconnects),
            stream_connect_failures: load(&self.stream_connect_failures),
            stream_recoveries: load(&self.stream_recoveries),
            sdk_market_ws_reconnects: load(&self.sdk_market_ws_reconnects),
            sdk_market_ws_recoveries: load(&self.sdk_market_ws_recoveries),
            sdk_market_ws_reconnecting_transitions: load(
                &self.sdk_market_ws_reconnecting_transitions,
            ),
            sdk_market_ws_disconnected_transitions: load(
                &self.sdk_market_ws_disconnected_transitions,
            ),
            sdk_market_ws_downtime_ms_total: load(&self.sdk_market_ws_downtime_ms_total),
            sdk_market_ws_downtime_ms_max: load(&self.sdk_market_ws_downtime_ms_max),
            sdk_market_ws_last_book_at_ms: self
                .sdk_market_ws_last_book_at_ms
                .load(Ordering::Relaxed),
            sdk_user_ws_reconnects: load(&self.sdk_user_ws_reconnects),
            sdk_user_ws_recoveries: load(&self.sdk_user_ws_recoveries),
            rejection_execution_spread: load(&self.rejection_execution_spread),
            rejection_trading_paused: load(&self.rejection_trading_paused),
            rejection_connection_unhealthy: load(&self.rejection_connection_unhealthy),
            rejection_unresolved_state: load(&self.rejection_unresolved_state),
            rejection_price_threshold: load(&self.rejection_price_threshold),
            rejection_near_market_end: load(&self.rejection_near_market_end),
            paper_pairs: load(&self.paper_pairs),
            paper_one_sided: load(&self.paper_one_sided),
            paper_net_pnl_microusd: self.paper_net_pnl_microusd.load(Ordering::Relaxed),
            arbitrage_rest_shadow_samples: load(&self.arbitrage_rest_shadow_samples),
            arbitrage_rest_shadow_both_fillable: load(&self.arbitrage_rest_shadow_both_fillable),
            arbitrage_rest_shadow_one_sided: load(&self.arbitrage_rest_shadow_one_sided),
            arbitrage_rest_shadow_neither_fillable: load(
                &self.arbitrage_rest_shadow_neither_fillable,
            ),
            arbitrage_rest_shadow_errors: load(&self.arbitrage_rest_shadow_errors),
            arbitrage_admission_shadow_samples: load(&self.arbitrage_admission_shadow_samples),
            arbitrage_admission_shadow_gross_passes: load(
                &self.arbitrage_admission_shadow_gross_passes,
            ),
            arbitrage_admission_shadow_net_passes: load(
                &self.arbitrage_admission_shadow_net_passes,
            ),
            late_resolution_winner_trading_attempts: load(
                &self.late_resolution_winner_trading_attempts,
            ),
            late_resolution_winner_trading_scans: load(&self.late_resolution_winner_trading_scans),
            late_resolution_winner_trading_signals: load(
                &self.late_resolution_winner_trading_signals,
            ),
            late_resolution_winner_trading_no_book: load(
                &self.late_resolution_winner_trading_no_book,
            ),
            late_resolution_winner_trading_no_signal: load(
                &self.late_resolution_winner_trading_no_signal,
            ),
            late_resolution_winner_trading_book_read_errors: load(
                &self.late_resolution_winner_trading_book_read_errors,
            ),
            late_resolution_winner_trading_incomplete_books: load(
                &self.late_resolution_winner_trading_incomplete_books,
            ),
            late_resolution_winner_trading_stale_books: load(
                &self.late_resolution_winner_trading_stale_books,
            ),
            late_resolution_winner_trading_stale_ws_books: load(
                &self.late_resolution_winner_trading_stale_ws_books,
            ),
            late_resolution_winner_trading_stale_rest_books: load(
                &self.late_resolution_winner_trading_stale_rest_books,
            ),
            late_resolution_winner_trading_ws_missing_books: load(
                &self.late_resolution_winner_trading_ws_missing_books,
            ),
            late_resolution_winner_trading_rest_fallbacks: load(
                &self.late_resolution_winner_trading_rest_fallbacks,
            ),
            late_resolution_winner_trading_rest_successes: load(
                &self.late_resolution_winner_trading_rest_successes,
            ),
            late_resolution_winner_trading_rest_empty_books: load(
                &self.late_resolution_winner_trading_rest_empty_books,
            ),
            late_resolution_winner_trading_rest_latency_ms_total: load(
                &self.late_resolution_winner_trading_rest_latency_ms_total,
            ),
            late_resolution_winner_trading_rest_latency_ms_max: load(
                &self.late_resolution_winner_trading_rest_latency_ms_max,
            ),
            late_resolution_winner_trading_fee_read_errors: load(
                &self.late_resolution_winner_trading_fee_read_errors,
            ),
            late_resolution_winner_trading_evaluations: load(
                &self.late_resolution_winner_trading_evaluations,
            ),
            late_resolution_winner_trading_evaluations_resolved: load(
                &self.late_resolution_winner_trading_evaluations_resolved,
            ),
            late_resolution_winner_trading_model_data_rejections: load(
                &self.late_resolution_winner_trading_model_data_rejections,
            ),
            late_resolution_winner_trading_rejection_no_price_history: load(
                &self.late_resolution_winner_trading_rejection_no_price_history,
            ),
            late_resolution_winner_trading_rejection_stale_price: load(
                &self.late_resolution_winner_trading_rejection_stale_price,
            ),
            late_resolution_winner_trading_rejection_no_opening_reference: load(
                &self.late_resolution_winner_trading_rejection_no_opening_reference,
            ),
            late_resolution_winner_trading_rejection_invalid_price: load(
                &self.late_resolution_winner_trading_rejection_invalid_price,
            ),
            late_resolution_winner_trading_rejection_insufficient_observations: load(
                &self.late_resolution_winner_trading_rejection_insufficient_observations,
            ),
            late_resolution_winner_trading_model_market_rejections: load(
                &self.late_resolution_winner_trading_model_market_rejections,
            ),
            late_resolution_winner_trading_model_boundary_rejections: load(
                &self.late_resolution_winner_trading_model_boundary_rejections,
            ),
            late_resolution_winner_trading_model_ev_rejections: load(
                &self.late_resolution_winner_trading_model_ev_rejections,
            ),
            late_resolution_winner_trading_shadow_ev_failures: load(
                &self.late_resolution_winner_trading_shadow_ev_failures,
            ),
            late_resolution_winner_trading_shadow_execution_attempts: load(
                &self.late_resolution_winner_trading_shadow_execution_attempts,
            ),
            late_resolution_winner_trading_shadow_execution_fills: load(
                &self.late_resolution_winner_trading_shadow_execution_fills,
            ),
            late_resolution_winner_trading_shadow_execution_no_fills: load(
                &self.late_resolution_winner_trading_shadow_execution_no_fills,
            ),
            late_resolution_winner_trading_shadow_execution_failures: load(
                &self.late_resolution_winner_trading_shadow_execution_failures,
            ),
            late_resolution_winner_trading_shadow_awaiting_confirmation: load(
                &self.late_resolution_winner_trading_shadow_awaiting_confirmation,
            ),
            late_resolution_winner_trading_shadow_confirmed: load(
                &self.late_resolution_winner_trading_shadow_confirmed,
            ),
            late_resolution_winner_trading_awaiting_confirmation: load(
                &self.late_resolution_winner_trading_awaiting_confirmation,
            ),
            late_resolution_winner_trading_fills: load(&self.late_resolution_winner_trading_fills),
            late_resolution_winner_trading_pending: load(
                &self.late_resolution_winner_trading_pending,
            ),
            late_resolution_winner_trading_resolved: load(
                &self.late_resolution_winner_trading_resolved,
            ),
            late_resolution_winner_trading_redeemed: load(
                &self.late_resolution_winner_trading_redeemed,
            ),
            late_resolution_winner_trading_settlement_query_failures: load(
                &self.late_resolution_winner_trading_settlement_query_failures,
            ),
            late_resolution_winner_trading_settlement_empty_results: load(
                &self.late_resolution_winner_trading_settlement_empty_results,
            ),
            late_resolution_winner_trading_settlement_not_terminal: load(
                &self.late_resolution_winner_trading_settlement_not_terminal,
            ),
            late_resolution_winner_trading_est_pnl_microusd: self
                .late_resolution_winner_trading_est_pnl_microusd
                .load(Ordering::Relaxed),
            late_resolution_winner_trading_wins: load(&self.late_resolution_winner_trading_wins),
            late_resolution_winner_trading_losses: load(
                &self.late_resolution_winner_trading_losses,
            ),
            late_resolution_winner_trading_realized_pnl_microusd: self
                .late_resolution_winner_trading_realized_pnl_microusd
                .load(Ordering::Relaxed),
            late_resolution_winner_trading_rtds_connections: load(
                &self.late_resolution_winner_trading_rtds_connections,
            ),
            late_resolution_winner_trading_rtds_disconnects: load(
                &self.late_resolution_winner_trading_rtds_disconnects,
            ),
            late_resolution_winner_trading_rtds_connect_failures: load(
                &self.late_resolution_winner_trading_rtds_connect_failures,
            ),
            late_resolution_winner_trading_rtds_messages: load(
                &self.late_resolution_winner_trading_rtds_messages,
            ),
            late_resolution_winner_trading_rtds_last_observed_at_ms: self
                .late_resolution_winner_trading_rtds_last_observed_at_ms
                .load(Ordering::Relaxed),
            late_resolution_winner_trading_by_asset,
        }
    }

    pub fn prometheus(&self) -> String {
        let snapshot = self.snapshot();
        let values = [
            ("orderbook_pairs_total", snapshot.orderbook_pairs),
            (
                "opportunities_admitted_total",
                snapshot.opportunities_admitted,
            ),
            (
                "opportunities_rejected_total",
                snapshot.opportunities_rejected,
            ),
            ("depth_rejections_total", snapshot.depth_rejections),
            ("stability_pending_total", snapshot.stability_pending),
            ("duplicate_signals_total", snapshot.duplicate_signals),
            ("quote_rejections_total", snapshot.quote_rejections),
            ("risk_rejections_total", snapshot.risk_rejections),
            ("balance_rejections_total", snapshot.balance_rejections),
            ("trades_started_total", snapshot.trades_started),
            ("trades_succeeded_total", snapshot.trades_succeeded),
            ("trades_failed_total", snapshot.trades_failed),
            ("stream_reconnects_total", snapshot.stream_reconnects),
            (
                "stream_connect_failures_total",
                snapshot.stream_connect_failures,
            ),
            ("stream_recoveries_total", snapshot.stream_recoveries),
            (
                "sdk_market_ws_reconnects_total",
                snapshot.sdk_market_ws_reconnects,
            ),
            (
                "sdk_market_ws_recoveries_total",
                snapshot.sdk_market_ws_recoveries,
            ),
            (
                "sdk_market_ws_reconnecting_transitions_total",
                snapshot.sdk_market_ws_reconnecting_transitions,
            ),
            (
                "sdk_market_ws_disconnected_transitions_total",
                snapshot.sdk_market_ws_disconnected_transitions,
            ),
            (
                "sdk_market_ws_downtime_ms_total",
                snapshot.sdk_market_ws_downtime_ms_total,
            ),
            (
                "sdk_user_ws_reconnects_total",
                snapshot.sdk_user_ws_reconnects,
            ),
            (
                "sdk_user_ws_recoveries_total",
                snapshot.sdk_user_ws_recoveries,
            ),
            (
                "rejection_execution_spread_total",
                snapshot.rejection_execution_spread,
            ),
            (
                "rejection_trading_paused_total",
                snapshot.rejection_trading_paused,
            ),
            (
                "rejection_connection_unhealthy_total",
                snapshot.rejection_connection_unhealthy,
            ),
            (
                "rejection_unresolved_state_total",
                snapshot.rejection_unresolved_state,
            ),
            (
                "rejection_price_threshold_total",
                snapshot.rejection_price_threshold,
            ),
            (
                "rejection_near_market_end_total",
                snapshot.rejection_near_market_end,
            ),
            ("paper_pairs_total", snapshot.paper_pairs),
            ("paper_one_sided_total", snapshot.paper_one_sided),
            (
                "arbitrage_rest_shadow_samples_total",
                snapshot.arbitrage_rest_shadow_samples,
            ),
            (
                "arbitrage_rest_shadow_both_fillable_total",
                snapshot.arbitrage_rest_shadow_both_fillable,
            ),
            (
                "arbitrage_rest_shadow_one_sided_total",
                snapshot.arbitrage_rest_shadow_one_sided,
            ),
            (
                "arbitrage_rest_shadow_neither_fillable_total",
                snapshot.arbitrage_rest_shadow_neither_fillable,
            ),
            (
                "arbitrage_rest_shadow_errors_total",
                snapshot.arbitrage_rest_shadow_errors,
            ),
            (
                "arbitrage_admission_shadow_samples_total",
                snapshot.arbitrage_admission_shadow_samples,
            ),
            (
                "arbitrage_admission_shadow_gross_passes_total",
                snapshot.arbitrage_admission_shadow_gross_passes,
            ),
            (
                "arbitrage_admission_shadow_net_passes_total",
                snapshot.arbitrage_admission_shadow_net_passes,
            ),
            (
                "late_resolution_winner_trading_attempts_total",
                snapshot.late_resolution_winner_trading_attempts,
            ),
            (
                "late_resolution_winner_trading_scans_total",
                snapshot.late_resolution_winner_trading_scans,
            ),
            (
                "late_resolution_winner_trading_signals_total",
                snapshot.late_resolution_winner_trading_signals,
            ),
            (
                "late_resolution_winner_trading_no_book_total",
                snapshot.late_resolution_winner_trading_no_book,
            ),
            (
                "late_resolution_winner_trading_no_signal_total",
                snapshot.late_resolution_winner_trading_no_signal,
            ),
            (
                "late_resolution_winner_trading_book_read_errors_total",
                snapshot.late_resolution_winner_trading_book_read_errors,
            ),
            (
                "late_resolution_winner_trading_incomplete_books_total",
                snapshot.late_resolution_winner_trading_incomplete_books,
            ),
            (
                "late_resolution_winner_trading_stale_books_total",
                snapshot.late_resolution_winner_trading_stale_books,
            ),
            (
                "late_resolution_winner_trading_stale_ws_books_total",
                snapshot.late_resolution_winner_trading_stale_ws_books,
            ),
            (
                "late_resolution_winner_trading_stale_rest_books_total",
                snapshot.late_resolution_winner_trading_stale_rest_books,
            ),
            (
                "late_resolution_winner_trading_ws_missing_books_total",
                snapshot.late_resolution_winner_trading_ws_missing_books,
            ),
            (
                "late_resolution_winner_trading_rest_fallbacks_total",
                snapshot.late_resolution_winner_trading_rest_fallbacks,
            ),
            (
                "late_resolution_winner_trading_rest_successes_total",
                snapshot.late_resolution_winner_trading_rest_successes,
            ),
            (
                "late_resolution_winner_trading_rest_empty_books_total",
                snapshot.late_resolution_winner_trading_rest_empty_books,
            ),
            (
                "late_resolution_winner_trading_rest_latency_ms_total",
                snapshot.late_resolution_winner_trading_rest_latency_ms_total,
            ),
            (
                "late_resolution_winner_trading_fee_read_errors_total",
                snapshot.late_resolution_winner_trading_fee_read_errors,
            ),
            (
                "late_resolution_winner_trading_evaluations_total",
                snapshot.late_resolution_winner_trading_evaluations,
            ),
            (
                "late_resolution_winner_trading_evaluations_resolved_total",
                snapshot.late_resolution_winner_trading_evaluations_resolved,
            ),
            (
                "late_resolution_winner_trading_model_data_rejections_total",
                snapshot.late_resolution_winner_trading_model_data_rejections,
            ),
            (
                "late_resolution_winner_trading_rejection_no_price_history_total",
                snapshot.late_resolution_winner_trading_rejection_no_price_history,
            ),
            (
                "late_resolution_winner_trading_rejection_stale_price_total",
                snapshot.late_resolution_winner_trading_rejection_stale_price,
            ),
            (
                "late_resolution_winner_trading_rejection_no_opening_reference_total",
                snapshot.late_resolution_winner_trading_rejection_no_opening_reference,
            ),
            (
                "late_resolution_winner_trading_rejection_invalid_price_total",
                snapshot.late_resolution_winner_trading_rejection_invalid_price,
            ),
            (
                "late_resolution_winner_trading_rejection_insufficient_observations_total",
                snapshot.late_resolution_winner_trading_rejection_insufficient_observations,
            ),
            (
                "late_resolution_winner_trading_model_market_rejections_total",
                snapshot.late_resolution_winner_trading_model_market_rejections,
            ),
            (
                "late_resolution_winner_trading_model_boundary_rejections_total",
                snapshot.late_resolution_winner_trading_model_boundary_rejections,
            ),
            (
                "late_resolution_winner_trading_model_ev_rejections_total",
                snapshot.late_resolution_winner_trading_model_ev_rejections,
            ),
            (
                "late_resolution_winner_trading_shadow_ev_failures_total",
                snapshot.late_resolution_winner_trading_shadow_ev_failures,
            ),
            (
                "late_resolution_winner_trading_shadow_execution_attempts_total",
                snapshot.late_resolution_winner_trading_shadow_execution_attempts,
            ),
            (
                "late_resolution_winner_trading_shadow_execution_fills_total",
                snapshot.late_resolution_winner_trading_shadow_execution_fills,
            ),
            (
                "late_resolution_winner_trading_shadow_execution_no_fills_total",
                snapshot.late_resolution_winner_trading_shadow_execution_no_fills,
            ),
            (
                "late_resolution_winner_trading_shadow_execution_failures_total",
                snapshot.late_resolution_winner_trading_shadow_execution_failures,
            ),
            (
                "late_resolution_winner_trading_shadow_awaiting_confirmation_total",
                snapshot.late_resolution_winner_trading_shadow_awaiting_confirmation,
            ),
            (
                "late_resolution_winner_trading_shadow_confirmed_total",
                snapshot.late_resolution_winner_trading_shadow_confirmed,
            ),
            (
                "late_resolution_winner_trading_awaiting_confirmation_total",
                snapshot.late_resolution_winner_trading_awaiting_confirmation,
            ),
            (
                "late_resolution_winner_trading_fills_total",
                snapshot.late_resolution_winner_trading_fills,
            ),
            (
                "late_resolution_winner_trading_resolved_total",
                snapshot.late_resolution_winner_trading_resolved,
            ),
            (
                "late_resolution_winner_trading_redeemed_total",
                snapshot.late_resolution_winner_trading_redeemed,
            ),
            (
                "late_resolution_winner_trading_settlement_query_failures_total",
                snapshot.late_resolution_winner_trading_settlement_query_failures,
            ),
            (
                "late_resolution_winner_trading_settlement_empty_results_total",
                snapshot.late_resolution_winner_trading_settlement_empty_results,
            ),
            (
                "late_resolution_winner_trading_settlement_not_terminal_total",
                snapshot.late_resolution_winner_trading_settlement_not_terminal,
            ),
            (
                "late_resolution_winner_trading_wins_total",
                snapshot.late_resolution_winner_trading_wins,
            ),
            (
                "late_resolution_winner_trading_losses_total",
                snapshot.late_resolution_winner_trading_losses,
            ),
            (
                "late_resolution_winner_trading_rtds_connections_total",
                snapshot.late_resolution_winner_trading_rtds_connections,
            ),
            (
                "late_resolution_winner_trading_rtds_disconnects_total",
                snapshot.late_resolution_winner_trading_rtds_disconnects,
            ),
            (
                "late_resolution_winner_trading_rtds_connect_failures_total",
                snapshot.late_resolution_winner_trading_rtds_connect_failures,
            ),
            (
                "late_resolution_winner_trading_rtds_messages_total",
                snapshot.late_resolution_winner_trading_rtds_messages,
            ),
        ];
        let mut output = String::new();
        for (name, value) in values {
            output.push_str("# TYPE polypulse_");
            output.push_str(name);
            output.push_str(" counter\npolypulse_");
            output.push_str(name);
            output.push(' ');
            output.push_str(&value.to_string());
            output.push('\n');
        }
        let asset_fields: [(&str, LateResolutionWinnerTradingAssetMetricAccessor); 8] = [
            ("scans", |metrics| metrics.scans),
            ("signals", |metrics| metrics.signals),
            ("no_book", |metrics| metrics.no_book),
            ("no_signal", |metrics| metrics.no_signal),
            ("evaluations", |metrics| metrics.evaluations),
            ("attempts", |metrics| metrics.attempts),
            ("fills", |metrics| metrics.fills),
            ("resolved", |metrics| metrics.resolved),
        ];
        for (name, value) in asset_fields {
            output.push_str("# TYPE polypulse_late_resolution_winner_trading_asset_");
            output.push_str(name);
            output.push_str("_total counter\n");
            for asset in &snapshot.late_resolution_winner_trading_by_asset {
                output.push_str("polypulse_late_resolution_winner_trading_asset_");
                output.push_str(name);
                output.push_str("_total{asset=\"");
                output.push_str(&asset.asset.replace('\\', "\\\\").replace('"', "\\\""));
                output.push_str("\"} ");
                output.push_str(&value(asset).to_string());
                output.push('\n');
            }
        }
        output.push_str("# TYPE polypulse_paper_net_pnl_usd gauge\npolypulse_paper_net_pnl_usd ");
        output.push_str(&(snapshot.paper_net_pnl_microusd as f64 / 1_000_000.0).to_string());
        output.push('\n');
        output.push_str("# TYPE polypulse_late_resolution_winner_trading_pending gauge\npolypulse_late_resolution_winner_trading_pending ");
        output.push_str(&snapshot.late_resolution_winner_trading_pending.to_string());
        output.push('\n');
        output.push_str("# TYPE polypulse_late_resolution_winner_trading_rest_latency_ms_max gauge\npolypulse_late_resolution_winner_trading_rest_latency_ms_max ");
        output.push_str(
            &snapshot
                .late_resolution_winner_trading_rest_latency_ms_max
                .to_string(),
        );
        output.push('\n');
        output.push_str(
            "# TYPE polypulse_late_resolution_winner_trading_est_pnl_usd gauge\npolypulse_late_resolution_winner_trading_est_pnl_usd ",
        );
        output.push_str(
            &(snapshot.late_resolution_winner_trading_est_pnl_microusd as f64 / 1_000_000.0)
                .to_string(),
        );
        output.push('\n');
        output.push_str("# TYPE polypulse_late_resolution_winner_trading_realized_pnl_usd gauge\npolypulse_late_resolution_winner_trading_realized_pnl_usd ");
        output.push_str(
            &(snapshot.late_resolution_winner_trading_realized_pnl_microusd as f64 / 1_000_000.0)
                .to_string(),
        );
        output.push('\n');
        output.push_str("# TYPE polypulse_late_resolution_winner_trading_rtds_last_observed_at_ms gauge\npolypulse_late_resolution_winner_trading_rtds_last_observed_at_ms ");
        output.push_str(
            &snapshot
                .late_resolution_winner_trading_rtds_last_observed_at_ms
                .to_string(),
        );
        output.push('\n');
        output.push_str("# TYPE polypulse_sdk_market_ws_last_book_at_ms gauge\npolypulse_sdk_market_ws_last_book_at_ms ");
        output.push_str(&snapshot.sdk_market_ws_last_book_at_ms.to_string());
        output.push('\n');
        output.push_str("# TYPE polypulse_sdk_market_ws_downtime_ms_max gauge\npolypulse_sdk_market_ws_downtime_ms_max ");
        output.push_str(&snapshot.sdk_market_ws_downtime_ms_max.to_string());
        output.push('\n');
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn snapshot_preserves_reason_and_total_counts() {
        let metrics = RuntimeMetrics::default();
        metrics.record_orderbook_pair();
        metrics.record_depth_rejection();
        metrics.record_stability_pending();
        metrics.record_arbitrage_rest_shadow(dec!(2), dec!(2), false);
        metrics.record_arbitrage_rest_shadow(dec!(2), dec!(0), true);
        metrics.record_arbitrage_rest_shadow(dec!(0), dec!(0), false);
        metrics.record_arbitrage_admission_shadow(false, false);
        metrics.record_arbitrage_admission_shadow(true, true);
        metrics.record_admission_rejection(AdmissionRejection::ConnectionUnhealthy);
        metrics.record_admission_rejection(AdmissionRejection::YesPriceTooLow);
        metrics.record_trade_started();
        metrics.record_trade_failed();
        metrics.record_stream_connect_failure();
        metrics.record_stream_reconnect();
        metrics.record_stream_recovery();
        metrics.record_sdk_market_ws_reconnect();
        metrics.record_sdk_market_ws_reconnecting_transition();
        metrics.record_sdk_market_ws_recovery();
        metrics.record_sdk_market_ws_downtime(125);
        metrics.record_late_resolution_winner_trading_scan();
        metrics.record_late_resolution_winner_trading_asset_scan("BTC");
        metrics.record_late_resolution_winner_trading_signal();
        metrics.record_late_resolution_winner_trading_asset_signal("btc");
        metrics.record_late_resolution_winner_trading_ws_missing_book();
        metrics.record_late_resolution_winner_trading_rest_fallback();
        metrics.record_late_resolution_winner_trading_rest_result(25, true);
        metrics.record_late_resolution_winner_trading_rest_result(40, false);
        metrics.record_late_resolution_winner_model_rejection(ModelRejection::StalePrice);
        metrics.record_late_resolution_winner_trading_shadow_execution_attempt();
        metrics.record_late_resolution_winner_trading_shadow_execution_fill();
        metrics.record_late_resolution_winner_trading_shadow_execution_no_fill();
        metrics.record_late_resolution_winner_trading_shadow_execution_failure();
        metrics.record_late_resolution_winner_trading_shadow_awaiting_confirmation();
        metrics.record_late_resolution_winner_trading_shadow_confirmed();
        metrics.record_late_resolution_winner_trading_awaiting_confirmation();
        metrics.set_late_resolution_winner_trading_pending(2);
        metrics.record_paper_result(dec!(3), dec!(2), dec!(0.4), dec!(0.5));
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.orderbook_pairs, 1);
        assert_eq!(snapshot.stability_pending, 1);
        assert_eq!(snapshot.arbitrage_rest_shadow_samples, 3);
        assert_eq!(snapshot.arbitrage_rest_shadow_both_fillable, 1);
        assert_eq!(snapshot.arbitrage_rest_shadow_one_sided, 1);
        assert_eq!(snapshot.arbitrage_rest_shadow_neither_fillable, 1);
        assert_eq!(snapshot.arbitrage_rest_shadow_errors, 1);
        assert_eq!(snapshot.arbitrage_admission_shadow_samples, 2);
        assert_eq!(snapshot.arbitrage_admission_shadow_gross_passes, 1);
        assert_eq!(snapshot.arbitrage_admission_shadow_net_passes, 1);
        assert_eq!(snapshot.depth_rejections, 1);
        assert_eq!(snapshot.opportunities_rejected, 2);
        assert_eq!(snapshot.rejection_connection_unhealthy, 1);
        assert_eq!(snapshot.rejection_price_threshold, 1);
        assert_eq!(snapshot.trades_started, snapshot.trades_failed);
        assert_eq!(snapshot.stream_connect_failures, 1);
        assert_eq!(snapshot.stream_reconnects, 1);
        assert_eq!(snapshot.stream_recoveries, 1);
        assert_eq!(snapshot.sdk_market_ws_reconnecting_transitions, 1);
        assert_eq!(snapshot.sdk_market_ws_downtime_ms_total, 125);
        assert_eq!(snapshot.sdk_market_ws_downtime_ms_max, 125);
        assert_eq!(snapshot.late_resolution_winner_trading_scans, 1);
        assert_eq!(snapshot.late_resolution_winner_trading_signals, 1);
        assert_eq!(snapshot.late_resolution_winner_trading_ws_missing_books, 1);
        assert_eq!(snapshot.late_resolution_winner_trading_rest_fallbacks, 1);
        assert_eq!(snapshot.late_resolution_winner_trading_rest_successes, 1);
        assert_eq!(snapshot.late_resolution_winner_trading_rest_empty_books, 1);
        assert_eq!(
            snapshot.late_resolution_winner_trading_rest_latency_ms_total,
            65
        );
        assert_eq!(
            snapshot.late_resolution_winner_trading_rest_latency_ms_max,
            40
        );
        assert_eq!(snapshot.late_resolution_winner_trading_by_asset.len(), 1);
        assert_eq!(
            snapshot.late_resolution_winner_trading_by_asset[0].asset,
            "btc"
        );
        assert_eq!(snapshot.late_resolution_winner_trading_by_asset[0].scans, 1);
        assert_eq!(
            snapshot.late_resolution_winner_trading_by_asset[0].signals,
            1
        );
        assert_eq!(
            snapshot.late_resolution_winner_trading_model_data_rejections,
            1
        );
        assert_eq!(
            snapshot.late_resolution_winner_trading_shadow_execution_attempts,
            1
        );
        assert_eq!(
            snapshot.late_resolution_winner_trading_shadow_execution_fills,
            1
        );
        assert_eq!(
            snapshot.late_resolution_winner_trading_shadow_execution_no_fills,
            1
        );
        assert_eq!(
            snapshot.late_resolution_winner_trading_shadow_execution_failures,
            1
        );
        assert_eq!(
            snapshot.late_resolution_winner_trading_shadow_awaiting_confirmation,
            1
        );
        assert_eq!(snapshot.late_resolution_winner_trading_shadow_confirmed, 1);
        assert_eq!(
            snapshot.late_resolution_winner_trading_awaiting_confirmation,
            1
        );
        assert_eq!(snapshot.late_resolution_winner_trading_pending, 2);
        assert_eq!(snapshot.paper_pairs, 1);
        assert_eq!(snapshot.paper_one_sided, 1);
        assert_eq!(snapshot.paper_net_pnl_microusd, -200_000);
        let exposition = metrics.prometheus();
        assert!(exposition.contains("polypulse_trades_failed_total 1"));
        assert!(exposition.contains("polypulse_arbitrage_rest_shadow_samples_total 3"));
        assert!(exposition.contains("polypulse_arbitrage_admission_shadow_samples_total 2"));
        assert!(exposition.contains("polypulse_sdk_market_ws_downtime_ms_max 125"));
        assert!(exposition.contains("polypulse_paper_net_pnl_usd -0.2"));
        assert!(exposition.contains("polypulse_late_resolution_winner_trading_pending 2"));
        assert!(exposition
            .contains("polypulse_late_resolution_winner_trading_shadow_execution_fills_total 1"));
        assert!(exposition
            .contains("polypulse_late_resolution_winner_trading_shadow_confirmed_total 1"));
        assert!(
            exposition.contains("polypulse_late_resolution_winner_trading_rest_latency_ms_max 40")
        );
        assert!(exposition.contains(
            "polypulse_late_resolution_winner_trading_asset_scans_total{asset=\"btc\"} 1"
        ));
    }
}
