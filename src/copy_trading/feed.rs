//! Real Polymarket data ingestion for the copy-trading strategy.
//!
//! This module closes the P0 gap identified in review: previously the strategy
//! had NO real signal source — signals had to be hand-fed and wallet profiles
//! were built from fabricated demo records. Here we pull real data from:
//!
//! - `data-api.polymarket.com`  wallet trades / positions / closed positions
//! - `gamma-api.polymarket.com` market metadata (question, expiry, volume)
//! - `clob.polymarket.com`      live order book snapshots (public /book)
//!
//! Honest limitations (documented):
//! - The data API does not expose maker/taker per trade, so `is_maker` /
//!   `is_arbitrage_leg` default to false in wallet profiles.
//! - Closed-position P&L is used as the outcome proxy for Brier scoring
//!   (realized P&L > 0 → the bought side resolved in the wallet's favor).
//! - Gamma market metadata does not expose NegRisk directly, so `is_neg_risk`
//!   is conservatively treated as `false` (binary-only filter still applies).

use crate::copy_trading::types::{
    BookLevel, DomainTag, MarketMetadata, OrderBookSnapshot, OutcomeSide, SignalAction,
    SignalEvent, WalletTradeRecord,
};
use crate::network_retry::{retry_read, ReadRetryPolicy};
use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use dashmap::DashMap;
use polymarket_client_sdk::data::types::request::{PositionsRequest, TradesRequest};
use polymarket_client_sdk::data::types::response::{Position, Trade};
use polymarket_client_sdk::data::types::Side;
use polymarket_client_sdk::data::Client as DataClient;
use polymarket_client_sdk::gamma::types::request::MarketsRequest;
use polymarket_client_sdk::gamma::types::response::Market as GammaMarket;
use polymarket_client_sdk::gamma::Client as GammaClient;
use polymarket_client_sdk::types::{Address, B256};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tracing::{debug, info, warn};

pub const DATA_API_URL: &str = "https://data-api.polymarket.com";
pub const GAMMA_API_URL: &str = "https://gamma-api.polymarket.com";
pub const CLOB_API_URL: &str = "https://clob.polymarket.com";

/// Formats a condition ID (B256) as the canonical lowercase `0x...` string.
pub fn condition_id_str(cond: &B256) -> String {
    format!("0x{:x}", cond)
}

/// Formats an outcome token asset (U256) as its decimal string.
pub fn token_id_str(asset: &polymarket_client_sdk::types::U256) -> String {
    asset.to_string()
}

