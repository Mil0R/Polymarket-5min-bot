//! Fetch user positions (Data API)

use crate::network_retry::{retry_read, ReadRetryPolicy};
use anyhow::{Context, Result};
use futures::future::BoxFuture;
use polymarket_client_sdk::data::types::request::PositionsRequest;
use polymarket_client_sdk::data::Client;
use polymarket_client_sdk::types::Address;

/// Position structure from Data API, re-exported for callers
pub use polymarket_client_sdk::data::types::response::Position;

pub trait PositionsTransport: Send + Sync {
    fn positions(&self, user: Address) -> BoxFuture<'_, Result<Vec<Position>>>;
}

struct SdkPositionsTransport {
    client: Client,
}

impl PositionsTransport for SdkPositionsTransport {
    fn positions(&self, user: Address) -> BoxFuture<'_, Result<Vec<Position>>> {
        Box::pin(async move {
            let request = PositionsRequest::builder().user(user).build();
            self.client
                .positions(&request)
                .await
                .context("Failed to fetch positions")
        })
    }
}

pub async fn get_positions_with_transport(
    transport: &dyn PositionsTransport,
    user: Address,
) -> Result<Vec<Position>> {
    retry_read(ReadRetryPolicy::default(), || transport.positions(user)).await
}

/// Read user address from `POLYMARKET_PROXY_ADDRESS`, call Data API for current open positions.
///
/// # Environment variables
///
/// - `POLYMARKET_PROXY_ADDRESS`: Required, Polymarket proxy wallet address (or EOA)
///
/// # Errors
///
/// - `POLYMARKET_PROXY_ADDRESS` not set
/// - Invalid address format
/// - Data API call failed
///
/// # Example
///
/// ```ignore
/// use polypulse::positions::{get_positions, Position};
///
/// let positions = get_positions().await?;
/// for p in positions {
///     println!("{}: {} @ {}", p.title, p.size, p.cur_price);
/// }
/// ```
pub async fn get_positions() -> Result<Vec<Position>> {
    dotenvy::dotenv().ok();
    let addr = std::env::var("POLYMARKET_PROXY_ADDRESS")
        .context("POLYMARKET_PROXY_ADDRESS not set")?
        .trim()
        .to_string();
    let user: Address = addr
        .parse()
        .context("POLYMARKET_PROXY_ADDRESS invalid format")?;
    let transport = SdkPositionsTransport {
        client: Client::default(),
    };
    get_positions_with_transport(&transport, user).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;
    use std::sync::Mutex;

    struct FakePositionsTransport {
        requested_user: Mutex<Option<Address>>,
        response: Mutex<Option<Vec<Position>>>,
    }

    impl PositionsTransport for FakePositionsTransport {
        fn positions(&self, user: Address) -> BoxFuture<'_, Result<Vec<Position>>> {
            Box::pin(async move {
                *self.requested_user.lock().unwrap() = Some(user);
                Ok(self.response.lock().unwrap().take().unwrap_or_default())
            })
        }
    }

    #[tokio::test]
    async fn injected_transport_receives_user_and_returns_positions() {
        let user: Address = "0x0000000000000000000000000000000000000001"
            .parse()
            .unwrap();
        let position: Position = serde_json::from_str(
            r#"{
                "proxyWallet":"0x0000000000000000000000000000000000000001",
                "asset":"42",
                "conditionId":"0x0000000000000000000000000000000000000000000000000000000000000007",
                "size":"10.5","avgPrice":"0.45","initialValue":"4.725",
                "currentValue":"5.25","cashPnl":"0.525","percentPnl":"11.11",
                "totalBought":"10.5","realizedPnl":"0","percentRealizedPnl":"0",
                "curPrice":"0.50","redeemable":false,"mergeable":true,
                "title":"Fixture","slug":"fixture","icon":"","eventSlug":"fixture",
                "eventId":"fixture","outcome":"Up","outcomeIndex":0,
                "oppositeOutcome":"Down","oppositeAsset":"43",
                "endDate":"2026-08-13","negativeRisk":false
            }"#,
        )
        .unwrap();
        let transport = FakePositionsTransport {
            requested_user: Mutex::new(None),
            response: Mutex::new(Some(vec![position])),
        };
        let positions = get_positions_with_transport(&transport, user)
            .await
            .unwrap();
        assert_eq!(positions[0].size, dec!(10.5));
        assert_eq!(*transport.requested_user.lock().unwrap(), Some(user));
    }
}
