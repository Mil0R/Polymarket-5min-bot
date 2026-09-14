use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use dashmap::{DashMap, DashSet};
use futures::{stream, StreamExt};
use polymarket_client_sdk::clob::ws::{types::response::BookUpdate, Client as WsClient};
use polymarket_client_sdk::types::U256;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::config::CrossMarketProbabilityArbitrageConfig;
#[derive(Debug, Deserialize)]
struct GammaEventPage {
    events: Vec<GammaEvent>,
    #[serde(default)]
    next_cursor: Option<String>,
}

use super::model::{
    BookLevel, BookSnapshot, BookSource, FeePolicy, MarketRelationship, Outcome, RelationLeg,
    RelationshipKind,
};

#[derive(Debug, Clone)]
pub struct CrossMarketProbabilityResearchClient {
    http: reqwest::Client,
    config: CrossMarketProbabilityArbitrageConfig,
    book_cache: Arc<DashMap<String, BookSnapshot>>,
    websocket_subscribed_tokens: Arc<DashSet<String>>,
    conversion_audit: Arc<DashMap<String, Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiscoveryDiagnostics {
    pub events_scanned: usize,
    pub deadline_cue_events: usize,
    pub deadline_relationships_generated: usize,
    pub neg_risk_events_seen: usize,
    pub neg_risk_events_emitted: usize,
    pub neg_risk_relationships_generated: usize,
    pub neg_risk_rejections: BTreeMap<String, u64>,
    #[serde(default)]
    pub neg_risk_market_rejections: BTreeMap<String, u64>,
    #[serde(default)]
    pub neg_risk_constraint_failures: BTreeMap<String, u64>,
    #[serde(default)]
    pub neg_risk_rejection_examples: BTreeMap<String, Vec<NegRiskRejectionExample>>,
    pub relationships_before_limit: usize,
    pub relationships_emitted: usize,
    pub relationships_truncated: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NegRiskRejectionExample {
    pub event_id: String,
    pub event_title: String,
    pub deployed_markets: usize,
    pub tradable_markets: usize,
    pub tradable_other_markets: usize,
    pub market_rejections: BTreeMap<String, usize>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct BookFetchDiagnostics {
    pub requested_tokens: usize,
    pub returned_books: usize,
    pub websocket_hits: usize,
    pub rest_successes: usize,
    pub rest_failures: usize,
    pub rest_latency_total_ms: u64,
    pub rest_latency_max_ms: u64,
}

impl BookFetchDiagnostics {
    pub fn merge(&mut self, other: &Self) {
        self.requested_tokens += other.requested_tokens;
        self.returned_books += other.returned_books;
        self.websocket_hits += other.websocket_hits;
        self.rest_successes += other.rest_successes;
        self.rest_failures += other.rest_failures;
        self.rest_latency_total_ms += other.rest_latency_total_ms;
        self.rest_latency_max_ms = self.rest_latency_max_ms.max(other.rest_latency_max_ms);
    }

    pub fn rest_latency_average_ms(&self) -> Option<u64> {
        (self.rest_successes > 0).then(|| self.rest_latency_total_ms / self.rest_successes as u64)
    }
}

#[derive(Debug, Clone)]
pub struct BookFetchBatch {
    pub books: HashMap<String, BookSnapshot>,
    pub diagnostics: BookFetchDiagnostics,
}

#[derive(Debug, Clone)]
pub struct DiscoverySnapshot {
    pub relationships: Vec<MarketRelationship>,
    pub diagnostics: DiscoveryDiagnostics,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GammaEvent {
    id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    slug: String,
    #[serde(default)]
    neg_risk: bool,
    #[serde(default)]
    enable_neg_risk: bool,
    #[serde(default)]
    markets: Vec<GammaMarket>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GammaMarket {
    #[serde(default)]
    condition_id: String,
    #[serde(default)]
    question: String,
    #[serde(default)]
    active: bool,
    #[serde(default)]
    closed: bool,
    #[serde(default)]
    accepting_orders: bool,
    #[serde(default)]
    enable_order_book: bool,
    #[serde(default)]
    end_date: Option<DateTime<Utc>>,
    #[serde(default)]
    clob_token_ids: Value,
    #[serde(default)]
    outcomes: Value,
    #[serde(default)]
    group_item_threshold: Option<String>,
    #[serde(default)]
    group_item_title: Option<String>,
    #[serde(default)]
    resolution_source: String,
    #[serde(default)]
    neg_risk_other: bool,
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
}

#[derive(Debug, Deserialize)]
struct ClobBookResponse {
    #[serde(default)]
    hash: Option<String>,
    #[serde(default)]
    asset_id: Option<String>,
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

#[derive(Debug, Clone)]
struct TradableMarket {
    condition_id: String,
    question: String,
    yes_token_id: String,
    no_token_id: String,
    deadline_at: Option<DateTime<Utc>>,
    deadline_rank: Option<i64>,
    deadline_label: Option<String>,
    resolution_source: String,
    neg_risk_other: bool,
    fee: FeePolicy,
}

fn rotate_and_limit<T>(mut values: Vec<T>, offset: usize, limit: usize) -> (Vec<T>, usize, usize) {
    let before = values.len();
    let selection_offset = if before == 0 { 0 } else { offset % before };
    values.rotate_left(selection_offset);
    values.truncate(limit);
    (values, selection_offset, before.saturating_sub(limit))
}

#[derive(Debug, Clone, Deserialize)]
struct SemanticReview {
    status: String,
    #[serde(default)]
    notes: Vec<String>,
    #[serde(default)]
    reviewed_at: Option<String>,
}

impl CrossMarketProbabilityResearchClient {
    pub fn new(config: CrossMarketProbabilityArbitrageConfig) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent("polypulse-bayesian-coherence-shadow")
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(20))
            .build()
            .context("failed to build cross-market probability research HTTP client")?;
        Ok(Self {
            http,
            config,
            book_cache: Arc::new(DashMap::new()),
            websocket_subscribed_tokens: Arc::new(DashSet::new()),
            conversion_audit: Arc::new(DashMap::new()),
        })
    }

    pub fn config(&self) -> &CrossMarketProbabilityArbitrageConfig {
        &self.config
    }

    /// Separate discovery policy for conversion research. Gamma membership is
    /// only a hypothesis: this deliberately grants no on-chain execution authority.
    pub async fn discover_conversion_routes(
        &self,
    ) -> Result<(Vec<super::conversion::Route>, BTreeMap<String, usize>)> {
        self.discover_conversion_routes_at(0).await
    }

    pub async fn discover_conversion_routes_at(
        &self,
        page_offset: usize,
    ) -> Result<(Vec<super::conversion::Route>, BTreeMap<String, usize>)> {
        self.discover_conversion_routes_at_with_route_offset(page_offset, 0)
            .await
    }

    pub async fn discover_conversion_routes_at_with_route_offset(
        &self,
        page_offset: usize,
        route_offset: usize,
    ) -> Result<(Vec<super::conversion::Route>, BTreeMap<String, usize>)> {
        let events = self.fetch_events_at(page_offset).await?;
        let mut routes = Vec::new();
        let mut diagnostics = BTreeMap::new();
        diagnostics.insert("events_scanned".into(), events.len());
        diagnostics.insert("page_offset".into(), page_offset);
        for event in events {
            if !(event.neg_risk || event.enable_neg_risk) {
                continue;
            }
            *diagnostics.entry("neg_risk_events".into()).or_default() += 1;
            let markets = event
                .markets
                .iter()
                .filter_map(tradable_market)
                .collect::<Vec<_>>();
            let complete = event.markets.iter().all(|m| {
                let outcomes = string_array(&m.outcomes).unwrap_or_default();
                outcomes.len() == 2
                    && outcomes.iter().any(|o| o.eq_ignore_ascii_case("yes"))
                    && outcomes.iter().any(|o| o.eq_ignore_ascii_case("no"))
                    && (!m.fees_enabled || m.fee_schedule.is_some())
            });
            if markets.len() != event.markets.len() {
                *diagnostics
                    .entry("untradable_or_incomplete_tokens".into())
                    .or_default() += 1;
            }
            if event.markets.len() < 2 {
                *diagnostics.entry("too_few_outcomes".into()).or_default() += 1;
            }
            if event.markets.len() > self.config.max_partition_legs {
                *diagnostics.entry("too_many_outcomes".into()).or_default() += 1;
            }
            if event
                .markets
                .iter()
                .any(|m| m.fees_enabled && m.fee_schedule.is_none())
            {
                *diagnostics
                    .entry("missing_enabled_fee_schedule".into())
                    .or_default() += 1;
            }
            if event.markets.iter().any(|m| {
                let o = string_array(&m.outcomes).unwrap_or_default();
                o.len() != 2
                    || !o.iter().any(|v| v.eq_ignore_ascii_case("yes"))
                    || !o.iter().any(|v| v.eq_ignore_ascii_case("no"))
            }) {
                *diagnostics
                    .entry("invalid_outcome_labels".into())
                    .or_default() += 1;
            }
            if !complete
                || markets.len() != event.markets.len()
                || !(2..=self.config.max_partition_legs).contains(&markets.len())
            {
                *diagnostics
                    .entry("incomplete_untradable_unknown_fee_or_leg_limit".into())
                    .or_default() += 1;
                continue;
            }
            let mut tokens = HashSet::new();
            if !markets.iter().all(|m| {
                tokens.insert(m.yes_token_id.clone()) && tokens.insert(m.no_token_id.clone())
            }) {
                *diagnostics.entry("duplicate_tokens".into()).or_default() += 1;
                continue;
            }
            for input in &markets {
                let mut deadlines = markets.iter().filter_map(|market| market.deadline_at);
                let earliest_deadline_at = deadlines.next().map(|first| {
                    markets
                        .iter()
                        .filter_map(|market| market.deadline_at)
                        .fold(first, |current, value| current.min(value))
                });
                let latest_deadline_at =
                    markets.iter().filter_map(|market| market.deadline_at).max();
                routes.push(super::conversion::Route {
                    id: format!("conversion:{}:{}", event.id, input.condition_id),
                    event_id: event.id.clone(),
                    slug: event.slug.clone(),
                    input: relation_leg(input, Outcome::No),
                    outputs: markets
                        .iter()
                        .filter(|m| m.condition_id != input.condition_id)
                        .map(|m| relation_leg(m, Outcome::Yes))
                        .collect(),
                    protocol_verified: false,
                    event_title: event.title.clone(),
                    market_count: markets.len(),
                    fee_enabled_legs: markets.iter().filter(|market| market.fee.enabled).count(),
                    neg_risk_other_markets: markets
                        .iter()
                        .filter(|market| market.neg_risk_other)
                        .count(),
                    earliest_deadline_at,
                    latest_deadline_at,
                });
            }
        }
        routes.sort_by(|a, b| {
            a.fee_enabled_legs
                .cmp(&b.fee_enabled_legs)
                .then_with(|| a.market_count.cmp(&b.market_count))
                .then_with(
                    || match (&a.earliest_deadline_at, &b.earliest_deadline_at) {
                        (Some(a), Some(b)) => a.cmp(b),
                        (Some(_), None) => std::cmp::Ordering::Less,
                        (None, Some(_)) => std::cmp::Ordering::Greater,
                        (None, None) => std::cmp::Ordering::Equal,
                    },
                )
                .then_with(|| a.id.cmp(&b.id))
        });
        let routes_before_limit = routes.len();
        let (routes, selection_offset, routes_truncated) =
            rotate_and_limit(routes, route_offset, self.config.max_relationships);
        diagnostics.insert("routes_before_limit".into(), routes_before_limit);
        diagnostics.insert("route_selection_offset".into(), selection_offset);
        diagnostics.insert("routes_truncated".into(), routes_truncated);
        diagnostics.insert("routes_emitted".into(), routes.len());
        Ok((routes, diagnostics))
    }

    pub async fn fetch_conversion_books(
        &self,
        routes: &[super::conversion::Route],
    ) -> HashMap<String, BookSnapshot> {
        self.conversion_audit.clear();
        let tokens = routes
            .iter()
            .flat_map(|r| std::iter::once(&r.input).chain(r.outputs.iter()))
            .map(|l| l.token_id.clone())
            .collect::<HashSet<_>>();
        stream::iter(tokens.into_iter().map(|token| async move {
            match self.fetch_book_checked(&token, true).await {
                Ok(book) => Some((token, book)),
                Err(error) => {
                    self.conversion_audit.insert(
                        token,
                        serde_json::json!({"error":format!("{error:#}"),"received_at":Utc::now()}),
                    );
                    None
                }
            }
        }))
        .buffer_unordered(self.config.request_concurrency)
        .filter_map(|v| async move { v })
        .collect()
        .await
    }

    pub fn conversion_book_audit(&self) -> BTreeMap<String, Value> {
        self.conversion_audit
            .iter()
            .map(|e| (e.key().clone(), e.value().clone()))
            .collect()
    }

    pub async fn discover_relationships(&self) -> Result<Vec<MarketRelationship>> {
        Ok(self.discover_snapshot().await?.relationships)
    }

    pub async fn discover_snapshot(&self) -> Result<DiscoverySnapshot> {
        let events = self.fetch_events().await?;
        let mut diagnostics = DiscoveryDiagnostics {
            events_scanned: events.len(),
            deadline_cue_events: 0,
            deadline_relationships_generated: 0,
            neg_risk_events_seen: 0,
            neg_risk_events_emitted: 0,
            neg_risk_relationships_generated: 0,
            neg_risk_rejections: BTreeMap::new(),
            neg_risk_market_rejections: BTreeMap::new(),
            neg_risk_constraint_failures: BTreeMap::new(),
            neg_risk_rejection_examples: BTreeMap::new(),
            relationships_before_limit: 0,
            relationships_emitted: 0,
            relationships_truncated: 0,
        };
        let mut relationships = Vec::new();
        for event in events {
            if event_title_has_deadline_cue(&event.title) {
                diagnostics.deadline_cue_events += 1;
            }
            let deadline = discover_deadline_relationships(&event);
            diagnostics.deadline_relationships_generated += deadline.len();
            relationships.extend(deadline);

            if event.neg_risk || event.enable_neg_risk {
                diagnostics.neg_risk_events_seen += 1;
                let constraint_failures =
                    neg_risk_constraint_failures(&event, self.config.max_partition_legs);
                for reason in &constraint_failures {
                    *diagnostics
                        .neg_risk_constraint_failures
                        .entry((*reason).to_string())
                        .or_default() += 1;
                    let examples = diagnostics
                        .neg_risk_rejection_examples
                        .entry((*reason).to_string())
                        .or_default();
                    if examples.len() < 3 {
                        examples.push(neg_risk_rejection_example(&event));
                    }
                }
                for market in event
                    .markets
                    .iter()
                    .filter(|market| !market.condition_id.is_empty())
                {
                    if let Some(reason) = market_tradability_rejection(market) {
                        *diagnostics
                            .neg_risk_market_rejections
                            .entry(reason.to_string())
                            .or_default() += 1;
                    }
                }
                match neg_risk_eligibility(&event, self.config.max_partition_legs) {
                    Ok(()) => {
                        let generated =
                            discover_neg_risk_relationships(&event, self.config.max_partition_legs);
                        diagnostics.neg_risk_events_emitted += 1;
                        diagnostics.neg_risk_relationships_generated += generated.len();
                        relationships.extend(generated);
                    }
                    Err(reason) => {
                        *diagnostics
                            .neg_risk_rejections
                            .entry(reason.to_string())
                            .or_default() += 1;
                    }
                }
            }
        }
        // Protocol-encoded complete sets are more authoritative than inferred
        // text templates, so preserve them first when the configured cap binds.
        relationships.sort_by_key(|relationship| match relationship.kind {
            RelationshipKind::NegRiskCompleteSetYes | RelationshipKind::NegRiskCompleteSetNo => 0,
            RelationshipKind::DeadlineImplication => 1,
        });
        let semantic_reviews = load_semantic_reviews(&self.config.semantic_review_file)?;
        for relationship in &mut relationships {
            if let Some(review) = semantic_reviews.get(&relationship.relationship_id) {
                relationship.semantic_status = normalize_review_status(&review.status).to_string();
                relationship.evidence.extend(review.notes.iter().cloned());
                if let Some(reviewed_at) = &review.reviewed_at {
                    relationship
                        .evidence
                        .push(format!("Semantic review recorded at {reviewed_at}."));
                }
                relationship.live_eligible = false;
            } else if self
                .config
                .semantic_allowlist
                .contains(&relationship.relationship_id)
            {
                relationship.semantic_status = "manual_allowlist_reviewed_shadow".into();
                // Approval only changes research labeling. This binary remains
                // incapable of live execution and the relation stays fail-closed.
                relationship.live_eligible = false;
            }
        }
        diagnostics.relationships_before_limit = relationships.len();
        relationships.truncate(self.config.max_relationships);
        diagnostics.relationships_emitted = relationships.len();
        diagnostics.relationships_truncated = diagnostics
            .relationships_before_limit
            .saturating_sub(diagnostics.relationships_emitted);
        Ok(DiscoverySnapshot {
            relationships,
            diagnostics,
        })
    }

    pub async fn fetch_books(
        &self,
        relationships: &[MarketRelationship],
    ) -> HashMap<String, BookSnapshot> {
        self.fetch_books_with_diagnostics(relationships).await.books
    }

    pub async fn fetch_books_with_diagnostics(
        &self,
        relationships: &[MarketRelationship],
    ) -> BookFetchBatch {
        self.start_book_stream(relationships);
        let token_ids = relationships
            .iter()
            .flat_map(|relationship| relationship.legs.iter().map(|leg| leg.token_id.clone()))
            .collect::<HashSet<_>>();
        let requests = token_ids.into_iter().map(|token_id| {
            let client = self.clone();
            async move {
                let result = client.fetch_book(&token_id).await;
                (token_id, result)
            }
        });
        let requested_tokens = requests.len();
        let results = stream::iter(requests)
            .buffer_unordered(self.config.request_concurrency)
            .collect::<Vec<_>>()
            .await;
        let mut books = HashMap::new();
        let mut diagnostics = BookFetchDiagnostics {
            requested_tokens,
            ..BookFetchDiagnostics::default()
        };
        for (token_id, result) in results {
            match result {
                Ok(book) => {
                    diagnostics.returned_books += 1;
                    match book.source {
                        BookSource::Websocket => diagnostics.websocket_hits += 1,
                        BookSource::Rest => {
                            diagnostics.rest_successes += 1;
                            if let Some(latency) = book.request_latency_ms {
                                diagnostics.rest_latency_total_ms += latency;
                                diagnostics.rest_latency_max_ms =
                                    diagnostics.rest_latency_max_ms.max(latency);
                            }
                        }
                        BookSource::Unknown => {}
                    }
                    books.insert(token_id, book);
                }
                Err(error) => {
                    diagnostics.rest_failures += 1;
                    tracing::debug!(%token_id, %error, "cross-market probability book request failed");
                }
            }
        }
        BookFetchBatch { books, diagnostics }
    }

    async fn fetch_events(&self) -> Result<Vec<GammaEvent>> {
        self.fetch_events_at(0).await
    }

    async fn fetch_events_at(&self, page_offset: usize) -> Result<Vec<GammaEvent>> {
        let mut all = Vec::new();
        let mut seen = HashSet::new();
        let mut cursor: Option<String> = None;
        let mut cursors = HashSet::new();
        // Walk cursors to the requested disjoint window; never synthesize an offset.
        for page in 0..page_offset.saturating_add(self.config.event_pages) {
            let mut request = self
                .http
                .get(format!("{}/events/keyset", self.config.gamma_url))
                .query(&[
                    ("active", "true"),
                    ("closed", "false"),
                    ("limit", &self.config.events_per_page.to_string()),
                ]);
            if let Some(cursor) = &cursor {
                request = request.query(&[("after_cursor", cursor)]);
            }
            let response = request
                .send()
                .await
                .with_context(|| format!("Gamma events/keyset request failed at page {page}"))?;
            let status = response.status();
            if !status.is_success() {
                let retry_after = response
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("unspecified")
                    .to_string();
                let body = response.text().await.unwrap_or_default();
                anyhow::bail!("Gamma events/keyset HTTP {status} at page {page}, window_page={page_offset}, retry-after={retry_after}: {}", body.chars().take(512).collect::<String>());
            }
            let batch: GammaEventPage = response
                .json()
                .await
                .with_context(|| format!("Gamma events/keyset invalid response at page {page}"))?;
            if page >= page_offset {
                for event in batch.events {
                    if seen.insert(event.id.clone()) {
                        all.push(event);
                    }
                }
            }
            cursor = batch.next_cursor.filter(|s| !s.is_empty());
            let Some(next) = &cursor else {
                break;
            };
            anyhow::ensure!(
                cursors.insert(next.clone()),
                "Gamma repeated pagination cursor at page {page}"
            );
        }
        Ok(all)
    }

    async fn fetch_book(&self, token_id: &str) -> Result<BookSnapshot> {
        self.fetch_book_checked(token_id, false).await
    }

    async fn fetch_book_checked(&self, token_id: &str, strict_token: bool) -> Result<BookSnapshot> {
        if let Some(book) = self.book_cache.get(token_id).filter(|_| !strict_token) {
            let age_ms = Utc::now()
                .signed_duration_since(book.observed_at)
                .num_milliseconds();
            if age_ms <= self.config.websocket_book_max_age_ms {
                return Ok(book.clone());
            }
        }
        let request_started = Instant::now();
        let request_started_at = Utc::now();
        let response = self
            .http
            .get(format!("{}/book", self.config.clob_url))
            .query(&[("token_id", token_id)])
            .send()
            .await
            .context("Cross-market probability CLOB book request failed")?
            .error_for_status()
            .context("Cross-market probability CLOB book returned non-success")?;
        let headers = ["date", "age", "cache-control", "cf-cache-status", "x-cache"]
            .into_iter()
            .filter_map(|k| {
                response
                    .headers()
                    .get(k)
                    .and_then(|v| v.to_str().ok())
                    .map(|v| (k.to_string(), v.to_string()))
            })
            .collect::<BTreeMap<_, _>>();
        let response: ClobBookResponse = response
            .json()
            .await
            .context("Cross-market probability CLOB book response is invalid")?;
        let request_latency_ms = request_started.elapsed().as_millis() as u64;
        if strict_token && response.asset_id.as_deref() != Some(token_id) {
            anyhow::bail!("missing or mismatched CLOB asset_id");
        }
        if strict_token {
            self.conversion_audit.insert(token_id.to_string(), serde_json::json!({"request_started_at":request_started_at,
                "received_at":Utc::now(),"request_latency_ms":request_latency_ms,"headers":headers,
                "exchange_timestamp_ms":response.timestamp.as_ref().and_then(value_i64),"hash":response.hash}));
        }
        let mut bids = response
            .bids
            .into_iter()
            .filter_map(|level| {
                let price = Decimal::from_str(&level.price).ok()?;
                let size = Decimal::from_str(&level.size).ok()?;
                (price > Decimal::ZERO && size > Decimal::ZERO).then_some(BookLevel { price, size })
            })
            .collect::<Vec<_>>();
        bids.sort_by_key(|level| std::cmp::Reverse(level.price));
        let mut asks = response
            .asks
            .into_iter()
            .filter_map(|level| {
                let price = Decimal::from_str(&level.price).ok()?;
                let size = Decimal::from_str(&level.size).ok()?;
                (price > Decimal::ZERO && size > Decimal::ZERO).then_some(BookLevel { price, size })
            })
            .collect::<Vec<_>>();
        asks.sort_by_key(|level| level.price);
        Ok(BookSnapshot {
            token_id: token_id.to_string(),
            observed_at: Utc::now(),
            exchange_timestamp_ms: response.timestamp.as_ref().and_then(value_i64),
            source: BookSource::Rest,
            request_latency_ms: Some(request_latency_ms),
            bids,
            asks,
        })
    }

    fn start_book_stream(&self, relationships: &[MarketRelationship]) {
        if !self.config.websocket_books {
            return;
        }
        let mut token_ids = Vec::new();
        for token in relationships
            .iter()
            .flat_map(|relationship| relationship.legs.iter().map(|leg| leg.token_id.as_str()))
        {
            if self.websocket_subscribed_tokens.insert(token.to_string()) {
                match U256::from_str(token) {
                    Ok(token_id) => token_ids.push(token_id),
                    Err(error) => {
                        tracing::warn!(%error, token, "Cross-market probability WebSocket token id is invalid")
                    }
                }
            }
        }
        if token_ids.is_empty() {
            return;
        }
        let cache = Arc::clone(&self.book_cache);
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
                                    tracing::warn!(%error, "Cross-market probability WebSocket orderbook stream error");
                                    break;
                                }
                            }
                        }
                    }
                    Err(error) => {
                        tracing::warn!(%error, "Cross-market probability WebSocket subscription failed")
                    }
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
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
    bids.sort_by_key(|level| std::cmp::Reverse(level.price));
    let mut asks = book
        .asks
        .into_iter()
        .filter(|level| level.price > Decimal::ZERO && level.size > Decimal::ZERO)
        .map(|level| BookLevel {
            price: level.price,
            size: level.size,
        })
        .collect::<Vec<_>>();
    asks.sort_by_key(|level| level.price);
    BookSnapshot {
        token_id: book.asset_id.to_string(),
        observed_at: Utc::now(),
        exchange_timestamp_ms: Some(book.timestamp),
        source: BookSource::Websocket,
        request_latency_ms: None,
        bids,
        asks,
    }
}

fn load_semantic_reviews(path: &std::path::Path) -> Result<HashMap<String, SemanticReview>> {
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let bytes = std::fs::read(path)
        .with_context(|| format!("failed to read semantic review file {}", path.display()))?;
    serde_json::from_slice(&bytes)
        .with_context(|| format!("invalid semantic review file {}", path.display()))
}

fn normalize_review_status(status: &str) -> &'static str {
    match status.trim().to_ascii_lowercase().as_str() {
        "rules_equivalent" => "rules_equivalent",
        "manually_approved" => "manually_approved",
        "rejected_semantic_mismatch" => "rejected_semantic_mismatch",
        _ => "review_status_invalid_fail_closed",
    }
}

