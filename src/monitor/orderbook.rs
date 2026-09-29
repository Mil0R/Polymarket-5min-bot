use anyhow::Result;
use chrono::Utc;
use dashmap::DashMap;
use futures::Stream;
use futures::StreamExt;
use polymarket_client_sdk::clob::ws::{
    types::response::{BookUpdate, OrderBookLevel, WsMessage},
    ChannelType, Client as WsClient,
};
use polymarket_client_sdk::types::{Decimal, B256, U256};
use polymarket_client_sdk::ws::connection::ConnectionState;
use std::collections::{HashMap, HashSet};
use std::pin::Pin;
use tracing::{debug, info};

use crate::market::MarketInfo;
use crate::trading::executor::WinnerBookTop;

/// Shorten B256 for logs: 0x + first 8 hex, e.g. 0xb91126b7..
#[inline]
fn short_b256(b: &B256) -> String {
    let s = format!("{b}");
    if s.len() > 12 {
        format!("{}..", &s[..10])
    } else {
        s
    }
}

/// Shorten U256 for logs: last 8 digits, e.g. ..67033653
#[inline]
fn short_u256(u: &U256) -> String {
    let s = format!("{u}");
    if s.len() > 12 {
        format!("..{}", &s[s.len().saturating_sub(8)..])
    } else {
        s
    }
}

pub struct OrderBookMonitor {
    ws_client: WsClient,
    books: DashMap<U256, BookUpdate>,
    book_received_at_ms: DashMap<U256, i64>,
    market_map: HashMap<B256, (U256, U256)>, // market_id -> (yes_token_id, no_token_id)
    extra_tokens: HashSet<U256>,
}

pub struct OrderBookPair {
    pub yes_book: BookUpdate,
    pub no_book: BookUpdate,
    pub market_id: B256,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarketWsConnectionState {
    Disconnected,
    Connecting,
    Connected,
    Reconnecting(u32),
}

/// Return the lowest non-empty ask without relying on wire ordering.
pub fn best_ask(book: &BookUpdate) -> Option<&OrderBookLevel> {
    book.asks
        .iter()
        .filter(|level| level.size > Decimal::ZERO)
        .min_by_key(|level| level.price)
}

/// Return the highest non-empty bid without relying on wire ordering.
pub fn best_bid(book: &BookUpdate) -> Option<&OrderBookLevel> {
    book.bids
        .iter()
        .filter(|level| level.size > Decimal::ZERO)
        .max_by_key(|level| level.price)
}

/// Decode a captured market WebSocket frame. Kept separate from the live stream so
/// protocol fixtures can be replayed deterministically in tests and diagnostics.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn replay_book_frame(frame: &str) -> Result<BookUpdate> {
    match serde_json::from_str::<WsMessage>(frame)? {
        WsMessage::Book(book) => Ok(book),
        _ => anyhow::bail!("WebSocket fixture is not an orderbook event"),
    }
}

impl OrderBookMonitor {
    pub fn new() -> Self {
        Self::with_ws_client(WsClient::default())
    }

    /// Reuse a WebSocket client across market windows so the SDK keeps one
    /// connection and reconnection task instead of creating one per window.
    pub fn with_ws_client(ws_client: WsClient) -> Self {
        Self {
            // Use unauthenticated client: orderbook is public, no auth needed
            // Only user data (orders, trades) requires auth
            ws_client,
            books: DashMap::new(),
            book_received_at_ms: DashMap::new(),
            market_map: HashMap::new(),
            extra_tokens: HashSet::new(),
        }
    }

    pub fn subscribe_recovery_token(&mut self, token_id: U256) {
        self.extra_tokens.insert(token_id);
    }

    /// Subscribe to new market
    pub fn subscribe_market(&mut self, market: &MarketInfo) -> Result<()> {
        // Record market mapping
        self.market_map
            .insert(market.market_id, (market.yes_token_id, market.no_token_id));

        info!(
            market_id = short_b256(&market.market_id),
            slug = %market.slug,
            yes = short_u256(&market.yes_token_id),
            no = short_u256(&market.no_token_id),
            "Subscribe to market orderbook"
        );

        Ok(())
    }

