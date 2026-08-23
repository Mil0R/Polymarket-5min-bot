use crate::opportunity_admission::AdmissionRejection;
use crate::winner_model::ModelRejection;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Mutex;

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct WinnerBuyAssetMetrics {
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

type WinnerBuyAssetMetricAccessor = fn(&WinnerBuyAssetMetrics) -> u64;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct MetricsSnapshot {
    pub orderbook_pairs: u64,
    pub opportunities_admitted: u64,
    pub opportunities_rejected: u64,
    pub depth_rejections: u64,
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
    pub winner_buy_attempts: u64,
    pub winner_buy_scans: u64,
    pub winner_buy_signals: u64,
    pub winner_buy_no_book: u64,
    pub winner_buy_no_signal: u64,
    pub winner_buy_book_read_errors: u64,
    pub winner_buy_incomplete_books: u64,
    pub winner_buy_stale_books: u64,
    pub winner_buy_stale_ws_books: u64,
    pub winner_buy_stale_rest_books: u64,
    pub winner_buy_ws_missing_books: u64,
    pub winner_buy_rest_fallbacks: u64,
    pub winner_buy_rest_successes: u64,
    pub winner_buy_rest_empty_books: u64,
    pub winner_buy_rest_latency_ms_total: u64,
    pub winner_buy_rest_latency_ms_max: u64,
    pub winner_buy_fee_read_errors: u64,
    pub winner_buy_evaluations: u64,
    pub winner_buy_evaluations_resolved: u64,
    pub winner_buy_model_data_rejections: u64,
    pub winner_buy_rejection_no_price_history: u64,
    pub winner_buy_rejection_stale_price: u64,
    pub winner_buy_rejection_no_opening_reference: u64,
    pub winner_buy_rejection_invalid_price: u64,
    pub winner_buy_rejection_insufficient_observations: u64,
    pub winner_buy_model_market_rejections: u64,
    pub winner_buy_model_boundary_rejections: u64,
    pub winner_buy_model_ev_rejections: u64,
    pub winner_buy_shadow_ev_failures: u64,
    pub winner_buy_awaiting_confirmation: u64,
    pub winner_buy_fills: u64,
    pub winner_buy_pending: u64,
    pub winner_buy_resolved: u64,
    pub winner_buy_redeemed: u64,
    pub winner_buy_settlement_query_failures: u64,
    pub winner_buy_settlement_empty_results: u64,
    pub winner_buy_settlement_not_terminal: u64,
    pub winner_buy_est_pnl_microusd: i64,
    pub winner_buy_wins: u64,
    pub winner_buy_losses: u64,
    pub winner_buy_realized_pnl_microusd: i64,
    pub winner_buy_rtds_connections: u64,
    pub winner_buy_rtds_disconnects: u64,
    pub winner_buy_rtds_connect_failures: u64,
    pub winner_buy_rtds_messages: u64,
    pub winner_buy_rtds_last_observed_at_ms: i64,
    pub winner_buy_by_asset: Vec<WinnerBuyAssetMetrics>,
}

#[derive(Default)]
pub struct RuntimeMetrics {
    orderbook_pairs: AtomicU64,
    opportunities_admitted: AtomicU64,
    opportunities_rejected: AtomicU64,
    depth_rejections: AtomicU64,
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
    winner_buy_attempts: AtomicU64,
    winner_buy_scans: AtomicU64,
    winner_buy_signals: AtomicU64,
    winner_buy_no_book: AtomicU64,
    winner_buy_no_signal: AtomicU64,
    winner_buy_book_read_errors: AtomicU64,
    winner_buy_incomplete_books: AtomicU64,
    winner_buy_stale_books: AtomicU64,
    winner_buy_stale_ws_books: AtomicU64,
    winner_buy_stale_rest_books: AtomicU64,
    winner_buy_ws_missing_books: AtomicU64,
    winner_buy_rest_fallbacks: AtomicU64,
    winner_buy_rest_successes: AtomicU64,
    winner_buy_rest_empty_books: AtomicU64,
    winner_buy_rest_latency_ms_total: AtomicU64,
    winner_buy_rest_latency_ms_max: AtomicU64,
    winner_buy_fee_read_errors: AtomicU64,
    winner_buy_evaluations: AtomicU64,
    winner_buy_evaluations_resolved: AtomicU64,
    winner_buy_model_data_rejections: AtomicU64,
    winner_buy_rejection_no_price_history: AtomicU64,
    winner_buy_rejection_stale_price: AtomicU64,
    winner_buy_rejection_no_opening_reference: AtomicU64,
    winner_buy_rejection_invalid_price: AtomicU64,
    winner_buy_rejection_insufficient_observations: AtomicU64,
    winner_buy_model_market_rejections: AtomicU64,
    winner_buy_model_boundary_rejections: AtomicU64,
    winner_buy_model_ev_rejections: AtomicU64,
    winner_buy_shadow_ev_failures: AtomicU64,
    winner_buy_awaiting_confirmation: AtomicU64,
    winner_buy_fills: AtomicU64,
    winner_buy_pending: AtomicU64,
    winner_buy_resolved: AtomicU64,
    winner_buy_redeemed: AtomicU64,
    winner_buy_settlement_query_failures: AtomicU64,
    winner_buy_settlement_empty_results: AtomicU64,
    winner_buy_settlement_not_terminal: AtomicU64,
    winner_buy_est_pnl_microusd: AtomicI64,
    winner_buy_wins: AtomicU64,
    winner_buy_losses: AtomicU64,
    winner_buy_realized_pnl_microusd: AtomicI64,
    winner_buy_rtds_connections: AtomicU64,
    winner_buy_rtds_disconnects: AtomicU64,
    winner_buy_rtds_connect_failures: AtomicU64,
    winner_buy_rtds_messages: AtomicU64,
    winner_buy_rtds_last_observed_at_ms: AtomicI64,
    winner_buy_by_asset: Mutex<HashMap<String, WinnerBuyAssetMetrics>>,
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
    incrementer!(record_duplicate_signal, duplicate_signals);
    incrementer!(record_quote_rejection, quote_rejections);
    incrementer!(record_risk_rejection, risk_rejections);
    incrementer!(record_balance_rejection, balance_rejections);
    incrementer!(record_trade_started, trades_started);
    incrementer!(record_trade_succeeded, trades_succeeded);
    incrementer!(record_trade_failed, trades_failed);
    incrementer!(record_stream_reconnect, stream_reconnects);
    incrementer!(record_stream_connect_failure, stream_connect_failures);
    incrementer!(record_stream_recovery, stream_recoveries);
    incrementer!(record_sdk_market_ws_reconnect, sdk_market_ws_reconnects);
    incrementer!(record_sdk_market_ws_recovery, sdk_market_ws_recoveries);
    incrementer!(record_sdk_user_ws_reconnect, sdk_user_ws_reconnects);
    incrementer!(record_sdk_user_ws_recovery, sdk_user_ws_recoveries);
    incrementer!(record_winner_buy_attempt, winner_buy_attempts);
    incrementer!(record_winner_buy_scan, winner_buy_scans);
    incrementer!(record_winner_buy_signal, winner_buy_signals);
    incrementer!(record_winner_buy_no_book, winner_buy_no_book);
    incrementer!(record_winner_buy_no_signal, winner_buy_no_signal);
    incrementer!(
        record_winner_buy_book_read_error,
        winner_buy_book_read_errors
    );
    incrementer!(
        record_winner_buy_incomplete_book,
        winner_buy_incomplete_books
    );
    incrementer!(record_winner_buy_stale_book, winner_buy_stale_books);
    incrementer!(record_winner_buy_stale_ws_book, winner_buy_stale_ws_books);
    incrementer!(
        record_winner_buy_stale_rest_book,
        winner_buy_stale_rest_books
    );
    incrementer!(record_winner_buy_rest_fallback, winner_buy_rest_fallbacks);
    incrementer!(
        record_winner_buy_ws_missing_book,
        winner_buy_ws_missing_books
    );
    incrementer!(record_winner_buy_fee_read_error, winner_buy_fee_read_errors);

    pub fn record_winner_buy_rest_result(&self, latency_ms: u64, found: bool) {
        let counter = if found {
            &self.winner_buy_rest_successes
        } else {
            &self.winner_buy_rest_empty_books
        };
        counter.fetch_add(1, Ordering::Relaxed);
        self.winner_buy_rest_latency_ms_total
            .fetch_add(latency_ms, Ordering::Relaxed);
        self.winner_buy_rest_latency_ms_max
            .fetch_max(latency_ms, Ordering::Relaxed);
    }
    incrementer!(record_winner_buy_evaluation, winner_buy_evaluations);
    incrementer!(
        record_winner_buy_rtds_connection,
        winner_buy_rtds_connections
    );
    incrementer!(
        record_winner_buy_rtds_disconnect,
        winner_buy_rtds_disconnects
    );
    incrementer!(
        record_winner_buy_rtds_connect_failure,
        winner_buy_rtds_connect_failures
    );
    incrementer!(
        record_winner_buy_awaiting_confirmation,
        winner_buy_awaiting_confirmation
    );
    incrementer!(
        record_winner_buy_shadow_ev_failure,
        winner_buy_shadow_ev_failures
    );
    incrementer!(record_winner_buy_fill, winner_buy_fills);
    incrementer!(record_winner_buy_resolved, winner_buy_resolved);
    incrementer!(record_winner_buy_redeemed, winner_buy_redeemed);
    incrementer!(
        record_winner_buy_settlement_query_failure,
        winner_buy_settlement_query_failures
    );
    incrementer!(
        record_winner_buy_settlement_empty_result,
        winner_buy_settlement_empty_results
    );
    incrementer!(
        record_winner_buy_settlement_not_terminal,
        winner_buy_settlement_not_terminal
    );

    pub fn set_winner_buy_pending(&self, count: usize) {
        self.winner_buy_pending
            .store(count.try_into().unwrap_or(u64::MAX), Ordering::Relaxed);
    }

    fn record_winner_asset(&self, asset: &str, update: impl FnOnce(&mut WinnerBuyAssetMetrics)) {
        let key = asset.trim().to_ascii_lowercase();
        let key = if key.is_empty() {
            "unknown".into()
        } else {
            key
        };
        let mut metrics = self
            .winner_buy_by_asset
            .lock()
            .expect("Winner Buy asset metrics lock poisoned");
        let entry = metrics
            .entry(key.clone())
            .or_insert_with(|| WinnerBuyAssetMetrics {
                asset: key,
                ..WinnerBuyAssetMetrics::default()
            });
        update(entry);
    }

    pub fn record_winner_buy_asset_scan(&self, asset: &str) {
        self.record_winner_asset(asset, |metrics| metrics.scans += 1);
    }

    pub fn record_winner_buy_asset_signal(&self, asset: &str) {
        self.record_winner_asset(asset, |metrics| metrics.signals += 1);
    }

    pub fn record_winner_buy_asset_no_book(&self, asset: &str) {
        self.record_winner_asset(asset, |metrics| metrics.no_book += 1);
    }

    pub fn record_winner_buy_asset_no_signal(&self, asset: &str) {
        self.record_winner_asset(asset, |metrics| metrics.no_signal += 1);
    }

    pub fn record_winner_buy_asset_evaluation(&self, asset: &str) {
        self.record_winner_asset(asset, |metrics| metrics.evaluations += 1);
    }

    pub fn record_winner_buy_asset_attempt(&self, asset: &str) {
        self.record_winner_asset(asset, |metrics| metrics.attempts += 1);
    }

    pub fn record_winner_buy_asset_fill(&self, asset: &str) {
        self.record_winner_asset(asset, |metrics| metrics.fills += 1);
    }

    pub fn record_winner_buy_asset_resolved(&self, asset: &str) {
        self.record_winner_asset(asset, |metrics| metrics.resolved += 1);
    }

    pub fn record_winner_model_rejection(&self, rejection: ModelRejection) {
        let detail = match rejection {
            ModelRejection::NoPriceHistory => Some(&self.winner_buy_rejection_no_price_history),
            ModelRejection::StalePrice => Some(&self.winner_buy_rejection_stale_price),
            ModelRejection::NoOpeningReference => {
                Some(&self.winner_buy_rejection_no_opening_reference)
            }
            ModelRejection::InvalidPrice => Some(&self.winner_buy_rejection_invalid_price),
            ModelRejection::InsufficientObservations => {
                Some(&self.winner_buy_rejection_insufficient_observations)
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
            | ModelRejection::InsufficientObservations => &self.winner_buy_model_data_rejections,
            ModelRejection::SpreadTooWide | ModelRejection::InsufficientDepth => {
                &self.winner_buy_model_market_rejections
            }
            ModelRejection::ReversalBoundary => &self.winner_buy_model_boundary_rejections,
            ModelRejection::NetEvTooLow => &self.winner_buy_model_ev_rejections,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_winner_buy_rtds_observation(&self, observed_at_ms: i64) {
        self.winner_buy_rtds_messages
            .fetch_add(1, Ordering::Relaxed);
        self.winner_buy_rtds_last_observed_at_ms
            .fetch_max(observed_at_ms, Ordering::Relaxed);
    }

    pub fn record_market_ws_book(&self, observed_at_ms: i64) {
        self.sdk_market_ws_last_book_at_ms
            .fetch_max(observed_at_ms, Ordering::Relaxed);
    }

    pub fn record_winner_buy_evaluations_resolved(&self, count: usize) {
        self.winner_buy_evaluations_resolved
            .fetch_add(count.try_into().unwrap_or(u64::MAX), Ordering::Relaxed);
    }

    /// Estimate winner-buy profit: filled shares held to settlement (pays 1.00).
    pub fn record_winner_buy_pnl(
        &self,
        filled: rust_decimal::Decimal,
        price: rust_decimal::Decimal,
    ) {
        use rust_decimal::prelude::ToPrimitive;
        use rust_decimal_macros::dec;
        let pnl = filled * (dec!(1) - price);
        let microusd = (pnl * dec!(1000000)).round().to_i64().unwrap_or(0);
        self.winner_buy_est_pnl_microusd
            .fetch_add(microusd, Ordering::Relaxed);
    }

    /// Record the REALIZED settlement result of a winner-buy paper position.
    pub fn record_winner_buy_settlement(&self, pnl: rust_decimal::Decimal) {
        use rust_decimal::prelude::ToPrimitive;
        use rust_decimal_macros::dec;
        if pnl > dec!(0) {
            self.winner_buy_wins.fetch_add(1, Ordering::Relaxed);
        } else {
            self.winner_buy_losses.fetch_add(1, Ordering::Relaxed);
        }
        let microusd = (pnl * dec!(1000000)).round().to_i64().unwrap_or(0);
        self.winner_buy_realized_pnl_microusd
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
        let mut winner_buy_by_asset: Vec<_> = self
            .winner_buy_by_asset
            .lock()
            .expect("Winner Buy asset metrics lock poisoned")
            .values()
            .cloned()
            .collect();
        winner_buy_by_asset.sort_by(|left, right| left.asset.cmp(&right.asset));
        MetricsSnapshot {
            orderbook_pairs: load(&self.orderbook_pairs),
            opportunities_admitted: load(&self.opportunities_admitted),
            opportunities_rejected: load(&self.opportunities_rejected),
            depth_rejections: load(&self.depth_rejections),
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
            winner_buy_attempts: load(&self.winner_buy_attempts),
            winner_buy_scans: load(&self.winner_buy_scans),
            winner_buy_signals: load(&self.winner_buy_signals),
            winner_buy_no_book: load(&self.winner_buy_no_book),
            winner_buy_no_signal: load(&self.winner_buy_no_signal),
            winner_buy_book_read_errors: load(&self.winner_buy_book_read_errors),
            winner_buy_incomplete_books: load(&self.winner_buy_incomplete_books),
            winner_buy_stale_books: load(&self.winner_buy_stale_books),
            winner_buy_stale_ws_books: load(&self.winner_buy_stale_ws_books),
            winner_buy_stale_rest_books: load(&self.winner_buy_stale_rest_books),
            winner_buy_ws_missing_books: load(&self.winner_buy_ws_missing_books),
            winner_buy_rest_fallbacks: load(&self.winner_buy_rest_fallbacks),
            winner_buy_rest_successes: load(&self.winner_buy_rest_successes),
            winner_buy_rest_empty_books: load(&self.winner_buy_rest_empty_books),
            winner_buy_rest_latency_ms_total: load(&self.winner_buy_rest_latency_ms_total),
            winner_buy_rest_latency_ms_max: load(&self.winner_buy_rest_latency_ms_max),
            winner_buy_fee_read_errors: load(&self.winner_buy_fee_read_errors),
            winner_buy_evaluations: load(&self.winner_buy_evaluations),
            winner_buy_evaluations_resolved: load(&self.winner_buy_evaluations_resolved),
            winner_buy_model_data_rejections: load(&self.winner_buy_model_data_rejections),
            winner_buy_rejection_no_price_history: load(
                &self.winner_buy_rejection_no_price_history,
            ),
            winner_buy_rejection_stale_price: load(&self.winner_buy_rejection_stale_price),
            winner_buy_rejection_no_opening_reference: load(
                &self.winner_buy_rejection_no_opening_reference,
            ),
            winner_buy_rejection_invalid_price: load(&self.winner_buy_rejection_invalid_price),
            winner_buy_rejection_insufficient_observations: load(
                &self.winner_buy_rejection_insufficient_observations,
            ),
            winner_buy_model_market_rejections: load(&self.winner_buy_model_market_rejections),
            winner_buy_model_boundary_rejections: load(&self.winner_buy_model_boundary_rejections),
            winner_buy_model_ev_rejections: load(&self.winner_buy_model_ev_rejections),
            winner_buy_shadow_ev_failures: load(&self.winner_buy_shadow_ev_failures),
            winner_buy_awaiting_confirmation: load(&self.winner_buy_awaiting_confirmation),
            winner_buy_fills: load(&self.winner_buy_fills),
            winner_buy_pending: load(&self.winner_buy_pending),
            winner_buy_resolved: load(&self.winner_buy_resolved),
            winner_buy_redeemed: load(&self.winner_buy_redeemed),
            winner_buy_settlement_query_failures: load(&self.winner_buy_settlement_query_failures),
            winner_buy_settlement_empty_results: load(&self.winner_buy_settlement_empty_results),
            winner_buy_settlement_not_terminal: load(&self.winner_buy_settlement_not_terminal),
            winner_buy_est_pnl_microusd: self.winner_buy_est_pnl_microusd.load(Ordering::Relaxed),
            winner_buy_wins: load(&self.winner_buy_wins),
            winner_buy_losses: load(&self.winner_buy_losses),
            winner_buy_realized_pnl_microusd: self
                .winner_buy_realized_pnl_microusd
                .load(Ordering::Relaxed),
            winner_buy_rtds_connections: load(&self.winner_buy_rtds_connections),
            winner_buy_rtds_disconnects: load(&self.winner_buy_rtds_disconnects),
            winner_buy_rtds_connect_failures: load(&self.winner_buy_rtds_connect_failures),
            winner_buy_rtds_messages: load(&self.winner_buy_rtds_messages),
            winner_buy_rtds_last_observed_at_ms: self
                .winner_buy_rtds_last_observed_at_ms
                .load(Ordering::Relaxed),
            winner_buy_by_asset,
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
            ("winner_buy_attempts_total", snapshot.winner_buy_attempts),
            ("winner_buy_scans_total", snapshot.winner_buy_scans),
            ("winner_buy_signals_total", snapshot.winner_buy_signals),
            ("winner_buy_no_book_total", snapshot.winner_buy_no_book),
            ("winner_buy_no_signal_total", snapshot.winner_buy_no_signal),
            (
                "winner_buy_book_read_errors_total",
                snapshot.winner_buy_book_read_errors,
            ),
            (
                "winner_buy_incomplete_books_total",
                snapshot.winner_buy_incomplete_books,
            ),
            (
                "winner_buy_stale_books_total",
                snapshot.winner_buy_stale_books,
            ),
            (
                "winner_buy_stale_ws_books_total",
                snapshot.winner_buy_stale_ws_books,
            ),
            (
                "winner_buy_stale_rest_books_total",
                snapshot.winner_buy_stale_rest_books,
            ),
            (
                "winner_buy_ws_missing_books_total",
                snapshot.winner_buy_ws_missing_books,
            ),
            (
                "winner_buy_rest_fallbacks_total",
                snapshot.winner_buy_rest_fallbacks,
            ),
            (
                "winner_buy_rest_successes_total",
                snapshot.winner_buy_rest_successes,
            ),
            (
                "winner_buy_rest_empty_books_total",
                snapshot.winner_buy_rest_empty_books,
            ),
            (
                "winner_buy_rest_latency_ms_total",
                snapshot.winner_buy_rest_latency_ms_total,
            ),
            (
                "winner_buy_fee_read_errors_total",
                snapshot.winner_buy_fee_read_errors,
            ),
            (
                "winner_buy_evaluations_total",
                snapshot.winner_buy_evaluations,
            ),
            (
                "winner_buy_evaluations_resolved_total",
                snapshot.winner_buy_evaluations_resolved,
            ),
            (
                "winner_buy_model_data_rejections_total",
                snapshot.winner_buy_model_data_rejections,
            ),
            (
                "winner_buy_rejection_no_price_history_total",
                snapshot.winner_buy_rejection_no_price_history,
            ),
            (
                "winner_buy_rejection_stale_price_total",
                snapshot.winner_buy_rejection_stale_price,
            ),
            (
                "winner_buy_rejection_no_opening_reference_total",
                snapshot.winner_buy_rejection_no_opening_reference,
            ),
            (
                "winner_buy_rejection_invalid_price_total",
                snapshot.winner_buy_rejection_invalid_price,
            ),
            (
                "winner_buy_rejection_insufficient_observations_total",
                snapshot.winner_buy_rejection_insufficient_observations,
            ),
            (
                "winner_buy_model_market_rejections_total",
                snapshot.winner_buy_model_market_rejections,
            ),
            (
                "winner_buy_model_boundary_rejections_total",
                snapshot.winner_buy_model_boundary_rejections,
            ),
            (
                "winner_buy_model_ev_rejections_total",
                snapshot.winner_buy_model_ev_rejections,
            ),
            (
                "winner_buy_shadow_ev_failures_total",
                snapshot.winner_buy_shadow_ev_failures,
            ),
            (
                "winner_buy_awaiting_confirmation_total",
                snapshot.winner_buy_awaiting_confirmation,
            ),
            ("winner_buy_fills_total", snapshot.winner_buy_fills),
            ("winner_buy_resolved_total", snapshot.winner_buy_resolved),
            ("winner_buy_redeemed_total", snapshot.winner_buy_redeemed),
            (
                "winner_buy_settlement_query_failures_total",
                snapshot.winner_buy_settlement_query_failures,
            ),
            (
                "winner_buy_settlement_empty_results_total",
                snapshot.winner_buy_settlement_empty_results,
            ),
            (
                "winner_buy_settlement_not_terminal_total",
                snapshot.winner_buy_settlement_not_terminal,
            ),
            ("winner_buy_wins_total", snapshot.winner_buy_wins),
            ("winner_buy_losses_total", snapshot.winner_buy_losses),
            (
                "winner_buy_rtds_connections_total",
                snapshot.winner_buy_rtds_connections,
            ),
            (
                "winner_buy_rtds_disconnects_total",
                snapshot.winner_buy_rtds_disconnects,
            ),
            (
                "winner_buy_rtds_connect_failures_total",
                snapshot.winner_buy_rtds_connect_failures,
            ),
            (
                "winner_buy_rtds_messages_total",
                snapshot.winner_buy_rtds_messages,
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
        let asset_fields: [(&str, WinnerBuyAssetMetricAccessor); 8] = [
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
            output.push_str("# TYPE polypulse_winner_buy_asset_");
            output.push_str(name);
            output.push_str("_total counter\n");
            for asset in &snapshot.winner_buy_by_asset {
                output.push_str("polypulse_winner_buy_asset_");
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
        output.push_str("# TYPE polypulse_winner_buy_pending gauge\npolypulse_winner_buy_pending ");
        output.push_str(&snapshot.winner_buy_pending.to_string());
        output.push('\n');
        output.push_str("# TYPE polypulse_winner_buy_rest_latency_ms_max gauge\npolypulse_winner_buy_rest_latency_ms_max ");
        output.push_str(&snapshot.winner_buy_rest_latency_ms_max.to_string());
        output.push('\n');
        output.push_str(
            "# TYPE polypulse_winner_buy_est_pnl_usd gauge\npolypulse_winner_buy_est_pnl_usd ",
        );
        output.push_str(&(snapshot.winner_buy_est_pnl_microusd as f64 / 1_000_000.0).to_string());
        output.push('\n');
        output.push_str("# TYPE polypulse_winner_buy_realized_pnl_usd gauge\npolypulse_winner_buy_realized_pnl_usd ");
        output.push_str(
            &(snapshot.winner_buy_realized_pnl_microusd as f64 / 1_000_000.0).to_string(),
        );
        output.push('\n');
        output.push_str("# TYPE polypulse_winner_buy_rtds_last_observed_at_ms gauge\npolypulse_winner_buy_rtds_last_observed_at_ms ");
        output.push_str(&snapshot.winner_buy_rtds_last_observed_at_ms.to_string());
        output.push('\n');
        output.push_str("# TYPE polypulse_sdk_market_ws_last_book_at_ms gauge\npolypulse_sdk_market_ws_last_book_at_ms ");
        output.push_str(&snapshot.sdk_market_ws_last_book_at_ms.to_string());
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
        metrics.record_admission_rejection(AdmissionRejection::ConnectionUnhealthy);
        metrics.record_admission_rejection(AdmissionRejection::YesPriceTooLow);
        metrics.record_trade_started();
        metrics.record_trade_failed();
        metrics.record_stream_connect_failure();
        metrics.record_stream_reconnect();
        metrics.record_stream_recovery();
        metrics.record_winner_buy_scan();
        metrics.record_winner_buy_asset_scan("BTC");
        metrics.record_winner_buy_signal();
        metrics.record_winner_buy_asset_signal("btc");
        metrics.record_winner_buy_ws_missing_book();
        metrics.record_winner_buy_rest_fallback();
        metrics.record_winner_buy_rest_result(25, true);
        metrics.record_winner_buy_rest_result(40, false);
        metrics.record_winner_model_rejection(ModelRejection::StalePrice);
        metrics.record_winner_buy_awaiting_confirmation();
        metrics.set_winner_buy_pending(2);
        metrics.record_paper_result(dec!(3), dec!(2), dec!(0.4), dec!(0.5));
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.orderbook_pairs, 1);
        assert_eq!(snapshot.depth_rejections, 1);
        assert_eq!(snapshot.opportunities_rejected, 2);
        assert_eq!(snapshot.rejection_connection_unhealthy, 1);
        assert_eq!(snapshot.rejection_price_threshold, 1);
        assert_eq!(snapshot.trades_started, snapshot.trades_failed);
        assert_eq!(snapshot.stream_connect_failures, 1);
        assert_eq!(snapshot.stream_reconnects, 1);
        assert_eq!(snapshot.stream_recoveries, 1);
        assert_eq!(snapshot.winner_buy_scans, 1);
        assert_eq!(snapshot.winner_buy_signals, 1);
        assert_eq!(snapshot.winner_buy_ws_missing_books, 1);
        assert_eq!(snapshot.winner_buy_rest_fallbacks, 1);
        assert_eq!(snapshot.winner_buy_rest_successes, 1);
        assert_eq!(snapshot.winner_buy_rest_empty_books, 1);
        assert_eq!(snapshot.winner_buy_rest_latency_ms_total, 65);
        assert_eq!(snapshot.winner_buy_rest_latency_ms_max, 40);
        assert_eq!(snapshot.winner_buy_by_asset.len(), 1);
        assert_eq!(snapshot.winner_buy_by_asset[0].asset, "btc");
        assert_eq!(snapshot.winner_buy_by_asset[0].scans, 1);
        assert_eq!(snapshot.winner_buy_by_asset[0].signals, 1);
        assert_eq!(snapshot.winner_buy_model_data_rejections, 1);
        assert_eq!(snapshot.winner_buy_awaiting_confirmation, 1);
        assert_eq!(snapshot.winner_buy_pending, 2);
        assert_eq!(snapshot.paper_pairs, 1);
        assert_eq!(snapshot.paper_one_sided, 1);
        assert_eq!(snapshot.paper_net_pnl_microusd, -200_000);
        let exposition = metrics.prometheus();
        assert!(exposition.contains("polypulse_trades_failed_total 1"));
        assert!(exposition.contains("polypulse_paper_net_pnl_usd -0.2"));
        assert!(exposition.contains("polypulse_winner_buy_pending 2"));
        assert!(exposition.contains("polypulse_winner_buy_rest_latency_ms_max 40"));
        assert!(exposition.contains("polypulse_winner_buy_asset_scans_total{asset=\"btc\"} 1"));
    }
}
