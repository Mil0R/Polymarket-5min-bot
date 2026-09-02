use super::boundary::analyze_boundaries;
use super::config::BayesianConsistencyConfig;
use super::discovery::{
    discover_partition_candidates, load_seed_file, rules_hash, CandidateSeed, DiscoveredContract,
    DiscoveryCandidate, DiscoveryEvent, DiscoveryReport,
};
use super::evaluator::{BookLevel, BookSnapshot};
use super::model::{HedgeBundle, PartitionContract, ValidatedPartition};
use anyhow::{Context, Result};
use chrono::Utc;
use futures::{stream, StreamExt};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::time::Instant;

#[derive(Clone)]
pub struct BayesianBookClient {
    config: BayesianConsistencyConfig,
    http: reqwest::Client,
    gamma_http: reqwest::Client,
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

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GammaEvent {
    id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    slug: String,
    #[serde(default)]
    markets: Vec<GammaMarket>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GammaMarket {
    #[serde(default)]
    id: String,
    #[serde(default)]
    condition_id: String,
    #[serde(default)]
    question: String,
    #[serde(default)]
    group_item_title: String,
    #[serde(default)]
    end_date: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    resolution_source: String,
    #[serde(default)]
    active: bool,
    #[serde(default)]
    closed: bool,
    #[serde(default)]
    accepting_orders: bool,
    #[serde(default)]
    enable_order_book: bool,
    #[serde(default)]
    clob_token_ids: Value,
    #[serde(default)]
    outcomes: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContractAudit {
    pub condition_id: String,
    pub question: String,
    pub passed: bool,
    pub violations: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryAuditReport {
    pub checked_at: chrono::DateTime<chrono::Utc>,
    pub checked: usize,
    pub passed: usize,
    pub failed: usize,
    pub contracts: Vec<ContractAudit>,
}

impl BayesianBookClient {
    pub fn new(config: BayesianConsistencyConfig) -> Result<Self> {
        Ok(Self {
            config,
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(6))
                .build()?,
            gamma_http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()?,
        })
    }

    pub async fn fetch_books(
        &self,
        relationships: &[ValidatedPartition],
    ) -> HashMap<String, BookSnapshot> {
        let tokens = relationships
            .iter()
            .flat_map(|relationship| relationship.bundles.iter())
            .flat_map(|bundle| bundle.legs.iter().map(|leg| leg.token_id.clone()))
            .collect::<HashSet<_>>();
        let requests = tokens.into_iter().map(|token| {
            let client = self.clone();
            async move {
                let result = client.fetch_book(&token).await;
                (token, result)
            }
        });
        stream::iter(requests)
            .buffer_unordered(self.config.request_concurrency)
            .filter_map(|(token, result)| async move {
                match result {
                    Ok(book) => Some((token, book)),
                    Err(error) => {
                        tracing::warn!(%token, %error, "Bayesian consistency CLOB book request failed");
                        None
                    }
                }
            })
            .collect()
            .await
    }

    pub async fn discover_candidates(&self) -> Result<DiscoveryReport> {
        let mut events_seen = 0_usize;
        let mut discovery_events = Vec::new();
        let mut seen_events = HashSet::new();
        for page in 0..self.config.event_pages {
            let offset = page * self.config.events_per_page;
            let events = self.fetch_gamma_page(offset, false).await?;
            let page_size = events.len();
            for event in events {
                if !seen_events.insert(event.id.clone()) {
                    continue;
                }
                events_seen = events_seen.saturating_add(1);
                let markets = event
                    .markets
                    .iter()
                    .filter_map(|market| discovered_contract(&event, market, true))
                    .collect::<Vec<_>>();
                if !markets.is_empty() {
                    discovery_events.push(DiscoveryEvent {
                        id: event.id,
                        title: event.title,
                        markets,
                    });
                }
            }
            if page_size < self.config.events_per_page {
                break;
            }
        }
        let tradable_markets_seen = discovery_events
            .iter()
            .map(|event| event.markets.len())
            .sum();
        let mut candidates = discover_partition_candidates(
            &discovery_events,
            self.config.max_children,
            self.config.max_discovery_candidates,
        );
        let mut seed_errors = Vec::new();
        let seed_file = load_seed_file(&self.config.seed_file, self.config.max_children)?;
        for seed in &seed_file.seeds {
            match self.discover_seed_candidate(seed).await {
                Ok(candidate) => candidates.push(candidate),
                Err(error) => seed_errors.push(format!("{}: {error:#}", seed.seed_id)),
            }
        }
        candidates.sort_by(|left, right| {
            discovery_priority(&left.discovery_source)
                .cmp(&discovery_priority(&right.discovery_source))
                .then_with(|| right.lexical_score.total_cmp(&left.lexical_score))
                .then_with(|| left.candidate_id.cmp(&right.candidate_id))
        });
        candidates.dedup_by(|left, right| left.candidate_id == right.candidate_id);
        candidates.truncate(self.config.max_discovery_candidates);
        Ok(DiscoveryReport {
            version: 1,
            scope: "active_tradable".into(),
            generated_at: Utc::now(),
            events_seen,
            tradable_markets_seen,
            seed_errors,
            candidates,
        })
    }

    pub async fn discover_historical_candidates(&self) -> Result<DiscoveryReport> {
        let mut events_seen = 0_usize;
        let mut discovery_events = Vec::new();
        let mut seen_events = HashSet::new();
        for page in 0..self.config.event_pages {
            let offset = page * self.config.events_per_page;
            let events = self.fetch_gamma_page(offset, true).await?;
            let page_size = events.len();
            for event in events {
                if !seen_events.insert(event.id.clone()) {
                    continue;
                }
                events_seen = events_seen.saturating_add(1);
                let markets = event
                    .markets
                    .iter()
                    .filter_map(|market| discovered_contract(&event, market, false))
                    .collect::<Vec<_>>();
                if !markets.is_empty() {
                    discovery_events.push(DiscoveryEvent {
                        id: event.id,
                        title: event.title,
                        markets,
                    });
                }
            }
            if page_size < self.config.events_per_page {
                break;
            }
        }
        let markets_seen = discovery_events
            .iter()
            .map(|event| event.markets.len())
            .sum();
        let candidates = discover_partition_candidates(
            &discovery_events,
            self.config.max_children,
            self.config.max_discovery_candidates,
        );
        Ok(DiscoveryReport {
            version: 1,
            scope: "historical_closed_research_only".into(),
            generated_at: Utc::now(),
            events_seen,
            tradable_markets_seen: markets_seen,
            seed_errors: vec![],
            candidates,
        })
    }

    pub async fn analyze_boundary_candidates(&self) -> Result<super::BoundaryReport> {
        let mut discovery_events = Vec::new();
        let mut seen_events = HashSet::new();
        for page in 0..self.config.event_pages {
            let offset = page * self.config.events_per_page;
            let events = self.fetch_gamma_page(offset, false).await?;
            let page_size = events.len();
            for event in events {
                if !seen_events.insert(event.id.clone()) {
                    continue;
                }
                let markets = event
                    .markets
                    .iter()
                    .filter_map(|market| discovered_contract(&event, market, true))
                    .collect::<Vec<_>>();
                if !markets.is_empty() {
                    discovery_events.push(DiscoveryEvent {
                        id: event.id,
                        title: event.title,
                        markets,
                    });
                }
            }
            if page_size < self.config.events_per_page {
                break;
            }
        }
        Ok(analyze_boundaries(
            &discovery_events,
            self.config.max_discovery_candidates,
        ))
    }

    async fn discover_seed_candidate(&self, seed: &CandidateSeed) -> Result<DiscoveryCandidate> {
        let parent = self
            .fetch_gamma_market(&seed.parent_condition_id)
            .await
            .context("parent seed lookup failed")?;
        let parent = seeded_contract(seed, parent)?;
        let mut children = Vec::with_capacity(seed.child_condition_ids.len());
        for condition_id in &seed.child_condition_ids {
            let market = self
                .fetch_gamma_market(condition_id)
                .await
                .with_context(|| format!("child seed lookup failed for {condition_id}"))?;
            children.push(seeded_contract(seed, market)?);
        }
        let mut evidence = vec![
            "Explicit condition-ID seed; no semantic relationship was inferred automatically."
                .into(),
            "Every seeded condition resolved to a currently open binary Gamma market.".into(),
        ];
        evidence.extend(seed.notes.iter().cloned());
        if std::iter::once(&parent)
            .chain(children.iter())
            .map(|contract| contract.resolution_source.trim())
            .all_equal()
        {
            evidence.push("Resolution source strings are identical across seeded legs.".into());
        } else {
            evidence.push("WARNING: resolution source strings differ across seeded legs.".into());
        }
        Ok(DiscoveryCandidate {
            candidate_id: format!("seed:{}", seed.seed_id),
            status: "pending".into(),
            discovery_source: "condition_id_seed".into(),
            lexical_score: 0.0,
            parent,
            children,
            evidence,
            required_reviews: vec![
                "Prove children are mutually exclusive from full resolution rules.".into(),
                "Prove children exhaust exactly the parent's YES states.".into(),
                "Review Other, tie, cancellation, invalid and clarification clauses.".into(),
                "Verify dates, data sources and resolution authorities are aligned.".into(),
                "A condition-ID seed never constitutes approval.".into(),
            ],
        })
    }

    pub async fn audit_relationships(
        &self,
        relationships: &[ValidatedPartition],
    ) -> RegistryAuditReport {
        let contracts = relationships
            .iter()
            .flat_map(|relationship| {
                std::iter::once(&relationship.relationship.parent)
                    .chain(relationship.relationship.children.iter())
            })
            .fold(
                HashMap::<String, PartitionContract>::new(),
                |mut items, contract| {
                    items
                        .entry(contract.condition_id.clone())
                        .or_insert_with(|| contract.clone());
                    items
                },
            );
        let requests = contracts.into_values().map(|contract| {
            let client = self.clone();
            async move { client.audit_contract(contract).await }
        });
        let mut audits = stream::iter(requests)
            .buffer_unordered(self.config.request_concurrency)
            .collect::<Vec<_>>()
            .await;
        audits.sort_by(|left, right| left.condition_id.cmp(&right.condition_id));
        let passed = audits.iter().filter(|audit| audit.passed).count();
        RegistryAuditReport {
            checked_at: Utc::now(),
            checked: audits.len(),
            passed,
            failed: audits.len().saturating_sub(passed),
            contracts: audits,
        }
    }

    async fn audit_contract(&self, expected: PartitionContract) -> ContractAudit {
        let result = self.fetch_gamma_market(&expected.condition_id).await;
        let violations = match result {
            Ok(current) => compare_contract(&expected, &current),
            Err(error) => vec![format!("gamma_lookup_failed: {error:#}")],
        };
        ContractAudit {
            condition_id: expected.condition_id,
            question: expected.question,
            passed: violations.is_empty(),
            violations,
        }
    }

    async fn fetch_gamma_market(&self, condition_id: &str) -> Result<GammaMarket> {
        let markets = self
            .gamma_http
            .get(format!(
                "{}/markets",
                self.config.gamma_url.trim_end_matches('/')
            ))
            .query(&[("condition_ids", condition_id)])
            .send()
            .await
            .context("Gamma condition lookup failed")?
            .error_for_status()
            .context("Gamma condition lookup returned non-success")?
            .json::<Vec<GammaMarket>>()
            .await
            .context("Gamma condition lookup response is invalid")?;
        markets
            .into_iter()
            .find(|market| market.condition_id == condition_id)
            .with_context(|| format!("Gamma returned no exact market for {condition_id}"))
    }

    async fn fetch_gamma_page(&self, offset: usize, closed: bool) -> Result<Vec<GammaEvent>> {
        let mut last_error = None;
        for attempt in 1..=3_u64 {
            let result = async {
                self.gamma_http
                    .get(format!(
                        "{}/events",
                        self.config.gamma_url.trim_end_matches('/')
                    ))
                    .query(&[
                        ("active", "true"),
                        ("closed", if closed { "true" } else { "false" }),
                        ("limit", &self.config.events_per_page.to_string()),
                        ("offset", &offset.to_string()),
                    ])
                    .send()
                    .await
                    .context("Gamma events request failed")?
                    .error_for_status()
                    .context("Gamma events returned non-success")?
                    .json::<Vec<GammaEvent>>()
                    .await
                    .context("Gamma events response is invalid")
            }
            .await;
            match result {
                Ok(events) => return Ok(events),
                Err(error) => {
                    tracing::warn!(attempt, offset, %error, "Bayesian Gamma discovery page failed; retrying");
                    last_error = Some(error);
                    if attempt < 3 {
                        tokio::time::sleep(std::time::Duration::from_millis(500 * attempt)).await;
                    }
                }
            }
        }
        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("Gamma page failed without an error")))
            .with_context(|| format!("Bayesian discovery failed at Gamma offset {offset}"))
    }

    /// Fetch one leg after another to approximate the time skew of non-atomic
    /// multi-market execution.  No order is ever constructed or submitted.
    pub async fn fetch_bundle_sequential(
        &self,
        bundle: &HedgeBundle,
    ) -> HashMap<String, BookSnapshot> {
        let mut books = HashMap::new();
        for leg in &bundle.legs {
            match self.fetch_book(&leg.token_id).await {
                Ok(book) => {
                    books.insert(leg.token_id.clone(), book);
                }
                Err(error) => {
                    tracing::warn!(token = %leg.token_id, %error,
                        "Bayesian sequential shadow book request failed");
                }
            }
        }
        books
    }

    async fn fetch_book(&self, token_id: &str) -> Result<BookSnapshot> {
        let started = Instant::now();
        let response: ClobBookResponse = self
            .http
            .get(format!(
                "{}/book",
                self.config.clob_url.trim_end_matches('/')
            ))
            .query(&[("token_id", token_id)])
            .send()
            .await
            .context("CLOB book request failed")?
            .error_for_status()
            .context("CLOB book returned non-success")?
            .json()
            .await
            .context("CLOB book response is invalid")?;
        let mut bids = levels(response.bids);
        bids.sort_by_key(|level| std::cmp::Reverse(level.price));
        let mut asks = levels(response.asks);
        asks.sort_by_key(|level| level.price);
        Ok(BookSnapshot {
            token_id: token_id.to_string(),
            observed_at: Utc::now(),
            exchange_timestamp_ms: response.timestamp.as_ref().and_then(value_i64),
            request_latency_ms: started.elapsed().as_millis() as u64,
            bids,
            asks,
        })
    }
}

fn seeded_contract(seed: &CandidateSeed, market: GammaMarket) -> Result<DiscoveredContract> {
    if !market.active
        || market.closed
        || !market.accepting_orders
        || !market.enable_order_book
        || market.condition_id.trim().is_empty()
        || market.question.trim().is_empty()
    {
        anyhow::bail!(
            "seeded market {} is not currently tradable",
            market.condition_id
        );
    }
    let tokens = string_array(&market.clob_token_ids).context("seeded market has no tokens")?;
    let outcomes = string_array(&market.outcomes).context("seeded market has no outcomes")?;
    if tokens.len() != 2 || outcomes.len() != 2 {
        anyhow::bail!("seeded market is not binary");
    }
    let yes = outcomes
        .iter()
        .position(|outcome| outcome.eq_ignore_ascii_case("yes"))
        .context("seeded market has no YES outcome")?;
    let no = outcomes
        .iter()
        .position(|outcome| outcome.eq_ignore_ascii_case("no"))
        .context("seeded market has no NO outcome")?;
    Ok(DiscoveredContract {
        market_id: market.id.clone(),
        event_id: format!("seed-market:{}", market.id),
        event_title: seed.title.clone(),
        event_slug: seed.seed_id.clone(),
        group_item_title: market.group_item_title.clone(),
        end_date: market.end_date.clone(),
        condition_id: market.condition_id.clone(),
        question: market.question.clone(),
        yes_token_id: tokens[yes].clone(),
        no_token_id: tokens[no].clone(),
        description: market.description.clone(),
        resolution_source: market.resolution_source.clone(),
        rules_hash: rules_hash(
            &market.question,
            &market.description,
            &market.resolution_source,
        ),
        tradable: true,
    })
}

fn discovery_priority(source: &str) -> u8 {
    if source == "condition_id_seed" {
        0
    } else {
        1
    }
}

trait IteratorAllEqual: Iterator {
    fn all_equal(mut self) -> bool
    where
        Self: Sized,
        Self::Item: PartialEq,
    {
        let Some(first) = self.next() else {
            return true;
        };
        self.all(|item| item == first)
    }
}

impl<T: Iterator> IteratorAllEqual for T {}

fn compare_contract(expected: &PartitionContract, current: &GammaMarket) -> Vec<String> {
    let mut violations = Vec::new();
    if current.question != expected.question {
        violations.push("question_changed".into());
    }
    if !current.active || current.closed || !current.accepting_orders || !current.enable_order_book
    {
        violations.push("market_not_open_for_orderbook_trading".into());
    }
    let tokens = string_array(&current.clob_token_ids);
    let outcomes = string_array(&current.outcomes);
    match (tokens, outcomes) {
        (Some(tokens), Some(outcomes)) if tokens.len() == 2 && outcomes.len() == 2 => {
            let yes = outcomes
                .iter()
                .position(|outcome| outcome.eq_ignore_ascii_case("yes"));
            let no = outcomes
                .iter()
                .position(|outcome| outcome.eq_ignore_ascii_case("no"));
            match (yes, no) {
                (Some(yes), Some(no)) => {
                    if tokens[yes] != expected.yes_token_id || tokens[no] != expected.no_token_id {
                        violations.push("token_ids_changed".into());
                    }
                }
                _ => violations.push("binary_yes_no_outcomes_missing".into()),
            }
        }
        _ => violations.push("binary_token_metadata_invalid".into()),
    }
    let current_hash = rules_hash(
        &current.question,
        &current.description,
        &current.resolution_source,
    );
    if current_hash != expected.rules_hash {
        violations.push("rules_hash_changed".into());
    }
    violations
}

fn discovered_contract(
    event: &GammaEvent,
    market: &GammaMarket,
    require_tradable: bool,
) -> Option<DiscoveredContract> {
    let tradable =
        market.active && !market.closed && market.accepting_orders && market.enable_order_book;
    if (require_tradable && !tradable)
        || market.condition_id.trim().is_empty()
        || market.question.trim().is_empty()
    {
        return None;
    }
    let tokens = string_array(&market.clob_token_ids)?;
    let outcomes =
        string_array(&market.outcomes).unwrap_or_else(|| vec!["Yes".into(), "No".into()]);
    if tokens.len() != 2 || outcomes.len() != 2 {
        return None;
    }
    let yes_index = outcomes
        .iter()
        .position(|outcome| outcome.eq_ignore_ascii_case("yes"))?;
    let no_index = outcomes
        .iter()
        .position(|outcome| outcome.eq_ignore_ascii_case("no"))?;
    Some(DiscoveredContract {
        market_id: market.id.clone(),
        event_id: event.id.clone(),
        event_title: event.title.clone(),
        event_slug: event.slug.clone(),
        group_item_title: market.group_item_title.clone(),
        end_date: market.end_date.clone(),
        condition_id: market.condition_id.clone(),
        question: market.question.clone(),
        yes_token_id: tokens[yes_index].clone(),
        no_token_id: tokens[no_index].clone(),
        description: market.description.clone(),
        resolution_source: market.resolution_source.clone(),
        rules_hash: rules_hash(
            &market.question,
            &market.description,
            &market.resolution_source,
        ),
        tradable,
    })
}

fn levels(raw: Vec<ClobLevel>) -> Vec<BookLevel> {
    raw.into_iter()
        .filter_map(|level| {
            let price = Decimal::from_str(&level.price).ok()?;
            let size = Decimal::from_str(&level.size).ok()?;
            (price > Decimal::ZERO && size > Decimal::ZERO).then_some(BookLevel { price, size })
        })
        .collect()
}

fn value_i64(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_str()?.parse::<i64>().ok())
}