/// Maps a Polymarket outcome label to our `OutcomeSide`.
///
/// Besides the literal "Yes"/"No" labels, directional binary markets use
/// "Up"/"Down" (or similar) pairs for their two outcome tokens — these map to
/// the YES/NO sides of a binary directional bet. Markets with more than two
/// outcomes (categorical) fall through to `Other` and are rejected by the
/// binary-only market filter.
pub fn outcome_from_label(label: &str) -> OutcomeSide {
    match label.trim().to_ascii_lowercase().as_str() {
        "yes" | "up" => OutcomeSide::Yes,
        "no" | "down" => OutcomeSide::No,
        other => OutcomeSide::Other(other.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Domain classification
// ---------------------------------------------------------------------------

/// Keyword-based classifier mapping market titles/slugs to strategy domains.
#[derive(Debug, Clone, Default)]
pub struct DomainClassifier;

impl DomainClassifier {
    pub fn classify(text: &str) -> DomainTag {
        let t = text.to_ascii_lowercase();

        let macro_kw = [
            "fed",
            "interest rate",
            "rate cut",
            "rate hike",
            "cpi",
            "inflation",
            "ecb",
            "central bank",
            "unemployment",
            "gdp",
            "recession",
            "jobs report",
            "fomc",
            "powell",
            "treasury",
            "yield",
        ];
        let geo_kw = [
            "war",
            "ceasefire",
            "invasion",
            "nato",
            "russia",
            "ukraine",
            "israel",
            "iran",
            "gaza",
            "china",
            "taiwan",
            "border",
            "sanction",
            "missile",
            "nuclear",
            "military",
        ];
        let tech_kw = [
            "sec",
            "regul",
            "law",
            "bill",
            "court",
            "lawsuit",
            "antitrust",
            "congress",
            "senate",
            "ban",
            "approval",
            "fda",
            "tiktok",
            "openai",
            "google",
            "meta",
            "apple",
            "nvidia",
            "ai",
        ];
        let crypto_kw = [
            "bitcoin",
            "btc",
            "ethereum",
            "eth",
            "solana",
            "sol",
            "xrp",
            "crypto",
            "stablecoin",
            "usdc",
            "etf",
            "halving",
            "coinbase",
            "binance",
            "blockchain",
            "defi",
        ];
        let election_kw = [
            "elect",
            "primary",
            "poll",
            "president",
            "senator",
            "governor",
            "mayor",
            "candidate",
            "debate",
            "nominee",
            "trump",
            "harris",
            "vance",
            "walz",
        ];
        let science_kw = [
            "launch",
            "nasa",
            "spacex",
            "rocket",
            "satellite",
            "mars",
            "moon",
            "climate",
            "temperature",
            "weather",
            "hurricane",
            "earthquake",
            "volcano",
            "pandemic",
            "vaccine",
            "covid",
        ];
        let sports_kw = [
            "nba",
            "nfl",
            "mlb",
            "nhl",
            "soccer",
            "football",
            "basketball",
            "tennis",
            "f1",
            "super bowl",
            "world cup",
            "championship",
            "draft",
            "goal",
            "match",
            "game",
            "series",
            "olympics",
            "oscar",
            "grammy",
            "box office",
            "movie",
            "album",
        ];

        let has = |kws: &[&str]| kws.iter().any(|k| t.contains(k));

        if has(&macro_kw) {
            DomainTag::MacroInterestRate
        } else if has(&election_kw) {
            DomainTag::Elections
        } else if has(&geo_kw) {
            DomainTag::Geopolitics
        } else if has(&crypto_kw) {
            DomainTag::Crypto
        } else if has(&tech_kw) {
            DomainTag::TechRegulation
        } else if has(&science_kw) {
            DomainTag::ScienceClimate
        } else if has(&sports_kw) {
            DomainTag::PopCultureSports
        } else {
            DomainTag::General
        }
    }
}

// ---------------------------------------------------------------------------
// Market metadata (Gamma API)
// ---------------------------------------------------------------------------

/// Fetches market metadata from the Gamma API for a given condition ID.
#[derive(Debug, Clone)]
pub struct MarketMetadataFetcher {
    gamma: GammaClient,
}

impl MarketMetadataFetcher {
    pub fn new() -> Result<Self> {
        Ok(Self {
            gamma: GammaClient::new(GAMMA_API_URL)
                .context("failed to construct gamma API client")?,
        })
    }

    pub async fn fetch_by_condition(&self, condition_id: &str) -> Result<Option<MarketMetadata>> {
        let cond: B256 = condition_id.parse().context("invalid condition id")?;
        let request = MarketsRequest::builder()
            .condition_ids(vec![cond])
            .limit(1)
            .build();
        let markets = self
            .gamma
            .markets(&request)
            .await
            .context("gamma markets request failed")?;
        Ok(markets.first().map(gamma_market_to_metadata))
    }
}

fn gamma_market_to_metadata(m: &GammaMarket) -> MarketMetadata {
    let outcomes = m.outcomes.clone().unwrap_or_default();
    let is_active = m.active.unwrap_or(false) && !m.closed.unwrap_or(false);
    let condition = condition_id_str(&m.condition_id.unwrap_or_default());
    MarketMetadata {
        market_id: condition.clone(),
        condition_id: condition,
        question: m.question.clone().unwrap_or_default(),
        domain: DomainClassifier::classify(&format!(
            "{} {}",
            m.question.clone().unwrap_or_default(),
            m.slug.clone().unwrap_or_default()
        )),
        outcome_count: outcomes.len().max(2),
        is_neg_risk: m.neg_risk.unwrap_or(false),
        settlement_time: m.end_date.unwrap_or_else(Utc::now),
        volume_24h_usdc: m.volume_24hr.unwrap_or(dec!(0.0)),
        is_active,
    }
}

// ---------------------------------------------------------------------------
// Order book (CLOB public /book endpoint)
// ---------------------------------------------------------------------------

/// Minimal serde shape of the CLOB `/book` response.
/// Note: `timestamp` arrives as a JSON string of milliseconds since epoch.
#[derive(Debug, serde::Deserialize)]
struct ClobBookResponse {
    #[serde(default)]
    bids: Vec<ClobLevel>,
    #[serde(default)]
    asks: Vec<ClobLevel>,
    #[serde(default)]
    timestamp: Option<serde_json::Value>,
}

#[derive(Debug, serde::Deserialize)]
struct ClobLevel {
    price: String,
    size: String,
}

/// Fetches live order book snapshots from the public CLOB endpoint (no auth).
#[derive(Debug, Clone)]
pub struct BookFetcher {
    client: reqwest::Client,
}

impl BookFetcher {
    pub fn new() -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .user_agent("polypulse-copy-trader")
                .build()
                .context("failed to build http client")?,
        })
    }

    pub async fn fetch(&self, token_id: &str, market_id: &str) -> Result<OrderBookSnapshot> {
        let url = format!("{}/book?token_id={}", CLOB_API_URL, token_id);
        let resp: ClobBookResponse = self
            .client
            .get(&url)
            .send()
            .await
            .context("clob /book request failed")?
            .error_for_status()
            .context("clob /book non-2xx response")?
            .json()
            .await
            .context("clob /book parse failed")?;

        let mut bids: Vec<BookLevel> = resp
            .bids
            .into_iter()
            .filter_map(|l| {
                let price: Decimal = l.price.parse().ok()?;
                let size: Decimal = l.size.parse().ok()?;
                (price > Decimal::ZERO && size > Decimal::ZERO).then_some(BookLevel { price, size })
            })
            .collect();
        // Sorted descending (best bid first)
        bids.sort_by_key(|l| std::cmp::Reverse(l.price));

        let mut asks: Vec<BookLevel> = resp
            .asks
            .into_iter()
            .filter_map(|l| {
                let price: Decimal = l.price.parse().ok()?;
                let size: Decimal = l.size.parse().ok()?;
                (price > Decimal::ZERO && size > Decimal::ZERO).then_some(BookLevel { price, size })
            })
            .collect();
        // Sorted ascending (best ask first)
        asks.sort_by_key(|l| l.price);

        // `timestamp` is a JSON string/number of MILLISECONDS since epoch.
        let timestamp = match resp.timestamp {
            Some(serde_json::Value::String(s)) => s
                .parse::<i64>()
                .ok()
                .and_then(|ms| DateTime::from_timestamp(ms / 1000, 0)),
            Some(serde_json::Value::Number(n)) => n
                .as_i64()
                .and_then(|ms| DateTime::from_timestamp(ms / 1000, 0)),
            _ => None,
        }
        .unwrap_or_else(Utc::now);

        Ok(OrderBookSnapshot {
            market_id: market_id.to_string(),
            token_id: token_id.to_string(),
            bids,
            asks,
            timestamp,
        })
    }
}