fn discover_deadline_relationships(event: &GammaEvent) -> Vec<MarketRelationship> {
    if !event_title_has_deadline_cue(&event.title) {
        return Vec::new();
    }
    let mut markets = event
        .markets
        .iter()
        .filter_map(tradable_market)
        .filter(|market| {
            market.deadline_at.is_some()
                && market.deadline_rank.is_some()
                && market
                    .deadline_label
                    .as_deref()
                    .is_some_and(looks_like_deadline_label)
        })
        .collect::<Vec<_>>();
    markets.sort_by_key(|market| market.deadline_at);
    let mut relationships = Vec::new();
    for narrow_index in 0..markets.len() {
        for broad_index in (narrow_index + 1)..markets.len() {
            let narrow = &markets[narrow_index];
            let broad = &markets[broad_index];
            if !same_resolution_source(narrow, broad) {
                continue;
            }
            let narrow_rank = narrow.deadline_rank.unwrap_or_default();
            let broad_rank = broad.deadline_rank.unwrap_or_default();
            let narrow_at = narrow.deadline_at.expect("deadline market was filtered");
            let broad_at = broad.deadline_at.expect("deadline market was filtered");
            if narrow_at >= broad_at {
                continue;
            }
            relationships.push(MarketRelationship {
                relationship_id: format!(
                    "deadline:{}:{}:{}",
                    event.id, narrow.condition_id, broad.condition_id
                ),
                event_id: event.id.clone(),
                event_slug: event.slug.clone(),
                event_title: event.title.clone(),
                kind: RelationshipKind::DeadlineImplication,
                semantic_status: "template_inferred_requires_rule_review".into(),
                evidence: vec![
                    "Both markets are active binary markets grouped in one Gamma event whose title contains a deadline cue.".into(),
                    format!(
                        "Gamma deadlines are monotone: '{}' ({narrow_at}, display rank {narrow_rank}) precedes '{}' ({broad_at}, display rank {broad_rank}).",
                        narrow.deadline_label.as_deref().unwrap_or("unknown"),
                        broad.deadline_label.as_deref().unwrap_or("unknown")
                    ),
                    "Payoff hypothesis: an occurrence by the earlier deadline implies occurrence by the later deadline.".into(),
                ],
                minimum_payout_per_share: dec!(1),
                // Template inference is intentionally never live eligible.
                live_eligible: false,
                legs: vec![
                    relation_leg(broad, Outcome::Yes),
                    relation_leg(narrow, Outcome::No),
                ],
            });
        }
    }
    relationships
}

