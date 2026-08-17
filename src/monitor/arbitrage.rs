use crate::monitor::orderbook::best_ask;
use polymarket_client_sdk::clob::ws::types::response::BookUpdate;
use polymarket_client_sdk::types::{Decimal, B256, U256};
use rust_decimal_macros::dec;
use tracing::debug;

#[derive(Debug, Clone)]
pub struct ArbitrageOpportunity {
    pub market_id: B256,
    pub yes_token_id: U256,
    pub no_token_id: U256,
    pub yes_ask_price: Decimal,
    pub no_ask_price: Decimal,
    pub total_cost: Decimal,
    pub profit_percentage: Decimal,
    pub yes_size: Decimal,
    pub no_size: Decimal,
}

/// Result of the detector's top-of-book scan, distinguishing why an opportunity
/// was not produced so callers can attribute it to depth vs. no-edge.
#[derive(Debug, Clone)]
pub enum ArbitrageScan {
    Opportunity(ArbitrageOpportunity),
    /// No best ask on one leg, or edge below the profit threshold.
    NoEdge,
    /// Edge exists but one or both legs lack the minimum ask liquidity.
    DepthRejected,
}

/// Internal fine-grained result of `find_best_opportunity`.
enum FindOutcome {
    Opportunity((Decimal, Decimal, Decimal, Decimal, Decimal, Decimal, Decimal)),
    NoEdge,
    DepthRejected,
}

pub struct ArbitrageDetector {
    min_profit_threshold: Decimal,
    min_order_value_usd: Decimal, // Min order value (USD)
    /// Minimum cumulative ask liquidity (USD) on each leg to trust the signal.
    min_ask_depth_usd: Decimal,
}

/// Cumulative notional (USD) of ask levels priced at or inside `up_to_price`.
/// The SDK contract sorts asks ascending, so this walks the top-of-book layers.
fn cumulative_ask_depth_usd(book: &BookUpdate, up_to_price: Decimal) -> Decimal {
    book.asks
        .iter()
        .take_while(|level| level.price <= up_to_price)
        .map(|level| level.price * level.size)
        .sum()
}

impl ArbitrageDetector {
    /// Backward-compatible constructor without the liquidity-depth filter.
    #[allow(dead_code)] // used by tests; production path uses with_min_depth
    pub fn new(min_profit_threshold: f64) -> Self {
        Self::with_min_depth(min_profit_threshold, 0.0)
    }

    pub fn with_min_depth(min_profit_threshold: f64, min_ask_depth_usd: f64) -> Self {
        Self {
            min_profit_threshold: Decimal::try_from(min_profit_threshold).unwrap_or(dec!(0.001)),
            min_order_value_usd: dec!(1.0), // Min $1 per order
            min_ask_depth_usd: Decimal::try_from(min_ask_depth_usd).unwrap_or(dec!(0)),
        }
    }

    /// Select prices and the independent top-of-book depth for each leg.
    /// Keeping both depths allows the executor to submit the harder (thinner) leg first.
    /// Executor will: compare prices, add slippage, create orders.
    fn find_best_opportunity(
        &self,
        yes_book: &BookUpdate,
        no_book: &BookUpdate,
    ) -> FindOutcome {
        let Some(yes_best) = best_ask(yes_book) else {
            return FindOutcome::NoEdge;
        };
        let Some(no_best) = best_ask(no_book) else {
            return FindOutcome::NoEdge;
        };

        let yes_price = yes_best.price.round_dp(2);
        let no_price = no_best.price.round_dp(2);
        let total_price = yes_price + no_price;

        let edge = dec!(1.0) - total_price;
        if edge < self.min_profit_threshold {
            return FindOutcome::NoEdge; // Best ask total > 1, no arb
        }

        // Available size at best ask: min of both, floor to 2 decimals
        let raw_size = yes_best.size.min(no_best.size);
        let final_size = if raw_size.is_zero() {
            dec!(0.01)
        } else {
            (raw_size * dec!(100.0)).floor() / dec!(100.0)
        };

        let yes_order_value = yes_price * final_size;
        let no_order_value = no_price * final_size;
        if yes_order_value < self.min_order_value_usd || no_order_value < self.min_order_value_usd {
            return FindOutcome::NoEdge;
        }

        // Liquidity sanity check: both legs must have enough cumulative ask size at
        // (or inside) the best ask. 5-min markets often show a best ask backed by a
        // single guard order that vanishes on touch; requiring real depth here filters
        // those phantom opportunities before they reach the executor.
        // NOTE: use the RAW best-ask price, not round_dp(2) — rounding can round
        // DOWN (e.g. 0.451 → 0.45), which would exclude the best-ask layer itself
        // from the depth walk and zero out the depth for ~half of all prices.
        if self.min_ask_depth_usd > dec!(0) {
            let yes_depth = cumulative_ask_depth_usd(yes_book, yes_best.price);
            let no_depth = cumulative_ask_depth_usd(no_book, no_best.price);
            if yes_depth < self.min_ask_depth_usd || no_depth < self.min_ask_depth_usd {
                return FindOutcome::DepthRejected;
            }
        }

        let profit_pct = (dec!(1.0) - total_price) * dec!(100.0);
        FindOutcome::Opportunity((
            yes_price,
            no_price,
            yes_best.size,
            no_best.size,
            final_size,
            profit_pct,
            total_price,
        ))
    }