// ---------------------------------------------------------------------------
// Trade feed (Data API) -> SignalEvents
// ---------------------------------------------------------------------------

/// Polls the Data API for fresh wallet trades and converts them into signals.
///
/// Deduplication: each (wallet, tx hash, asset, size, price) is seen at most
/// once, so overlapping poll windows never double-emit a signal.
#[derive(Debug, Clone)]
pub struct TradeFeed {
    data: DataClient,
    seen: Arc<DashMap<String, HashSet<String>>>,
    max_signal_age_seconds: i64,
}

impl TradeFeed {
    pub fn new() -> Result<Self> {
        Self::with_max_signal_age_seconds(120)
    }

    /// Creates a feed that seeds its deduplication state from the latest API
    /// page while only emitting observations that are still actionable. This
    /// prevents every process restart from replaying up to 100 historical
    /// trades per wallet through Gamma and CLOB.
    pub fn with_max_signal_age_seconds(max_signal_age_seconds: i64) -> Result<Self> {
        Ok(Self {
            data: DataClient::default(),
            seen: Arc::new(DashMap::new()),
            max_signal_age_seconds: max_signal_age_seconds.max(1),
        })
    }

    /// Fetches the most recent trades for a wallet and returns only NEW signals.
    pub async fn poll_wallet(&self, wallet: &str, now: DateTime<Utc>) -> Result<Vec<SignalEvent>> {
        let user: Address = wallet.parse().context("invalid target wallet address")?;
        let request = TradesRequest::builder()
            .user(user)
            .limit(100)
            .map_err(|e| anyhow::anyhow!("trade limit bound: {e}"))?
            .taker_only(false)
            .build();
        let trades = self
            .data
            .trades(&request)
            .await
            .context("data-api trades request failed")?;

        let mut seen_set = self.seen.entry(wallet.to_string()).or_default();

        // Cap the seen-set so it cannot grow without bound over long runs.
        if seen_set.len() > 2000 {
            seen_set.clear();
        }
        let new_trades: Vec<_> = trades
            .iter()
            .filter(|trade| seen_set.insert(trade_dedup_key(trade)))
            .cloned()
            .collect();
        drop(seen_set);

        // Only query positions when an unseen SELL is still fresh enough to
        // affect the strategy. Previously any historical SELL in the latest
        // 100 rows caused a positions request on every poll, even when there
        // was no new activity, creating avoidable rate limits.
        let has_actionable_sell = new_trades.iter().any(|trade| {
            is_actionable_sell(
                &trade.side,
                trade.timestamp,
                now,
                self.max_signal_age_seconds,
            )
        });

        // For actionable SELLs, compare the observed sale with the wallet's
        // remaining token balance. This turns an ambiguous trade into a
        // proportional reduction instead of treating every sell as a full
        // liquidation.
        let (current_positions, sell_positions_available): (HashMap<String, Decimal>, bool) =
            if has_actionable_sell {
                let request = PositionsRequest::builder().user(user).build();
                match self.data.positions(&request).await {
                    Ok(positions) => (
                        positions
                            .into_iter()
                            .map(|p| (token_id_str(&p.asset), p.size))
                            .collect(),
                        true,
                    ),
                    Err(e) => {
                        warn!(wallet = %wallet, error = %e, "Could not size target SELL proportion; exits will be skipped safely");
                        (HashMap::new(), false)
                    }
                }
            } else {
                (HashMap::new(), true)
            };

        let mut fresh = Vec::new();
        let mut stale_seeded = 0usize;
        let mut skipped_unsized_sells = 0usize;

        for trade in &new_trades {
            let remaining = current_positions.get(&token_id_str(&trade.asset)).copied();
            if let Some(sig) = self.trade_to_signal(trade, now, remaining) {
                let age = now.signed_duration_since(sig.timestamp).num_seconds();
                if age > self.max_signal_age_seconds || age < -30 {
                    // Keep the id in `seen`: this is startup baseline data,
                    // not a live event that should be retried next poll.
                    stale_seeded += 1;
                } else if trade.side == Side::Sell && !sell_positions_available {
                    skipped_unsized_sells += 1;
                } else {
                    fresh.push(sig);
                }
            }
        }

        if stale_seeded > 0 {
            info!(
                wallet = %wallet,
                stale_signals = stale_seeded,
                max_age_seconds = self.max_signal_age_seconds,
                "Seeded stale wallet history without replaying it"
            );
        }

        if !fresh.is_empty() {
            info!(
                wallet = %wallet,
                fresh_signals = fresh.len(),
                "Trade feed produced new signals for wallet"
            );
        }
        if skipped_unsized_sells > 0 {
            warn!(
                wallet = %wallet,
                skipped_signals = skipped_unsized_sells,
                "Skipped fresh SELL signals because target position sizing was unavailable"
            );
        }

        Ok(fresh)
    }