fn event_title_has_deadline_cue(title: &str) -> bool {
    let title = title.to_ascii_lowercase();
    title.contains(" by") || title.contains("before")
}

fn neg_risk_eligibility(
    event: &GammaEvent,
    max_legs: usize,
) -> std::result::Result<(), &'static str> {
    let deployed = event
        .markets
        .iter()
        .filter(|market| !market.condition_id.is_empty())
        .count();
    let markets = event
        .markets
        .iter()
        .filter_map(tradable_market)
        .collect::<Vec<_>>();
    if markets.len() != deployed {
        Err("some_deployed_markets_not_tradable")
    } else if markets.len() < 2 {
        Err("fewer_than_two_tradable_markets")
    } else if markets.len() > max_legs {
        Err("partition_leg_limit_exceeded")
    } else if !markets.iter().any(|market| market.neg_risk_other) {
        Err("missing_tradable_neg_risk_other")
    } else {
        Ok(())
    }
}

fn neg_risk_constraint_failures(event: &GammaEvent, max_legs: usize) -> Vec<&'static str> {
    let deployed = event
        .markets
        .iter()
        .filter(|market| !market.condition_id.is_empty())
        .count();
    let markets = event
        .markets
        .iter()
        .filter_map(tradable_market)
        .collect::<Vec<_>>();
    let mut failures = Vec::new();
    if markets.len() != deployed {
        failures.push("some_deployed_markets_not_tradable");
    }
    if markets.len() < 2 {
        failures.push("fewer_than_two_tradable_markets");
    }
    if markets.len() > max_legs {
        failures.push("partition_leg_limit_exceeded");
    }
    if !markets.iter().any(|market| market.neg_risk_other) {
        failures.push("missing_tradable_neg_risk_other");
    }
    failures
}