    /// Print order depth (debug level to avoid log spam)
    fn print_orderbook_depth(
        &self,
        yes_book: &BookUpdate,
        no_book: &BookUpdate,
        yes_final_price: Decimal,
        no_final_price: Decimal,
    ) {
        let yes_asks = &yes_book.asks;
        let yes_depth_str: Vec<String> = yes_asks
            .iter()
            .take(5)
            .map(|level| {
                let m = if (level.price - yes_final_price).abs() < dec!(0.001) {
                    "←"
                } else {
                    ""
                };
                format!("{:.2}@{:.2}{}", level.price, level.size, m)
            })
            .collect();
        let no_asks = &no_book.asks;
        let no_depth_str: Vec<String> = no_asks
            .iter()
            .take(5)
            .map(|level| {
                let m = if (level.price - no_final_price).abs() < dec!(0.001) {
                    "←"
                } else {
                    ""
                };
                format!("{:.2}@{:.2}{}", level.price, level.size, m)
            })
            .collect();
        debug!(
            yes_depth = yes_depth_str.join(", "),
            no_depth = no_depth_str.join(", "),
            "Order depth"
        );
        // Level selection log moved to executor; prints slippage-adjusted prices
    }

    /// Check if orderbook has arbitrage opportunity.
    /// Compatibility wrapper; production path uses `check_arbitrage_reasoned`.
    #[allow(dead_code)] // used by tests and kept as the simple API
    pub fn check_arbitrage(
        &self,
        yes_book: &BookUpdate,
        no_book: &BookUpdate,
        market_id: &B256,
    ) -> Option<ArbitrageOpportunity> {
        match self.check_arbitrage_reasoned(yes_book, no_book, market_id) {
            ArbitrageScan::Opportunity(opportunity) => Some(opportunity),
            _ => None,
        }
    }

