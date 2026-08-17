//! Captured wire-format fixtures for upstream API compatibility checks.

use polymarket_client_sdk::clob::types::response::PostOrderResponse;
use polymarket_client_sdk::data::types::response::Position;
use polymarket_client_sdk::gamma::types::response::Market;
use rust_decimal_macros::dec;

#[test]
fn replays_clob_post_order_response() {
    let response: PostOrderResponse = serde_json::from_str(
        r#"{
            "errorMsg":"",
            "makingAmount":"4.50",
            "takingAmount":"10.00",
            "orderID":"fixture-order",
            "status":"matched",
            "success":true,
            "transactionHashes":[],
            "tradeIds":["fixture-trade"]
        }"#,
    )
    .unwrap();
    assert!(response.success);
    assert_eq!(response.order_id, "fixture-order");
    assert_eq!(response.taking_amount, dec!(10));
}

#[test]
fn replays_gamma_market_response() {
    let market: Market = serde_json::from_str(
        r#"{
            "id":"fixture-market",
            "question":"Will BTC go up?",
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
    assert_eq!(market.slug.as_deref(), Some("btc-updown-5m-1720000000"));
    assert_eq!(market.outcomes.unwrap(), vec!["Up", "Down"]);
    assert_eq!(market.clob_token_ids.unwrap().len(), 2);
}

#[test]
fn replays_data_api_position_response() {
    let position: Position = serde_json::from_str(
        r#"{
            "proxyWallet":"0x0000000000000000000000000000000000000001",
            "asset":"42",
            "conditionId":"0x0000000000000000000000000000000000000000000000000000000000000007",
            "size":"10.5",
            "avgPrice":"0.45",
            "initialValue":"4.725",
            "currentValue":"5.25",
            "cashPnl":"0.525",
            "percentPnl":"11.11",
            "totalBought":"10.5",
            "realizedPnl":"0",
            "percentRealizedPnl":"0",
            "curPrice":"0.50",
            "redeemable":false,
            "mergeable":true,
            "title":"Fixture market",
            "slug":"fixture-market",
            "icon":"",
            "eventSlug":"fixture-event",
            "eventId":"fixture-event-id",
            "outcome":"Up",
            "outcomeIndex":0,
            "oppositeOutcome":"Down",
            "oppositeAsset":"43",
            "endDate":"2026-08-13",
            "negativeRisk":false
        }"#,
    )
    .unwrap();
    assert_eq!(position.size, dec!(10.5));
    assert_eq!(position.outcome_index, 0);
    assert!(position.mergeable);
}