fn neg_risk_rejection_example(event: &GammaEvent) -> NegRiskRejectionExample {
    let deployed = event
        .markets
        .iter()
        .filter(|market| !market.condition_id.is_empty())
        .collect::<Vec<_>>();
    let tradable = deployed
        .iter()
        .filter_map(|market| tradable_market(market))
        .collect::<Vec<_>>();
    let mut market_rejections = BTreeMap::new();
    for market in &deployed {
        if let Some(reason) = market_tradability_rejection(market) {
            *market_rejections.entry(reason.to_string()).or_default() += 1;
        }
    }
    NegRiskRejectionExample {
        event_id: event.id.clone(),
        event_title: event.title.clone(),
        deployed_markets: deployed.len(),
        tradable_markets: tradable.len(),
        tradable_other_markets: tradable
            .iter()
            .filter(|market| market.neg_risk_other)
            .count(),
        market_rejections,
    }
}

fn discover_neg_risk_relationships(event: &GammaEvent, max_legs: usize) -> Vec<MarketRelationship> {
    if !(event.neg_risk || event.enable_neg_risk) {
        return Vec::new();
    }
    let deployed = event
        .markets
        .iter()
        .filter(|market| !market.condition_id.is_empty())
        .count();
    let markets = event
        .markets
        .iter()
        .filter_map(tradable_market)
        .collect::<Vec<_>>();
    if neg_risk_eligibility(event, max_legs).is_err() || markets.len() != deployed {
        return Vec::new();
    }
    let evidence = vec![
        "Gamma marks the event as NegRisk and every deployed question is currently tradable."
            .into(),
        "A tradable negRiskOther question is present, so the outcome set is treated as exhaustive."
            .into(),
        "NegRisk payoff hypothesis: exactly one component market resolves YES.".into(),
    ];
    vec![
        MarketRelationship {
            relationship_id: format!("neg-risk-yes:{}", event.id),
            event_id: event.id.clone(),
            event_slug: event.slug.clone(),
            event_title: event.title.clone(),
            kind: RelationshipKind::NegRiskCompleteSetYes,
            semantic_status: "protocol_encoded".into(),
            evidence: evidence.clone(),
            minimum_payout_per_share: dec!(1),
            live_eligible: false,
            legs: markets
                .iter()
                .map(|market| relation_leg(market, Outcome::Yes))
                .collect(),
        },
        MarketRelationship {
            relationship_id: format!("neg-risk-no:{}", event.id),
            event_id: event.id.clone(),
            event_slug: event.slug.clone(),
            event_title: event.title.clone(),
            kind: RelationshipKind::NegRiskCompleteSetNo,
            semantic_status: "protocol_encoded".into(),
            evidence,
            minimum_payout_per_share: Decimal::from(markets.len() as u64) - dec!(1),
            live_eligible: false,
            legs: markets
                .iter()
                .map(|market| relation_leg(market, Outcome::No))
                .collect(),
        },
    ]
}

