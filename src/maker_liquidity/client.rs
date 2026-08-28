use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use dashmap::{DashMap, DashSet};
use futures::{stream, StreamExt};
use polymarket_client_sdk::clob::ws::{types::response::BookUpdate, Client as WsClient};
use polymarket_client_sdk::types::U256;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashSet;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::config::MakerResearchConfig;
use super::model::{
    classify_market, rank_candidates, BookLevel, BookSnapshot, MarketCandidate, MarketSelection,
    MarketTrade,
};

#[derive(Debug, Clone)]
pub struct MakerResearchClient {
    http: reqwest::Client,
    config: MakerResearchConfig,
    trade_cache: Arc<DashMap<String, (Instant, Vec<MarketTrade>)>>,
    book_cache: Arc<DashMap<String, BookSnapshot>>,
    websocket_subscribed_tokens: Arc<DashSet<String>>,
}

#[derive(Debug, Clone, Deserialize)]
struct RewardPage {
    #[serde(default)]
    data: Vec<RewardMarket>,
    #[serde(default)]
    next_cursor: String,
}

#[derive(Debug, Clone, Deserialize)]
struct RewardMarket {
    condition_id: String,
    rewards_max_spread: Value,
    rewards_min_size: Value,
    #[serde(default)]
    total_daily_rate: Option<Value>,
    #[serde(default)]
    native_daily_rate: Option<Value>,
    #[serde(default)]
    sponsored_daily_rate: Option<Value>,
    #[serde(default)]
    rewards_config: Vec<RewardConfig>,
}

#[derive(Debug, Clone, Deserialize)]
struct RewardConfig {
    #[serde(default)]
    rate_per_day: Value,
}

impl RewardMarket {
    fn max_spread(&self) -> Decimal {
        decimal_value(&self.rewards_max_spread)
    }

    fn min_size(&self) -> Decimal {
        decimal_value(&self.rewards_min_size)
    }

