use crate::opportunity_admission::AdmissionRejection;
use serde::Serialize;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

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
    pub rejection_execution_spread: u64,
    pub rejection_trading_paused: u64,
    pub rejection_connection_unhealthy: u64,
    pub rejection_unresolved_state: u64,
    pub rejection_price_threshold: u64,
    pub rejection_near_market_end: u64,
    pub paper_pairs: u64,
    pub paper_one_sided: u64,
    pub paper_net_pnl_microusd: i64,
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
    rejection_execution_spread: AtomicU64,
    rejection_trading_paused: AtomicU64,
    rejection_connection_unhealthy: AtomicU64,
    rejection_unresolved_state: AtomicU64,
    rejection_price_threshold: AtomicU64,
    rejection_near_market_end: AtomicU64,
    paper_pairs: AtomicU64,
    paper_one_sided: AtomicU64,
    paper_net_pnl_microusd: AtomicI64,
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
            rejection_execution_spread: load(&self.rejection_execution_spread),
            rejection_trading_paused: load(&self.rejection_trading_paused),
            rejection_connection_unhealthy: load(&self.rejection_connection_unhealthy),
            rejection_unresolved_state: load(&self.rejection_unresolved_state),
            rejection_price_threshold: load(&self.rejection_price_threshold),
            rejection_near_market_end: load(&self.rejection_near_market_end),
            paper_pairs: load(&self.paper_pairs),
            paper_one_sided: load(&self.paper_one_sided),
            paper_net_pnl_microusd: self.paper_net_pnl_microusd.load(Ordering::Relaxed),
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
        output.push_str("# TYPE polypulse_paper_net_pnl_usd gauge\npolypulse_paper_net_pnl_usd ");
        output.push_str(&(snapshot.paper_net_pnl_microusd as f64 / 1_000_000.0).to_string());
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
        metrics.record_paper_result(dec!(3), dec!(2), dec!(0.4), dec!(0.5));
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.orderbook_pairs, 1);
        assert_eq!(snapshot.depth_rejections, 1);
        assert_eq!(snapshot.opportunities_rejected, 2);
        assert_eq!(snapshot.rejection_connection_unhealthy, 1);
        assert_eq!(snapshot.rejection_price_threshold, 1);
        assert_eq!(snapshot.trades_started, snapshot.trades_failed);
        assert_eq!(snapshot.paper_pairs, 1);
        assert_eq!(snapshot.paper_one_sided, 1);
        assert_eq!(snapshot.paper_net_pnl_microusd, -200_000);
        let exposition = metrics.prometheus();
        assert!(exposition.contains("polypulse_trades_failed_total 1"));
        assert!(exposition.contains("polypulse_paper_net_pnl_usd -0.2"));
    }
}