fn tradable_market(market: &GammaMarket) -> Option<TradableMarket> {
    if market_tradability_rejection(market).is_some() {
        return None;
    }
    let tokens = string_array(&market.clob_token_ids)?;
    if tokens.len() != 2 {
        return None;
    }
    let outcomes =
        string_array(&market.outcomes).unwrap_or_else(|| vec!["Yes".into(), "No".into()]);
    let yes_index = outcomes
        .iter()
        .position(|outcome| outcome.eq_ignore_ascii_case("yes"))
        .unwrap_or(0);
    let no_index = outcomes
        .iter()
        .position(|outcome| outcome.eq_ignore_ascii_case("no"))
        .unwrap_or(1);
    let fee = market.fee_schedule.as_ref();
    Some(TradableMarket {
        condition_id: market.condition_id.clone(),
        question: market.question.clone(),
        yes_token_id: tokens.get(yes_index)?.clone(),
        no_token_id: tokens.get(no_index)?.clone(),
        deadline_at: market.end_date,
        deadline_rank: market
            .group_item_threshold
            .as_deref()
            .and_then(|value| value.parse::<i64>().ok()),
        deadline_label: market.group_item_title.clone(),
        resolution_source: market.resolution_source.trim().to_ascii_lowercase(),
        neg_risk_other: market.neg_risk_other,
        fee: FeePolicy {
            enabled: market.fees_enabled,
            rate: fee
                .map(|policy| decimal_value(&policy.rate))
                .unwrap_or_default(),
            exponent: fee.map(|policy| policy.exponent).unwrap_or(1).max(1),
        },
    })
}