fn string_array(value: &Value) -> Option<Vec<String>> {
    if let Some(values) = value.as_array() {
        return Some(
            values
                .iter()
                .filter_map(|value| value.as_str().map(str::to_string))
                .collect(),
        );
    }
    let encoded = value.as_str()?;
    serde_json::from_str(encoded).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn gamma_market() -> GammaMarket {
        GammaMarket {
            id: "market-id".into(),
            condition_id: "condition".into(),
            question: "Will the parent occur?".into(),
            group_item_title: String::new(),
            end_date: String::new(),
            description: "Reviewed rules".into(),
            resolution_source: "https://example.com/source".into(),
            active: true,
            closed: false,
            accepting_orders: true,
            enable_order_book: true,
            clob_token_ids: serde_json::json!(["yes", "no"]),
            outcomes: serde_json::json!(["Yes", "No"]),
        }
    }

    fn expected(market: &GammaMarket) -> PartitionContract {
        PartitionContract {
            condition_id: market.condition_id.clone(),
            question: market.question.clone(),
            yes_token_id: "yes".into(),
            no_token_id: "no".into(),
            rules_hash: rules_hash(
                &market.question,
                &market.description,
                &market.resolution_source,
            ),
            fee_rate: dec!(0.07),
            fee_exponent: 1,
        }
    }

    #[test]
    fn unchanged_open_contract_passes_live_audit() {
        let market = gamma_market();
        assert!(compare_contract(&expected(&market), &market).is_empty());
    }

    #[test]
    fn rules_token_and_status_drift_fail_live_audit() {
        let market = gamma_market();
        let expected = expected(&market);
        let mut changed = market;
        changed.description = "Changed rules".into();
        changed.clob_token_ids = serde_json::json!(["new-yes", "new-no"]);
        changed.accepting_orders = false;
        let failures = compare_contract(&expected, &changed);
        assert!(failures.contains(&"rules_hash_changed".to_string()));
        assert!(failures.contains(&"token_ids_changed".to_string()));
        assert!(failures.contains(&"market_not_open_for_orderbook_trading".to_string()));
    }
}