    fn trade_to_signal(
        &self,
        t: &Trade,
        now: DateTime<Utc>,
        remaining_position: Option<Decimal>,
    ) -> Option<SignalEvent> {
        let price = t.price;
        let size = t.size;
        if price <= Decimal::ZERO || size <= Decimal::ZERO {
            return None;
        }

        let notional = (size * price).round_dp(2);
        let outcome = outcome_from_label(&t.outcome);
        // Only binary (Yes/No) markets are tradable per Module 2.
        if !matches!(outcome, OutcomeSide::Yes | OutcomeSide::No) {
            return None;
        }

        let action = match t.side {
            Side::Buy => SignalAction::OpenBuy,
            Side::Sell => SignalAction::CloseSell,
            _ => return None, // Unknown / future side values
        };
        let target_reduction_pct = if t.side == Side::Sell {
            remaining_position.and_then(|remaining| {
                let before = remaining.max(Decimal::ZERO) + size;
                (before > Decimal::ZERO).then_some((size / before).min(Decimal::ONE))
            })
        } else {
            None
        };

        let timestamp = DateTime::from_timestamp(t.timestamp, 0).unwrap_or(now);
        let domain = DomainClassifier::classify(&format!("{} {}", t.title, t.slug));

        Some(SignalEvent {
            signal_id: format!("sig-{:x}-{}-{}", t.transaction_hash, t.asset, t.timestamp),
            source_wallet: wallet_str(&t.proxy_wallet),
            market_id: condition_id_str(&t.condition_id),
            token_id: token_id_str(&t.asset),
            outcome,
            domain,
            action,
            target_vwap: price,
            target_shares: size,
            target_notional_usdc: notional,
            target_reduction_pct,
            timestamp,
        })
    }
}