fn market_tradability_rejection(market: &GammaMarket) -> Option<&'static str> {
    if market.condition_id.is_empty() {
        Some("missing_condition_id")
    } else if !market.active {
        Some("inactive")
    } else if market.closed {
        Some("closed")
    } else if !market.accepting_orders {
        Some("not_accepting_orders")
    } else if !market.enable_order_book {
        Some("order_book_disabled")
    } else if market.end_date.is_some_and(|end| end <= Utc::now()) {
        Some("expired")
    } else if string_array(&market.clob_token_ids).is_none_or(|tokens| tokens.len() != 2) {
        Some("invalid_binary_token_ids")
    } else {
        None
    }
}

fn relation_leg(market: &TradableMarket, outcome: Outcome) -> RelationLeg {
    RelationLeg {
        condition_id: market.condition_id.clone(),
        question: market.question.clone(),
        outcome,
        token_id: match outcome {
            Outcome::Yes => market.yes_token_id.clone(),
            Outcome::No => market.no_token_id.clone(),
        },
        fee: market.fee.clone(),
    }
}

fn same_resolution_source(left: &TradableMarket, right: &TradableMarket) -> bool {
    left.resolution_source.is_empty()
        || right.resolution_source.is_empty()
        || left.resolution_source == right.resolution_source
}

