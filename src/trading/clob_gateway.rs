use crate::monitor::arbitrage::ArbitrageOpportunity;
use crate::network_retry::{retry_read, ReadRetryPolicy};
use crate::trading::executor::{ExecutionQuote, OrderPairResult, TradingExecutor};
use crate::trading::AuthenticatedClobClient;
use anyhow::Result;
use futures::future::BoxFuture;
use polymarket_client_sdk::types::{Decimal, U256};
use std::collections::HashSet;

/// Minimal read boundary used by startup and post-disconnect reconciliation.
/// Trading remains on the authenticated SDK client; risk accounting can be tested
/// without constructing credentials or performing network I/O.
pub trait OrderStatusReader: Send + Sync {
    fn cumulative_fill<'a>(&'a self, order_id: &'a str) -> BoxFuture<'a, Result<Decimal>>;
}

impl OrderStatusReader for AuthenticatedClobClient {
    fn cumulative_fill<'a>(&'a self, order_id: &'a str) -> BoxFuture<'a, Result<Decimal>> {
        Box::pin(async move {
            retry_read(ReadRetryPolicy::default(), || async {
                Ok(self.order(order_id).await?.size_matched)
            })
            .await
        })
    }
}

/// Business-level CLOB write boundary. SDK order builders and signed-order types
/// remain private to `TradingExecutor`.
pub trait TradingGateway: Send + Sync {
    fn execute_pair<'a>(
        &'a self,
        opportunity: &'a ArbitrageOpportunity,
        quote: &'a ExecutionQuote,
    ) -> BoxFuture<'a, Result<OrderPairResult>>;

    fn cancel_window_orders<'a>(
        &'a self,
        tokens: &'a HashSet<U256>,
    ) -> BoxFuture<'a, Result<usize>>;

    fn sell<'a>(&'a self, token: U256, price: Decimal, size: Decimal) -> BoxFuture<'a, Result<()>>;
}

impl TradingGateway for TradingExecutor {
    fn execute_pair<'a>(
        &'a self,
        opportunity: &'a ArbitrageOpportunity,
        quote: &'a ExecutionQuote,
    ) -> BoxFuture<'a, Result<OrderPairResult>> {
        Box::pin(async move { self.execute_arbitrage_pair(opportunity, quote).await })
    }

    fn cancel_window_orders<'a>(
        &'a self,
        tokens: &'a HashSet<U256>,
    ) -> BoxFuture<'a, Result<usize>> {
        Box::pin(async move { self.cancel_orders_for_tokens(tokens).await })
    }

    fn sell<'a>(&'a self, token: U256, price: Decimal, size: Decimal) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.sell_at_price(token, price, size).await?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use polymarket_client_sdk::types::B256;
    use rust_decimal_macros::dec;
    use std::sync::Mutex;

    struct FakeTradingGateway {
        pair_result: Mutex<Option<Result<OrderPairResult>>>,
        cancel_result: Mutex<Option<Result<usize>>>,
    }

    impl TradingGateway for FakeTradingGateway {
        fn execute_pair<'a>(
            &'a self,
            _opportunity: &'a ArbitrageOpportunity,
            _quote: &'a ExecutionQuote,
        ) -> BoxFuture<'a, Result<OrderPairResult>> {
            Box::pin(async move {
                self.pair_result
                    .lock()
                    .unwrap()
                    .take()
                    .expect("scripted pair result")
            })
        }

        fn cancel_window_orders<'a>(
            &'a self,
            _tokens: &'a HashSet<U256>,
        ) -> BoxFuture<'a, Result<usize>> {
            Box::pin(async move {
                self.cancel_result
                    .lock()
                    .unwrap()
                    .take()
                    .expect("scripted cancel result")
            })
        }

        fn sell<'a>(
            &'a self,
            _token: U256,
            _price: Decimal,
            _size: Decimal,
        ) -> BoxFuture<'a, Result<()>> {
            Box::pin(async move { Ok(()) })
        }
    }

    fn opportunity() -> ArbitrageOpportunity {
        ArbitrageOpportunity {
            market_id: B256::ZERO,
            yes_token_id: U256::from(1),
            no_token_id: U256::from(2),
            yes_ask_price: dec!(0.45),
            no_ask_price: dec!(0.50),
            total_cost: dec!(0.95),
            profit_percentage: dec!(5),
            yes_size: dec!(10),
            no_size: dec!(10),
        }
    }

    fn quote() -> ExecutionQuote {
        ExecutionQuote {
            yes_price: dec!(0.45),
            no_price: dec!(0.50),
            size: dec!(10),
            total_cost: dec!(9.5),
            edge_per_share: dec!(0.05),
        }
    }

    #[tokio::test]
    async fn fake_gateway_models_partial_pair_and_cancel_failure() {
        let gateway: Box<dyn TradingGateway> = Box::new(FakeTradingGateway {
            pair_result: Mutex::new(Some(Ok(OrderPairResult {
                pair_id: "pair".into(),
                yes_order_id: "yes".into(),
                no_order_id: "no".into(),
                yes_filled: dec!(10),
                no_filled: dec!(0),
                yes_size: dec!(10),
                no_size: dec!(10),
                yes_price: dec!(0.45),
                no_price: dec!(0.50),
                success: false,
                write_uncertain: false,
            }))),
            cancel_result: Mutex::new(Some(Err(anyhow::anyhow!("cancel rejected")))),
        });
        let pair = gateway
            .execute_pair(&opportunity(), &quote())
            .await
            .unwrap();
        assert!(!pair.success);
        assert_eq!(pair.yes_filled, dec!(10));
        assert_eq!(pair.no_filled, dec!(0));
        assert!(gateway
            .cancel_window_orders(&HashSet::from([U256::from(1)]))
            .await
            .unwrap_err()
            .to_string()
            .contains("cancel rejected"));
    }

    #[tokio::test]
    async fn fake_gateway_models_full_fill_and_transport_failure() {
        let filled: Box<dyn TradingGateway> = Box::new(FakeTradingGateway {
            pair_result: Mutex::new(Some(Ok(OrderPairResult {
                pair_id: "filled".into(),
                yes_order_id: "yes".into(),
                no_order_id: "no".into(),
                yes_filled: dec!(10),
                no_filled: dec!(10),
                yes_size: dec!(10),
                no_size: dec!(10),
                yes_price: dec!(0.45),
                no_price: dec!(0.50),
                success: true,
                write_uncertain: false,
            }))),
            cancel_result: Mutex::new(Some(Ok(0))),
        });
        assert!(
            filled
                .execute_pair(&opportunity(), &quote())
                .await
                .unwrap()
                .success
        );

        let failed: Box<dyn TradingGateway> = Box::new(FakeTradingGateway {
            pair_result: Mutex::new(Some(Err(anyhow::anyhow!("second leg timeout")))),
            cancel_result: Mutex::new(Some(Ok(1))),
        });
        assert!(failed
            .execute_pair(&opportunity(), &quote())
            .await
            .unwrap_err()
            .to_string()
            .contains("second leg timeout"));
    }
}