fn trade_dedup_key(t: &Trade) -> String {
    format!(
        "{:x}-{}-{}-{}",
        t.transaction_hash, t.asset, t.size, t.price
    )
}

fn is_actionable_sell(
    side: &Side,
    timestamp_seconds: i64,
    now: DateTime<Utc>,
    max_age_seconds: i64,
) -> bool {
    if *side != Side::Sell {
        return false;
    }
    let timestamp = DateTime::from_timestamp(timestamp_seconds, 0).unwrap_or(now);
    let age = now.signed_duration_since(timestamp).num_seconds();
    age >= -30 && age <= max_age_seconds.max(1)
}

fn wallet_str(addr: &polymarket_client_sdk::types::Address) -> String {
    format!("0x{:x}", addr)
}

// ---------------------------------------------------------------------------
// Wallet history (Data API) -> WalletTradeRecord for whitelist evaluation
// ---------------------------------------------------------------------------

/// Lenient decimal that accepts both JSON strings ("0.5405") and numbers (0.5405).
#[derive(Debug, Clone, Copy)]
struct FlexDecimal(Decimal);

impl<'de> serde::Deserialize<'de> for FlexDecimal {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(serde::Deserialize)]
        #[serde(untagged)]
        enum Repr {
            S(String),
            F(f64),
            I(i64),
            U(u64),
        }
        match Repr::deserialize(d)? {
            Repr::S(s) => s
                .parse::<Decimal>()
                .map(FlexDecimal)
                .map_err(serde::de::Error::custom),
            Repr::F(f) => Decimal::from_f64_retain(f)
                .map(FlexDecimal)
                .ok_or_else(|| serde::de::Error::custom("invalid float")),
            Repr::I(i) => Ok(FlexDecimal(Decimal::from(i))),
            Repr::U(u) => Ok(FlexDecimal(Decimal::from(u))),
        }
    }
}

/// Raw defensive shape of the `/closed-positions` response.
///
/// The SDK's typed `ClosedPosition` uses `DateTime<Utc>` for `endDate`, but the
/// API returns date-only strings (e.g. `"2026-04-14"`) which the SDK cannot
/// parse — so we fetch and parse this endpoint ourselves, tolerating both
/// date and numeric formats. Some fields are kept for API-shape documentation
/// even if the current evaluation funnel does not consume them.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
struct RawClosedPosition {
    proxy_wallet: String,
    asset: String,
    condition_id: String,
    avg_price: FlexDecimal,
    total_bought: FlexDecimal,
    realized_pnl: FlexDecimal,
    cur_price: FlexDecimal,
    timestamp: i64,
    title: String,
    slug: String,
    outcome: String,
    outcome_index: i32,
    opposite_outcome: String,
    opposite_asset: String,
    #[serde(default)]
    end_date: Option<String>,
}

/// Builds a wallet's historical trade profile from the Data API.
#[derive(Debug, Clone)]
pub struct WalletHistoryFetcher {
    data: DataClient,
    http: reqwest::Client,
}

fn closed_position_page_offsets(
    offset: i32,
    page_size: i32,
    concurrency: i32,
    cap: i32,
) -> Vec<i32> {
    (offset..(offset + concurrency * page_size).min(cap))
        .step_by(page_size as usize)
        .collect()
}

impl WalletHistoryFetcher {
    pub fn new() -> Result<Self> {
        Ok(Self {
            data: DataClient::default(),
            http: reqwest::Client::builder()
                .user_agent("polypulse-copy-trader")
                .build()
                .context("failed to build http client")?,
        })
    }

