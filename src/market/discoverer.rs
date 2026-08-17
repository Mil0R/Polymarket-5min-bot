use crate::network_retry::{retry_read, ReadRetryPolicy};
use anyhow::Result;
use chrono::{DateTime, Utc};
use futures::future::BoxFuture;
use polymarket_client_sdk::gamma::{
    types::{request::MarketsRequest, response::Market},
    Client,
};
use polymarket_client_sdk::types::{B256, U256};
use std::sync::Arc;
use tracing::{info, warn};

/// 5-minute window duration in seconds (kept for tests that hard-code 5-minute fixtures)
#[allow(dead_code)]
pub const FIVE_MIN_SECS: i64 = 300;

/// Window duration in seconds for a period expressed in minutes.
pub fn window_seconds(period_minutes: u64) -> i64 {
    (period_minutes.max(1) as i64) * 60
}

/// Period suffix used in Polymarket crypto slugs, e.g. "5m", "15m".
pub fn period_slug_suffix(period_minutes: u64) -> String {
    format!("{}m", period_minutes)
}

#[derive(Debug, Clone)]
pub struct MarketInfo {
    pub market_id: B256,
    pub slug: String,
    pub yes_token_id: U256,
    pub no_token_id: U256,
    pub title: String,
    pub end_date: DateTime<Utc>,
    pub crypto_symbol: String,
}

pub struct MarketDiscoverer {
    gamma_transport: Arc<dyn GammaTransport>,
    crypto_symbols: Vec<String>,
    period_minutes: u64,
}

pub trait GammaTransport: Send + Sync {
    fn markets(&self, slugs: Vec<String>) -> BoxFuture<'_, Result<Vec<Market>>>;
}

struct SdkGammaTransport {
    client: Client,
}

impl GammaTransport for SdkGammaTransport {
    fn markets(&self, slugs: Vec<String>) -> BoxFuture<'_, Result<Vec<Market>>> {
        Box::pin(async move {
            let request = MarketsRequest::builder().slug(slugs).build();
            Ok(self.client.markets(&request).await?)
        })
    }
}

impl MarketDiscoverer {
    pub fn new(crypto_symbols: Vec<String>, period_minutes: u64) -> Self {
        Self {
            gamma_transport: Arc::new(SdkGammaTransport {
                client: Client::default(),
            }),
            crypto_symbols,
            period_minutes,
        }
    }

    #[allow(dead_code)] // Test/embedding injection point; main uses the SDK transport.
    pub fn with_transport(
        crypto_symbols: Vec<String>,
        period_minutes: u64,
        gamma_transport: Arc<dyn GammaTransport>,
    ) -> Self {
        Self {
            gamma_transport,
            crypto_symbols,
            period_minutes,
        }
    }

    #[allow(dead_code)] // convenience accessor, kept for embedding callers
    pub fn period_minutes(&self) -> u64 {
        self.period_minutes
    }

    /// Current window start timestamp (UTC), aligned to the configured period.
    pub fn calculate_current_window_timestamp(&self, now: DateTime<Utc>) -> i64 {
        let ts = now.timestamp();
        let secs = window_seconds(self.period_minutes);
        (ts / secs) * secs
    }

    /// Next window start timestamp (UTC)
    pub fn calculate_next_window_timestamp(&self, now: DateTime<Utc>) -> i64 {
        let ts = now.timestamp();
        let secs = window_seconds(self.period_minutes);
        ((ts / secs) + 1) * secs
    }

    /// Generate market slugs, e.g. btc-updown-5m-1770972300
    pub fn generate_market_slugs(&self, timestamp: i64) -> Vec<String> {
        let suffix = period_slug_suffix(self.period_minutes);
        self.crypto_symbols
            .iter()
            .map(|symbol| format!("{}-updown-{}-{}", symbol, suffix, timestamp))
            .collect()
    }