    fn daily_rate(&self) -> Decimal {
        if let Some(total) = self
            .total_daily_rate
            .as_ref()
            .map(decimal_value)
            .filter(|value| *value > Decimal::ZERO)
        {
            return total;
        }
        self.native_daily_rate
            .as_ref()
            .map(decimal_value)
            .unwrap_or_else(|| {
                self.rewards_config
                    .iter()
                    .map(|config| decimal_value(&config.rate_per_day))
                    .sum()
            })
            + self
                .sponsored_daily_rate
                .as_ref()
                .map(decimal_value)
                .unwrap_or_default()
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GammaMarket {
    condition_id: String,
    #[serde(default)]
    question: String,
    #[serde(default)]
    slug: String,
    #[serde(default)]
    end_date: Option<DateTime<Utc>>,
    #[serde(default)]
    active: bool,
    #[serde(default)]
    closed: bool,
    #[serde(default)]
    accepting_orders: bool,
    #[serde(default)]
    enable_order_book: bool,
    clob_token_ids: Value,
    #[serde(default)]
    outcome_prices: Value,
    #[serde(default)]
    liquidity: Value,
    #[serde(default)]
    volume_24hr: Value,
    #[serde(default)]
    spread: Value,
    #[serde(default)]
    order_price_min_tick_size: Value,
    #[serde(default)]
    fees_enabled: bool,
    #[serde(default)]
    fee_schedule: Option<GammaFeeSchedule>,
}

#[derive(Debug, Clone, Deserialize)]
struct GammaFeeSchedule {
    #[serde(default)]
    rate: Value,
    #[serde(default)]
    exponent: u32,
    #[serde(default, rename = "rebateRate")]
    rebate_rate: Value,
}

#[derive(Debug, Deserialize)]
struct ClobBookResponse {
    #[serde(default)]
    bids: Vec<ClobLevel>,
    #[serde(default)]
    asks: Vec<ClobLevel>,
    #[serde(default)]
    timestamp: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct ClobLevel {
    price: String,
    size: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DataTrade {
    asset: String,
    side: String,
    price: Value,
    size: Value,
    timestamp: i64,
    #[serde(default)]
    transaction_hash: String,
}

impl MakerResearchClient {
    pub fn new(config: MakerResearchConfig) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent("polypulse-maker-liquidity-research")
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(15))
            .build()
            .context("failed to build maker research HTTP client")?;
        Ok(Self {
            http,
            config,
            trade_cache: Arc::new(DashMap::new()),
            book_cache: Arc::new(DashMap::new()),
            websocket_subscribed_tokens: Arc::new(DashSet::new()),
        })
    }

    pub fn config(&self) -> &MakerResearchConfig {
        &self.config
    }

    pub async fn discover(&self) -> Result<Vec<MarketSelection>> {
        self.discover_excluding(&HashSet::new()).await
    }

    pub async fn discover_excluding(
        &self,
        excluded_condition_ids: &HashSet<String>,
    ) -> Result<Vec<MarketSelection>> {
        let mut rewards = self.fetch_reward_markets().await?;
        rewards.retain(|market| {
            market.daily_rate() >= self.config.min_daily_reward_usdc
                && market.min_size() <= self.config.max_rewards_min_size
                && market.max_spread() > Decimal::ZERO
        });
        rewards.sort_by(|left, right| {
            let left_efficiency = left.daily_rate() / left.min_size().max(dec!(1));
            let right_efficiency = right.daily_rate() / right.min_size().max(dec!(1));
            right_efficiency.cmp(&left_efficiency)
        });
        rewards.truncate(self.config.enrichment_limit);

        let concurrency = self.config.request_concurrency;
        let candidates = stream::iter(rewards.into_iter().map(|reward| async move {
            let market = self.fetch_gamma_market(&reward.condition_id).await?;
            Ok::<_, anyhow::Error>(market.and_then(|market| enrich_market(reward, market)))
        }))
        .buffer_unordered(concurrency)
        .filter_map(|result| async move {
            match result {
                Ok(candidate) => candidate,
                Err(error) => {
                    tracing::debug!(%error, "Maker market enrichment failed");
                    None
                }
            }
        })
        .collect::<Vec<_>>()
        .await;
        Ok(rank_candidates(
            candidates
                .into_iter()
                .filter(|candidate| !excluded_condition_ids.contains(&candidate.condition_id))
                .collect(),
            &self.config,
        ))
    }

    pub async fn fetch_book(&self, token_id: &str) -> Result<BookSnapshot> {
        if let Some(book) = self.book_cache.get(token_id) {
            let age_ms = Utc::now()
                .signed_duration_since(book.observed_at)
                .num_milliseconds();
            if age_ms <= self.config.websocket_book_max_age_ms {
                return Ok(book.clone());
            }
        }
        let response: ClobBookResponse = self
            .get_json(
                format!("{}/book", self.config.clob_url),
                vec![("token_id".to_string(), token_id.to_string())],
                "maker CLOB book",
            )
            .await?;
        let mut bids = parse_levels(response.bids);
        let mut asks = parse_levels(response.asks);
        bids.sort_by_key(|level| std::cmp::Reverse(level.price));
        asks.sort_by_key(|level| level.price);
        Ok(BookSnapshot {
            token_id: token_id.to_string(),
            observed_at: Utc::now(),
            exchange_timestamp_ms: response.timestamp.as_ref().and_then(value_i64),
            bids,
            asks,
        })
    }

    /// Fetch recent taker trades for one condition. A taker SELL at or below
    /// a hypothetical bid is public evidence that the bid was at least touched.
    pub async fn fetch_market_trades(&self, condition_id: &str) -> Result<Vec<MarketTrade>> {
        if let Some(cached) = self.trade_cache.get(condition_id) {
            if cached.value().0.elapsed() <= Duration::from_secs(self.config.trade_cache_secs) {
                return Ok(cached.value().1.clone());
            }
        }
        let response: Vec<DataTrade> = self
            .get_json(
                format!("{}/trades", self.config.data_url),
                vec![
                    ("market".to_string(), condition_id.to_string()),
                    ("limit".to_string(), "1000".to_string()),
                    ("takerOnly".to_string(), "true".to_string()),
                ],
                "maker Data API trades",
            )
            .await?;
        let trades = response
            .into_iter()
            .filter_map(|trade| {
                let price = decimal_value(&trade.price);
                let size = decimal_value(&trade.size);
                (price > Decimal::ZERO && size > Decimal::ZERO).then_some(MarketTrade {
                    asset: trade.asset,
                    side: trade.side,
                    price,
                    size,
                    timestamp: trade.timestamp,
                    transaction_hash: trade.transaction_hash,
                })
            })
            .collect::<Vec<_>>();
        self.trade_cache
            .insert(condition_id.to_string(), (Instant::now(), trades.clone()));
        Ok(trades)
    }

    pub fn start_book_stream(&self, selections: &[MarketSelection]) {
        if !self.config.websocket_books {
            return;
        }
        let mut token_ids = Vec::new();
        for token in selections.iter().flat_map(|selection| {
            [
                selection.market.yes_token_id.as_str(),
                selection.market.no_token_id.as_str(),
            ]
        }) {
            if self.websocket_subscribed_tokens.insert(token.to_string()) {
                match U256::from_str(token) {
                    Ok(token_id) => token_ids.push(token_id),
                    Err(error) => {
                        tracing::warn!(%error, token, "Maker WebSocket token id is invalid")
                    }
                }
            }
        }
        if token_ids.is_empty() {
            return;
        }
        let cache = Arc::clone(&self.book_cache);
        let retry_base_ms = self.config.retry_base_ms;
        tokio::spawn(async move {
            loop {
                let client = WsClient::default();
                match client.subscribe_orderbook(token_ids.clone()) {
                    Ok(stream) => {
                        futures::pin_mut!(stream);
                        while let Some(update) = stream.next().await {
                            match update {
                                Ok(book) => {
                                    let snapshot = websocket_book_snapshot(book);
                                    cache.insert(snapshot.token_id.clone(), snapshot);
                                }
                                Err(error) => {
                                    tracing::warn!(%error, "Maker WebSocket orderbook stream error");
                                    break;
                                }
                            }
                        }
                    }
                    Err(error) => tracing::warn!(%error, "Maker WebSocket subscription failed"),
                }
                tokio::time::sleep(Duration::from_millis(retry_base_ms.max(250))).await;
            }
        });
    }

    async fn fetch_reward_markets(&self) -> Result<Vec<RewardMarket>> {
        let mut all = Vec::new();
        let mut cursor: Option<String> = None;
        let mut seen = HashSet::new();
        for _ in 0..self.config.discovery_pages {
            let mut query = Vec::new();
            if let Some(value) = cursor.as_deref() {
                query.push(("next_cursor".to_string(), value.to_string()));
            }
            let page: RewardPage = self
                .get_json(
                    format!("{}/rewards/markets/current", self.config.clob_url),
                    query,
                    "maker reward market",
                )
                .await?;
            for market in page.data {
                if seen.insert(market.condition_id.clone()) {
                    all.push(market);
                }
            }
            if page.next_cursor.is_empty() || page.next_cursor == "LTE=" {
                break;
            }
            cursor = Some(page.next_cursor);
        }
        Ok(all)
    }

    async fn fetch_gamma_market(&self, condition_id: &str) -> Result<Option<GammaMarket>> {
        let markets: Vec<GammaMarket> = self
            .get_json(
                format!("{}/markets", self.config.gamma_url),
                vec![
                    ("condition_ids".to_string(), condition_id.to_string()),
                    ("limit".to_string(), "1".to_string()),
                ],
                "maker Gamma market",
            )
            .await?;
        Ok(markets.into_iter().next())
    }

    async fn get_json<T>(&self, url: String, query: Vec<(String, String)>, label: &str) -> Result<T>
    where
        T: DeserializeOwned,
    {
        let mut last_error = None;
        for attempt in 0..self.config.http_max_retries {
            let result = async {
                self.http
                    .get(&url)
                    .query(&query)
                    .send()
                    .await
                    .with_context(|| format!("{label} request failed"))?
                    .error_for_status()
                    .with_context(|| format!("{label} returned non-success"))?
                    .json::<T>()
                    .await
                    .with_context(|| format!("{label} response is invalid"))
            }
            .await;
            match result {
                Ok(value) => return Ok(value),
                Err(error) => last_error = Some(error),
            }
            if attempt + 1 < self.config.http_max_retries {
                let multiplier = 1_u64 << attempt.min(10);
                tokio::time::sleep(Duration::from_millis(
                    self.config.retry_base_ms.saturating_mul(multiplier),
                ))
                .await;
            }
        }
        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("{label} request failed")))
    }
}