    /// Fetches closed + open positions and converts them into evaluation records.
    pub async fn fetch_records(&self, wallet: &str) -> Result<Vec<WalletTradeRecord>> {
        let user: Address = wallet.parse().context("invalid wallet address")?;

        // Closed positions via raw HTTP, fetched with bounded concurrency so large
        // histories (thousands of positions) do not take minutes to load.
        let mut records = Vec::new();
        let closed = self.fetch_closed_positions_paged(wallet).await?;
        for cp in closed {
            records.push(self.raw_closed_position_to_record(&cp));
        }

        // Open positions (current holdings; contribute entry/domain stats).
        let request = PositionsRequest::builder().user(user).build();
        let open = self
            .data
            .positions(&request)
            .await
            .context("data-api positions request failed")?;
        for p in open {
            records.push(self.open_position_to_record(&p));
        }

        debug!(
            wallet = %wallet,
            records = records.len(),
            "Fetched wallet history from Data API"
        );
        Ok(records)
    }

    /// Fetches `/closed-positions` pages with bounded parallelism (4 concurrent
    /// requests per batch, 429-aware retries), stopping as soon as a full batch
    /// comes back empty.
    async fn fetch_closed_positions_paged(&self, wallet: &str) -> Result<Vec<RawClosedPosition>> {
        const PAGE_SIZE: i32 = 50;
        const CONCURRENCY: i32 = 2;
        const SAFETY_CAP: i32 = 5000;

        let mut out = Vec::new();
        let mut offset = 0i32;
        while offset < SAFETY_CAP {
            let mut handles = Vec::new();
            for page_offset in
                closed_position_page_offsets(offset, PAGE_SIZE, CONCURRENCY, SAFETY_CAP)
            {
                let url = format!(
                    "{}/closed-positions?user={}&limit={}&offset={}",
                    DATA_API_URL, wallet, PAGE_SIZE, page_offset
                );
                let client = self.http.clone();
                handles.push(tokio::spawn(async move {
                    // 429-aware retry (data-api rate limits): back off up to ~6s.
                    let policy = ReadRetryPolicy {
                        max_attempts: 5,
                        initial_delay: std::time::Duration::from_secs(1),
                        max_delay: std::time::Duration::from_secs(10),
                    };
                    retry_read(policy, || {
                        let client = client.clone();
                        let url = url.clone();
                        async move {
                            let resp =
                                client.get(&url).send().await.map_err(anyhow::Error::from)?;
                            resp.error_for_status()
                                .map_err(anyhow::Error::from)?
                                .json::<Vec<RawClosedPosition>>()
                                .await
                                .map_err(anyhow::Error::from)
                        }
                    })
                    .await
                }));
            }

            let results = futures::future::join_all(handles).await;
            let mut got_any = false;
            let mut reached_end = false;
            let mut page_errors = Vec::new();
            for (i, r) in results.into_iter().enumerate() {
                match r {
                    Ok(Ok(page)) => {
                        if !page.is_empty() {
                            got_any = true;
                            reached_end |= page.len() < PAGE_SIZE as usize;
                            out.extend(page);
                        } else {
                            reached_end = true;
                        }
                    }
                    Ok(Err(e)) => {
                        let failed_offset = offset + i as i32 * PAGE_SIZE;
                        warn!(wallet = %wallet, offset = failed_offset, error = %e, "closed-positions page failed after retries");
                        page_errors.push(format!("offset {failed_offset}: {e}"));
                    }
                    Err(e) => {
                        warn!(wallet = %wallet, "closed-positions task panicked: {e}");
                        page_errors.push(format!("task failure: {e}"));
                    }
                }
            }
            if !page_errors.is_empty() {
                anyhow::bail!(
                    "incomplete closed-position history for {wallet}: {}",
                    page_errors.join("; ")
                );
            }
            if !got_any || reached_end {
                break;
            }
            offset += CONCURRENCY * PAGE_SIZE;
        }
        // Be defensive against an upstream page changing while we paginate.
        // Duplicate history rows would otherwise inflate every whitelist metric.
        let mut seen = HashSet::new();
        out.retain(|cp| seen.insert(format!("{}:{}:{}", cp.condition_id, cp.asset, cp.timestamp)));
        Ok(out)
    }