    /// Like `check_arbitrage` but reports why nothing was produced.
    pub fn check_arbitrage_reasoned(
        &self,
        yes_book: &BookUpdate,
        no_book: &BookUpdate,
        market_id: &B256,
    ) -> ArbitrageScan {
        // Select best ask first; executor: compare prices, add slippage, create orders
        let (yes_ask, no_ask, yes_size, no_size, final_size, net_profit_pct, total_price) =
            match self.find_best_opportunity(yes_book, no_book) {
                FindOutcome::Opportunity(found) => found,
                FindOutcome::NoEdge => return ArbitrageScan::NoEdge,
                FindOutcome::DepthRejected => return ArbitrageScan::DepthRejected,
            };

        self.print_orderbook_depth(yes_book, no_book, yes_ask, no_ask);

        debug!(
            market_id = %market_id,
            yes_price = %yes_ask,
            no_price = %no_ask,
            total_price = %total_price,
            net_profit_pct = %net_profit_pct,
            order_size = %final_size,
            "Arbitrage opportunity (best ask)"
        );

        ArbitrageScan::Opportunity(ArbitrageOpportunity {
            market_id: *market_id,
            yes_token_id: yes_book.asset_id,
            no_token_id: no_book.asset_id,
            yes_ask_price: yes_ask,
            no_ask_price: no_ask,
            total_cost: total_price * final_size,
            profit_percentage: net_profit_pct,
            yes_size,
            no_size,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use polymarket_client_sdk::clob::ws::types::response::{BookUpdate, OrderBookLevel};

    fn book(asset_id: U256, ask: Decimal, size: Decimal) -> BookUpdate {
        BookUpdate::builder()
            .asset_id(asset_id)
            .market(B256::ZERO)
            .timestamp(0)
            .bids(Vec::new())
            .asks(vec![OrderBookLevel::builder()
                .price(ask)
                .size(size)
                .build()])
            .build()
    }

    #[test]
    fn applies_minimum_profit_threshold() {
        let detector = ArbitrageDetector::new(0.02);
        let yes = book(U256::from(1), dec!(0.50), dec!(10));
        let no = book(U256::from(2), dec!(0.49), dec!(10));
        assert!(detector.check_arbitrage(&yes, &no, &B256::ZERO).is_none());
        let no = book(U256::from(2), dec!(0.47), dec!(10));
        assert!(detector.check_arbitrage(&yes, &no, &B256::ZERO).is_some());
    }

    #[test]
    fn uses_lowest_ask_when_wire_order_is_descending() {
        let yes = BookUpdate::builder()
            .asset_id(U256::from(1))
            .market(B256::ZERO)
            .timestamp(0)
            .bids(Vec::new())
            .asks(vec![
                OrderBookLevel::builder()
                    .price(dec!(0.99))
                    .size(dec!(10))
                    .build(),
                OrderBookLevel::builder()
                    .price(dec!(0.40))
                    .size(dec!(10))
                    .build(),
            ])
            .build();
        let no = book(U256::from(2), dec!(0.50), dec!(10));
        let opportunity = ArbitrageDetector::new(0.01)
            .check_arbitrage(&yes, &no, &B256::ZERO)
            .unwrap();
        assert_eq!(opportunity.yes_ask_price, dec!(0.40));
        assert_eq!(opportunity.no_ask_price, dec!(0.50));
    }

    #[test]
    fn preserves_each_legs_best_ask_depth() {
        let yes = book(U256::from(1), dec!(0.40), dec!(3));
        let no = book(U256::from(2), dec!(0.50), dec!(9));
        let opportunity = ArbitrageDetector::new(0.01)
            .check_arbitrage(&yes, &no, &B256::ZERO)
            .unwrap();

        assert_eq!(opportunity.yes_size, dec!(3));
        assert_eq!(opportunity.no_size, dec!(9));
    }

    #[test]
    fn rejects_phantom_signal_when_either_leg_lacks_depth() {
        // Both legs show a best ask under the profit threshold, but the NO side
        // carries only a thin guard order (0.50 * 1 = $0.50) far below the $10
        // minimum depth — exactly the 5-min market phantom-signal shape.
        let yes = book(U256::from(1), dec!(0.40), dec!(40)); // $16 depth
        let no = book(U256::from(2), dec!(0.50), dec!(1)); //  $0.50 depth
        let detector = ArbitrageDetector::with_min_depth(0.01, 10.0);
        assert!(detector.check_arbitrage(&yes, &no, &B256::ZERO).is_none());
    }

    #[test]
    fn admits_signal_when_both_legs_have_enough_depth() {
        let yes = book(U256::from(1), dec!(0.40), dec!(40)); // $16 depth
        let no = book(U256::from(2), dec!(0.50), dec!(40)); //  $20 depth
        let detector = ArbitrageDetector::with_min_depth(0.01, 10.0);
        assert!(detector.check_arbitrage(&yes, &no, &B256::ZERO).is_some());
    }

    #[test]
    fn depth_uses_raw_best_ask_not_rounded_down_price() {
        // Regression: best ask 0.451 rounds to 0.45; the depth walk must still
        // include the 0.451 layer or every such price is wrongly rejected.
        let yes = book(U256::from(1), dec!(0.451), dec!(40)); // $18.04 depth
        let no = book(U256::from(2), dec!(0.499), dec!(30)); //  $14.97 depth
        let detector = ArbitrageDetector::with_min_depth(0.001, 10.0);
        assert!(detector.check_arbitrage(&yes, &no, &B256::ZERO).is_some());
    }

    #[test]
    fn reasoned_scan_reports_depth_rejection_vs_no_edge() {
        // Edge exists but NO leg has only $1.50 depth (3 shares @ 0.50) which is
        // below the $10 minimum yet above the $1 per-leg minimum → DepthRejected.
        let yes = book(U256::from(1), dec!(0.40), dec!(40));
        let no = book(U256::from(2), dec!(0.50), dec!(3));
        let detector = ArbitrageDetector::with_min_depth(0.001, 10.0);
        assert!(matches!(
            detector.check_arbitrage_reasoned(&yes, &no, &B256::ZERO),
            ArbitrageScan::DepthRejected
        ));

        // No edge (total >= 1) → NoEdge, not DepthRejected.
        let yes_no_edge = book(U256::from(1), dec!(0.60), dec!(100));
        let no_no_edge = book(U256::from(2), dec!(0.60), dec!(100));
        assert!(matches!(
            detector.check_arbitrage_reasoned(&yes_no_edge, &no_no_edge, &B256::ZERO),
            ArbitrageScan::NoEdge
        ));
    }
}