fn websocket_book_snapshot(book: BookUpdate) -> BookSnapshot {
    let mut bids = book
        .bids
        .into_iter()
        .filter(|level| level.price > Decimal::ZERO && level.size > Decimal::ZERO)
        .map(|level| BookLevel {
            price: level.price,
            size: level.size,
        })
        .collect::<Vec<_>>();
    let mut asks = book
        .asks
        .into_iter()
        .filter(|level| level.price > Decimal::ZERO && level.size > Decimal::ZERO)
        .map(|level| BookLevel {
            price: level.price,
            size: level.size,
        })
        .collect::<Vec<_>>();
    bids.sort_by_key(|level| std::cmp::Reverse(level.price));
    asks.sort_by_key(|level| level.price);
    BookSnapshot {
        token_id: book.asset_id.to_string(),
        observed_at: Utc::now(),
        exchange_timestamp_ms: Some(book.timestamp),
        bids,
        asks,
    }
}

fn enrich_market(reward: RewardMarket, market: GammaMarket) -> Option<MarketCandidate> {
    if !market.active
        || market.closed
        || !market.accepting_orders
        || !market.enable_order_book
        || market.condition_id != reward.condition_id
    {
        return None;
    }
    let tokens = string_array(&market.clob_token_ids)?;
    if tokens.len() != 2 {
        return None;
    }
    let prices = decimal_array(&market.outcome_prices).unwrap_or_default();
    let yes_reference_price = prices.first().copied().unwrap_or(dec!(0.5));
    let fee = market.fee_schedule.as_ref();
    let rewards_max_spread_cents = reward.max_spread();
    let rewards_min_size = reward.min_size();
    let daily_reward_usdc = reward.daily_rate();
    let question = market.question;
    let slug = market.slug;
    Some(MarketCandidate {
        condition_id: reward.condition_id,
        category: classify_market(&question, &slug),
        question,
        slug,
        yes_token_id: tokens[0].clone(),
        no_token_id: tokens[1].clone(),
        end_date: market.end_date,
        rewards_max_spread_cents,
        rewards_min_size,
        daily_reward_usdc,
        liquidity_usdc: decimal_value(&market.liquidity),
        volume_24h_usdc: decimal_value(&market.volume_24hr),
        observed_spread: decimal_value(&market.spread),
        yes_reference_price,
        tick_size: decimal_value(&market.order_price_min_tick_size).max(dec!(0.001)),
        fees_enabled: market.fees_enabled,
        fee_rate: fee
            .map(|value| decimal_value(&value.rate))
            .unwrap_or_default(),
        fee_exponent: fee.map(|value| value.exponent).unwrap_or(1),
        maker_rebate_rate: fee
            .map(|value| decimal_value(&value.rebate_rate))
            .unwrap_or_default(),
    })
}