    /// Discovers candidate wallets from the official weekly-volume leaderboard.
    /// These are candidates only: the normal settled-history funnel still has
    /// to approve every wallet before it can produce a signal.
    pub async fn discover_candidates(&self, limit: usize) -> Result<Vec<String>> {
        let url = format!(
            "{}/v1/leaderboard?category=OVERALL&timePeriod=WEEK&orderBy=VOL&limit={}",
            DATA_API_URL, limit
        );
        let resp = reqwest::Client::new()
            .get(&url)
            .send()
            .await
            .context("leaderboard request failed")?
            .error_for_status()
            .context("leaderboard non-2xx response")?
            .json::<serde_json::Value>()
            .await
            .context("leaderboard parse failed")?;

        let mut candidates = Vec::new();
        if let Some(arr) = resp.as_array() {
            for item in arr {
                if let Some(addr) = item.get("proxyWallet").and_then(|v| v.as_str()) {
                    let normalized = addr.trim().to_ascii_lowercase();
                    if normalized.len() == 42
                        && normalized.starts_with("0x")
                        && normalized[2..].chars().all(|c| c.is_ascii_hexdigit())
                        && !candidates.contains(&normalized)
                    {
                        candidates.push(normalized);
                    }
                }
            }
        }
        Ok(candidates)
    }

    fn raw_closed_position_to_record(&self, cp: &RawClosedPosition) -> WalletTradeRecord {
        let outcome = outcome_from_label(&cp.outcome);
        let avg_price = cp.avg_price.0;
        let total_bought = cp.total_bought.0;
        let realized_pnl = cp.realized_pnl.0;
        let cur_price = cp.cur_price.0;
        let notional = (total_bought * avg_price).round_dp(2);
        // A closed position is not necessarily a resolved prediction: the wallet
        // may simply have sold early. Only terminal prices are valid labels for
        // Brier/win-rate evaluation; P&L sign is never an outcome oracle.
        let resolved_outcome = if cur_price >= dec!(0.99) {
            Some(outcome.clone())
        } else if cur_price <= dec!(0.01) {
            Some(outcome_from_label(&cp.opposite_outcome))
        } else {
            None
        };
        let is_resolved = resolved_outcome.is_some();
        let settled_at = DateTime::from_timestamp(cp.timestamp, 0).unwrap_or_else(Utc::now);

        WalletTradeRecord {
            trade_id: format!("cp-{}-{}-{}", cp.condition_id, cp.asset, cp.timestamp),
            market_id: cp.condition_id.clone(),
            domain: DomainClassifier::classify(&format!("{} {}", cp.title, cp.slug)),
            outcome_bought: outcome,
            entry_price: avg_price,
            exit_price: Some(cur_price),
            notional_usdc: notional,
            pnl_usdc: realized_pnl,
            return_pct: if notional > Decimal::ZERO {
                realized_pnl / notional
            } else {
                Decimal::ZERO
            },
            is_maker: false,
            is_arbitrage_leg: false,
            behavior_data_known: false,
            resolved_outcome,
            // The closed-position endpoint does not expose an actual open time.
            // Keep the observable activity timestamp instead of fabricating one.
            opened_at: settled_at,
            settled_at: is_resolved.then_some(settled_at),
        }
    }