    /// Create orderbook subscription stream
    ///
    /// Note: Orderbook uses unauthenticated WebSocket; orderbook data is public.
    /// Only user data (order status, trade history) needs auth.
    pub fn create_orderbook_stream(
        &self,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<BookUpdate>> + Send + '_>>> {
        // Collect all token_ids to subscribe
        let token_ids = self.subscribed_token_ids();

        if token_ids.is_empty() {
            return Err(anyhow::anyhow!("No markets to subscribe"));
        }

        info!(
            token_count = token_ids.len(),
            "Creating orderbook stream (unauthenticated)"
        );

        // subscribe_orderbook does not need auth
        let stream = self.ws_client.subscribe_orderbook(token_ids)?;
        // Convert SDK Error to anyhow::Error
        let stream = stream.map(|result| result.map_err(|e| anyhow::anyhow!("{}", e)));
        Ok(Box::pin(stream))
    }

    /// Tokens registered for this window, including open-position recovery.
    pub fn subscribed_token_ids(&self) -> Vec<U256> {
        let mut token_ids: Vec<U256> = self
            .market_map
            .values()
            .flat_map(|(yes, no)| [*yes, *no])
            .collect();
        token_ids.extend(self.extra_tokens.iter().copied());
        token_ids.sort_unstable();
        token_ids.dedup();
        token_ids
    }

    /// Handle orderbook update
    pub fn handle_book_update(&self, book: BookUpdate) -> Option<OrderBookPair> {
        // Print top 5 bid/ask (debug)
        if !book.bids.is_empty() {
            let top_bids: Vec<String> = book
                .bids
                .iter()
                .take(5)
                .map(|b| format!("{}@{}", b.size, b.price))
                .collect();
            debug!(
                asset_id = %book.asset_id,
                "Top 5 bids: {}",
                top_bids.join(", ")
            );
        }
        if !book.asks.is_empty() {
            let top_asks: Vec<String> = book
                .asks
                .iter()
                .take(5)
                .map(|a| format!("{}@{}", a.size, a.price))
                .collect();
            debug!(
                asset_id = short_u256(&book.asset_id),
                "Top 5 asks: {}",
                top_asks.join(", ")
            );
        }

        // Update orderbook cache
        self.book_received_at_ms
            .insert(book.asset_id, Utc::now().timestamp_millis());
        self.books.insert(book.asset_id, book.clone());

        // Find which market this token belongs to; either side update returns OrderBookPair for arbitrage
        for (market_id, (yes_token, no_token)) in &self.market_map {
            if book.asset_id == *yes_token {
                if let Some(no_book) = self.books.get(no_token) {
                    return Some(OrderBookPair {
                        yes_book: book.clone(),
                        no_book: no_book.clone(),
                        market_id: *market_id,
                    });
                }
            } else if book.asset_id == *no_token {
                if let Some(yes_book) = self.books.get(yes_token) {
                    return Some(OrderBookPair {
                        yes_book: yes_book.clone(),
                        no_book: book.clone(),
                        market_id: *market_id,
                    });
                }
            }
        }

        None
    }

    /// Return the latest WebSocket top-of-book without requiring both sides.
    /// Near settlement a valid outcome book is frequently one-sided.
    pub fn winner_book_top(&self, token_id: U256) -> Option<WinnerBookTop> {
        let book = self.books.get(&token_id)?;
        let bid = best_bid(&book);
        let ask = best_ask(&book);
        if bid.is_none() && ask.is_none() {
            return None;
        }
        Some(WinnerBookTop {
            received_at_ms: self
                .book_received_at_ms
                .get(&token_id)
                .map(|value| *value)
                .unwrap_or(book.timestamp),
            exchange_updated_at_ms: book.timestamp,
            bid_price: bid.map(|level| level.price),
            ask_price: ask.map(|level| level.price),
            ask_size: ask.map(|level| level.size),
        })
    }

    /// Copy the latest WebSocket depth for research journaling. Callers must
    /// still check its exchange timestamp and local receive age.
    pub fn winner_book_snapshot(&self, token_id: U256) -> Option<BookUpdate> {
        self.books.get(&token_id).map(|book| book.clone())
    }