    /// Fetch 5-minute markets for given timestamp
    pub async fn get_markets_for_timestamp(&self, timestamp: i64) -> Result<Vec<MarketInfo>> {
        // Generate slugs for all crypto symbols
        let slugs = self.generate_market_slugs(timestamp);

        info!(timestamp, slug_count = slugs.len(), "Querying markets");

        match retry_read(ReadRetryPolicy::default(), || {
            self.gamma_transport.markets(slugs.clone())
        })
        .await
        {
            Ok(markets) => {
                // Filter and parse markets
                let valid_markets: Vec<MarketInfo> = markets
                    .into_iter()
                    .filter_map(|market| self.parse_market(market))
                    .collect();

                info!(count = valid_markets.len(), "Found valid markets");
                Ok(valid_markets)
            }
            Err(e) => {
                warn!(error = %e, timestamp = timestamp, "Market query failed, markets may not exist yet");
                Ok(Vec::new())
            }
        }
    }

    /// Parse market, extract YES and NO token_ids
    fn parse_market(&self, market: Market) -> Option<MarketInfo> {
        // Check market is active, orderbook enabled and accepting orders
        if !market.active.unwrap_or(false)
            || !market.enable_order_book.unwrap_or(false)
            || !market.accepting_orders.unwrap_or(false)
        {
            return None;
        }

        // Check outcomes are ["Up", "Down"]
        let outcomes = market.outcomes.as_ref()?;

        if outcomes.len() != 2
            || !outcomes.contains(&"Up".to_string())
            || !outcomes.contains(&"Down".to_string())
        {
            return None;
        }

        // Get clobTokenIds
        let token_ids = market.clob_token_ids.as_ref()?;

        if token_ids.len() != 2 {
            return None;
        }

        // First is Up token_id, second is Down
        let yes_token_id = token_ids[0];
        let no_token_id = token_ids[1];

        // Get conditionId
        let market_id = market.condition_id?;

        // Extract crypto symbol from slug
        let slug = market.slug.as_ref()?;
        let crypto_symbol = slug.split('-').next().unwrap_or("").to_string();

        // Get endDate
        let end_date = market.end_date?;

        Some(MarketInfo {
            market_id,
            slug: slug.clone(),
            yes_token_id,
            no_token_id,
            title: market.question.unwrap_or_default(),
            end_date,
            crypto_symbol,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct FakeGammaTransport {
        requested: Mutex<Vec<Vec<String>>>,
        response: Mutex<Option<Vec<Market>>>,
    }

    impl GammaTransport for FakeGammaTransport {
        fn markets(&self, slugs: Vec<String>) -> BoxFuture<'_, Result<Vec<Market>>> {
            Box::pin(async move {
                self.requested.lock().unwrap().push(slugs);
                Ok(self.response.lock().unwrap().take().unwrap_or_default())
            })
        }
    }

    #[tokio::test]
    async fn injected_transport_receives_generated_slugs_and_returns_parsed_markets() {
        let market: Market = serde_json::from_str(
            r#"{
                "id":"fixture",
                "question":"BTC Up or Down",
                "conditionId":"0x0000000000000000000000000000000000000000000000000000000000000007",
                "slug":"btc-updown-5m-1720000000",
                "endDate":"2026-08-13T12:05:00Z",
                "outcomes":"[\"Up\",\"Down\"]",
                "active":true,
                "enableOrderBook":true,
                "acceptingOrders":true,
                "clobTokenIds":"[\"42\",\"43\"]"
            }"#,
        )
        .unwrap();
        let transport = Arc::new(FakeGammaTransport {
            requested: Mutex::new(Vec::new()),
            response: Mutex::new(Some(vec![market])),
        });
        let discoverer =
            MarketDiscoverer::with_transport(vec!["btc".to_string()], 5, transport.clone());
        let markets = discoverer
            .get_markets_for_timestamp(1_720_000_000)
            .await
            .unwrap();
        assert_eq!(markets.len(), 1);
        assert_eq!(markets[0].yes_token_id, U256::from(42));
        assert_eq!(
            transport.requested.lock().unwrap()[0],
            vec!["btc-updown-5m-1720000000"]
        );
    }
}