    fn open_position_to_record(&self, p: &Position) -> WalletTradeRecord {
        let notional = (p.total_bought * p.avg_price).round_dp(2);
        WalletTradeRecord {
            trade_id: format!("op-{}-{}", condition_id_str(&p.condition_id), p.asset),
            market_id: condition_id_str(&p.condition_id),
            domain: DomainClassifier::classify(&format!("{} {}", p.title, p.slug)),
            outcome_bought: outcome_from_label(&p.outcome),
            entry_price: p.avg_price,
            exit_price: None,
            notional_usdc: notional,
            pnl_usdc: p.cash_pnl,
            return_pct: p.percent_pnl,
            is_maker: false,
            is_arbitrage_leg: false,
            behavior_data_known: false,
            resolved_outcome: None,
            opened_at: Utc::now() - Duration::days(30),
            settled_at: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifier_detects_major_domains() {
        assert_eq!(
            DomainClassifier::classify("Will the Fed cut rates in September?"),
            DomainTag::MacroInterestRate
        );
        assert_eq!(
            DomainClassifier::classify("Will Russia declare a ceasefire?"),
            DomainTag::Geopolitics
        );
        assert_eq!(
            DomainClassifier::classify("Will Bitcoin reach $100k by June?"),
            DomainTag::Crypto
        );
        assert_eq!(
            DomainClassifier::classify("Who will win the 2026 midterm election?"),
            DomainTag::Elections
        );
        assert_eq!(
            DomainClassifier::classify("Will the Lakers win the NBA championship?"),
            DomainTag::PopCultureSports
        );
    }

    #[test]
    fn outcome_labels_map_to_yes_no() {
        assert_eq!(outcome_from_label("Yes"), OutcomeSide::Yes);
        assert_eq!(outcome_from_label("NO"), OutcomeSide::No);
        // Directional binary markets use Up/Down pairs.
        assert_eq!(outcome_from_label("Up"), OutcomeSide::Yes);
        assert_eq!(outcome_from_label("down"), OutcomeSide::No);
        // Categorical outcomes stay Other (rejected by the binary-only filter).
        assert_eq!(
            outcome_from_label("Over"),
            OutcomeSide::Other("over".to_string())
        );
    }

    #[test]
    fn closed_position_maps_to_trade_record() {
        let cp: RawClosedPosition = serde_json::from_str(
            r#"{
                "proxyWallet":"0x0000000000000000000000000000000000000001",
                "asset":"1234",
                "conditionId":"0x00000000000000000000000000000000000000000000000000000000000000aa",
                "avgPrice":"0.60","totalBought":"100","realizedPnl":"35.00",
                "curPrice":"1.0","timestamp":1710000000,
                "title":"Will the Fed cut rates?","slug":"fed-cut","icon":"","eventSlug":"fed",
                "outcome":"Yes","outcomeIndex":0,"oppositeOutcome":"No","oppositeAsset":"1235",
                "endDate":"2024-03-01T00:00:00Z"
            }"#,
        )
        .unwrap();
        let f = WalletHistoryFetcher::new().unwrap();
        let rec = f.raw_closed_position_to_record(&cp);
        assert_eq!(rec.entry_price, dec!(0.60));
        assert_eq!(rec.pnl_usdc, dec!(35.0));
        assert_eq!(rec.resolved_outcome, Some(OutcomeSide::Yes));
        assert_eq!(rec.domain, DomainTag::MacroInterestRate);
        assert!(rec.settled_at.is_some());
    }

    #[test]
    fn losing_closed_position_maps_opposite_outcome() {
        let cp: RawClosedPosition = serde_json::from_str(
            r#"{
                "proxyWallet":"0x0000000000000000000000000000000000000001",
                "asset":"1234",
                "conditionId":"0x00000000000000000000000000000000000000000000000000000000000000aa",
                "avgPrice":"0.60","totalBought":"100","realizedPnl":"-40.00",
                "curPrice":"0.0","timestamp":1710000000,
                "title":"Will the Fed cut rates?","slug":"fed-cut","icon":"","eventSlug":"fed",
                "outcome":"Yes","outcomeIndex":0,"oppositeOutcome":"No","oppositeAsset":"1235",
                "endDate":"2024-03-01T00:00:00Z"
            }"#,
        )
        .unwrap();
        let f = WalletHistoryFetcher::new().unwrap();
        let rec = f.raw_closed_position_to_record(&cp);
        assert_eq!(rec.resolved_outcome, Some(OutcomeSide::No));
    }

    #[test]
    fn condition_and_token_formatting() {
        let cond: B256 = "0x00000000000000000000000000000000000000000000000000000000000000aa"
            .parse()
            .unwrap();
        let s = condition_id_str(&cond);
        assert!(s.starts_with("0x") && s.len() == 66, "got {}", s);
    }

    #[test]
    fn history_pagination_uses_non_overlapping_offsets() {
        assert_eq!(
            closed_position_page_offsets(0, 50, 4, 5_000),
            vec![0, 50, 100, 150]
        );
        assert_eq!(
            closed_position_page_offsets(4_900, 50, 4, 5_000),
            vec![4_900, 4_950]
        );
    }

    #[test]
    fn positions_are_needed_only_for_fresh_unseen_sell_candidates() {
        let now = Utc::now();
        assert!(is_actionable_sell(
            &Side::Sell,
            (now - Duration::seconds(299)).timestamp(),
            now,
            300
        ));
        assert!(!is_actionable_sell(
            &Side::Sell,
            (now - Duration::seconds(301)).timestamp(),
            now,
            300
        ));
        assert!(!is_actionable_sell(&Side::Buy, now.timestamp(), now, 300));
    }
}