    pub fn connection_state(&self) -> MarketWsConnectionState {
        match self.ws_client.connection_state(ChannelType::Market) {
            ConnectionState::Disconnected => MarketWsConnectionState::Disconnected,
            ConnectionState::Connecting => MarketWsConnectionState::Connecting,
            ConnectionState::Connected { .. } => MarketWsConnectionState::Connected,
            ConnectionState::Reconnecting { attempt } => {
                MarketWsConnectionState::Reconnecting(attempt)
            }
            _ => MarketWsConnectionState::Disconnected,
        }
    }

    /// Drop cached snapshots while preserving the active subscription map.
    /// This prevents pairing a pre-reconnect leg with a post-reconnect leg.
    pub fn invalidate_books(&self) {
        self.books.clear();
        self.book_received_at_ms.clear();
    }

    /// Clear cached data and local market mappings; SDK subscriptions are
    /// switched by the caller after the next window has subscribed.
    pub fn clear(&mut self) {
        self.books.clear();
        self.book_received_at_ms.clear();
        self.market_map.clear();
        self.extra_tokens.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn replays_captured_orderbook_frame() {
        let frame = r#"{
            "event_type":"book",
            "asset_id":"42",
            "market":"0x0000000000000000000000000000000000000000000000000000000000000007",
            "timestamp":"1720000000123",
            "bids":[{"price":"0.44","size":"12.5"}],
            "asks":[{"price":"0.46","size":"9.25"}],
            "hash":"fixture-hash"
        }"#;
        let book = replay_book_frame(frame).unwrap();
        assert_eq!(book.asset_id, U256::from(42));
        assert_eq!(book.timestamp, 1_720_000_000_123);
        assert_eq!(book.bids[0].price, dec!(0.44));
        assert_eq!(book.asks[0].size, dec!(9.25));
    }

    #[test]
    fn rejects_non_book_websocket_frame() {
        let frame = r#"{
            "event_type":"last_trade_price",
            "asset_id":"42",
            "market":"0x0000000000000000000000000000000000000000000000000000000000000007",
            "price":"0.45",
            "side":"BUY",
            "size":"1",
            "fee_rate_bps":"0",
            "timestamp":"1720000000123"
        }"#;
        assert!(replay_book_frame(frame).is_err());
    }

    #[test]
    fn winner_top_preserves_valid_one_sided_book() {
        let frame = r#"{
            "event_type":"book",
            "asset_id":"42",
            "market":"0x0000000000000000000000000000000000000000000000000000000000000007",
            "timestamp":"1720000000123",
            "bids":[],
            "asks":[{"price":"0.97","size":"12"}],
            "hash":"fixture-hash"
        }"#;
        let monitor = OrderBookMonitor::new();
        monitor.handle_book_update(replay_book_frame(frame).unwrap());
        let top = monitor.winner_book_top(U256::from(42)).unwrap();
        assert_eq!(top.bid_price, None);
        assert_eq!(top.ask_price, Some(dec!(0.97)));
        assert_eq!(top.ask_size, Some(dec!(12)));
        assert_eq!(top.exchange_updated_at_ms, 1_720_000_000_123);
        assert!(top.received_at_ms > top.exchange_updated_at_ms);
    }

    #[test]
    fn best_prices_ignore_wire_order_and_empty_levels() {
        let level = |price, size| OrderBookLevel::builder().price(price).size(size).build();
        let book = BookUpdate::builder()
            .asset_id(U256::from(42))
            .market(B256::ZERO)
            .timestamp(0)
            .bids(vec![
                level(dec!(0.01), dec!(10)),
                level(dec!(0.44), dec!(5)),
                level(dec!(0.50), Decimal::ZERO),
            ])
            .asks(vec![
                level(dec!(0.99), dec!(10)),
                level(dec!(0.46), dec!(5)),
                level(dec!(0.40), Decimal::ZERO),
            ])
            .build();

        assert_eq!(best_bid(&book).unwrap().price, dec!(0.44));
        assert_eq!(best_ask(&book).unwrap().price, dec!(0.46));
    }
}