fn parse_levels(levels: Vec<ClobLevel>) -> Vec<BookLevel> {
    levels
        .into_iter()
        .filter_map(|level| {
            let price = Decimal::from_str(&level.price).ok()?;
            let size = Decimal::from_str(&level.size).ok()?;
            (price > Decimal::ZERO && size > Decimal::ZERO).then_some(BookLevel { price, size })
        })
        .collect()
}

fn decimal_value(value: &Value) -> Decimal {
    match value {
        Value::String(raw) => Decimal::from_str(raw).unwrap_or_default(),
        Value::Number(number) => Decimal::from_str(&number.to_string()).unwrap_or_default(),
        _ => Decimal::ZERO,
    }
}

fn value_i64(value: &Value) -> Option<i64> {
    match value {
        Value::String(raw) => raw.parse().ok(),
        Value::Number(number) => number.as_i64(),
        _ => None,
    }
}

fn string_array(value: &Value) -> Option<Vec<String>> {
    match value {
        Value::String(encoded) => serde_json::from_str(encoded).ok(),
        Value::Array(values) => Some(
            values
                .iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect(),
        ),
        _ => None,
    }
}

fn decimal_array(value: &Value) -> Option<Vec<Decimal>> {
    match value {
        Value::String(encoded) => {
            let values: Vec<String> = serde_json::from_str(encoded).ok()?;
            Some(
                values
                    .iter()
                    .filter_map(|raw| Decimal::from_str(raw).ok())
                    .collect(),
            )
        }
        Value::Array(values) => Some(values.iter().map(decimal_value).collect()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_gamma_encoded_arrays() {
        assert_eq!(
            string_array(&Value::String("[\"1\",\"2\"]".into())).unwrap(),
            vec!["1", "2"]
        );
        assert_eq!(
            decimal_array(&Value::String("[\"0.4\",\"0.6\"]".into())).unwrap(),
            vec![dec!(0.4), dec!(0.6)]
        );
    }

    #[test]
    fn reward_rate_falls_back_to_config_sum() {
        let reward = RewardMarket {
            condition_id: "0x1".into(),
            rewards_max_spread: Value::from(3),
            rewards_min_size: Value::from(20),
            total_daily_rate: None,
            native_daily_rate: None,
            sponsored_daily_rate: None,
            rewards_config: vec![
                RewardConfig {
                    rate_per_day: Value::from(2),
                },
                RewardConfig {
                    rate_per_day: Value::from(3),
                },
            ],
        };
        assert_eq!(reward.daily_rate(), dec!(5));
    }
}