fn looks_like_deadline_label(label: &str) -> bool {
    let lower = label.to_ascii_lowercase();
    const MONTHS: [&str; 12] = [
        "january",
        "february",
        "march",
        "april",
        "may",
        "june",
        "july",
        "august",
        "september",
        "october",
        "november",
        "december",
    ];
    MONTHS.iter().any(|month| lower.contains(month))
        || lower
            .split(|character: char| !character.is_ascii_digit())
            .any(|token| token.len() == 4 && token.parse::<u16>().is_ok_and(|year| year >= 2020))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_limit_rotates_through_truncated_candidates() {
        let first = rotate_and_limit((0..7).collect::<Vec<_>>(), 0, 3);
        let second = rotate_and_limit((0..7).collect::<Vec<_>>(), 3, 3);
        let third = rotate_and_limit((0..7).collect::<Vec<_>>(), 6, 3);
        assert_eq!(first, (vec![0, 1, 2], 0, 4));
        assert_eq!(second, (vec![3, 4, 5], 3, 4));
        assert_eq!(third, (vec![6, 0, 1], 6, 4));
        let seen = first
            .0
            .into_iter()
            .chain(second.0)
            .chain(third.0)
            .collect::<HashSet<_>>();
        assert_eq!(seen.len(), 7);
    }

    #[test]
    fn keyset_envelope_requires_events_and_accepts_terminal_cursor() {
        let page: GammaEventPage =
            serde_json::from_value(serde_json::json!({"events":[],"next_cursor":"opaque"}))
                .unwrap();
        assert_eq!(page.next_cursor.as_deref(), Some("opaque"));
        let last: GammaEventPage =
            serde_json::from_value(serde_json::json!({"events":[],"next_cursor":null})).unwrap();
        assert!(last.next_cursor.is_none());
        assert!(serde_json::from_value::<GammaEventPage>(
            serde_json::json!({"error":"bad request"})
        )
        .is_err());
    }

    fn gamma_market(condition: &str, rank: i64, other: bool) -> GammaMarket {
        GammaMarket {
            condition_id: condition.into(),
            question: format!("Question {rank}"),
            active: true,
            closed: false,
            accepting_orders: true,
            enable_order_book: true,
            end_date: Some(Utc::now() + chrono::Duration::days(30)),
            clob_token_ids: serde_json::json!([
                format!("yes-{condition}"),
                format!("no-{condition}")
            ]),
            outcomes: serde_json::json!(["Yes", "No"]),
            group_item_threshold: Some(rank.to_string()),
            group_item_title: Some(if rank == 0 {
                "June 30, 2027".into()
            } else {
                "December 31, 2027".into()
            }),
            resolution_source: "official".into(),
            neg_risk_other: other,
            fees_enabled: false,
            fee_schedule: None,
        }
    }

    #[test]
    fn deadline_chain_builds_correct_dominance_legs() {
        let event = GammaEvent {
            id: "1".into(),
            title: "Something by...?".into(),
            slug: "something-by".into(),
            neg_risk: false,
            enable_neg_risk: false,
            markets: vec![
                gamma_market("early", 0, false),
                gamma_market("late", 1, false),
            ],
        };
        let relations = discover_deadline_relationships(&event);
        assert_eq!(relations.len(), 1);
        assert_eq!(relations[0].legs[0].token_id, "yes-late");
        assert_eq!(relations[0].legs[1].token_id, "no-early");
        assert!(!relations[0].live_eligible);
    }

    #[test]
    fn neg_risk_requires_other_and_every_market_tradable() {
        let mut event = GammaEvent {
            id: "2".into(),
            title: "Winner".into(),
            slug: "winner".into(),
            neg_risk: true,
            enable_neg_risk: true,
            markets: vec![gamma_market("a", 0, false), gamma_market("other", 1, true)],
        };
        assert_eq!(discover_neg_risk_relationships(&event, 10).len(), 2);
        event.markets[1].active = false;
        assert!(discover_neg_risk_relationships(&event, 10).is_empty());
        assert_eq!(
            market_tradability_rejection(&event.markets[1]),
            Some("inactive")
        );
        assert_eq!(
            neg_risk_eligibility(&event, 10),
            Err("some_deployed_markets_not_tradable")
        );
        let failures = neg_risk_constraint_failures(&event, 10);
        assert!(failures.contains(&"some_deployed_markets_not_tradable"));
        assert!(failures.contains(&"missing_tradable_neg_risk_other"));
        let example = neg_risk_rejection_example(&event);
        assert_eq!(example.deployed_markets, 2);
        assert_eq!(example.tradable_markets, 1);
        assert_eq!(example.tradable_other_markets, 0);
        assert_eq!(example.market_rejections.get("inactive"), Some(&1));
    }

    #[test]
    fn non_date_group_labels_cannot_become_deadline_relations() {
        let mut france = gamma_market("france", 0, false);
        france.group_item_title = Some("France".into());
        let mut germany = gamma_market("germany", 1, false);
        germany.group_item_title = Some("Germany".into());
        let event = GammaEvent {
            id: "3".into(),
            title: "Which countries act before 2027?".into(),
            slug: "countries-before-2027".into(),
            neg_risk: false,
            enable_neg_risk: false,
            markets: vec![france, germany],
        };
        assert!(discover_deadline_relationships(&event).is_empty());
        assert!(looks_like_deadline_label("June 30, 2027"));
        assert!(!looks_like_deadline_label("France"));
    }

    #[test]
    fn semantic_review_statuses_are_normalized_fail_closed() {
        assert_eq!(
            normalize_review_status(" RULES_EQUIVALENT "),
            "rules_equivalent"
        );
        assert_eq!(
            normalize_review_status("manually_approved"),
            "manually_approved"
        );
        assert_eq!(
            normalize_review_status("rejected_semantic_mismatch"),
            "rejected_semantic_mismatch"
        );
        assert_eq!(
            normalize_review_status("approved_typo"),
            "review_status_invalid_fail_closed"
        );
    }
}
