use super::config::BayesianConsistencyConfig;
use super::model::{HedgeBundle, HedgeDirection};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BookLevel {
    pub price: Decimal,
    pub size: Decimal,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BookSnapshot {
    pub token_id: String,
    pub observed_at: DateTime<Utc>,
    pub exchange_timestamp_ms: Option<i64>,
    pub request_latency_ms: u64,
    pub bids: Vec<BookLevel>,
    pub asks: Vec<BookLevel>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LegFill {
    pub token_id: String,
    pub condition_id: String,
    pub outcome: String,
    pub shares: Decimal,
    pub average_price: Decimal,
    pub raw_cost_usdc: Decimal,
    pub taker_fee_usdc: Decimal,
    pub request_latency_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BundleEvaluation {
    pub relationship_id: String,
    pub direction: HedgeDirection,
    pub observed_at: DateTime<Utc>,
    pub size_shares: Option<Decimal>,
    pub guaranteed_payout_usdc: Option<Decimal>,
    pub raw_cost_usdc: Option<Decimal>,
    pub taker_fee_usdc: Option<Decimal>,
    pub execution_buffer_usdc: Option<Decimal>,
    pub net_profit_usdc: Option<Decimal>,
    pub net_edge: Option<Decimal>,
    pub qualifies: bool,
    pub rejection_reason: Option<String>,
    pub max_book_age_ms: Option<i64>,
    pub max_leg_snapshot_skew_ms: Option<i64>,
    pub max_exchange_snapshot_skew_ms: Option<i64>,
    pub max_rest_latency_ms: Option<u64>,
    pub legs: Vec<LegFill>,
}

pub fn evaluate_bundle(
    bundle: &HedgeBundle,
    books: &HashMap<String, BookSnapshot>,
    config: &BayesianConsistencyConfig,
    now: DateTime<Utc>,
    liquidity_haircut: Decimal,
) -> BundleEvaluation {
    let telemetry = telemetry(bundle, books, now);
    if telemetry
        .max_book_age_ms
        .is_some_and(|age| age > config.max_book_age_ms)
    {
        return rejected(bundle, now, telemetry, "stale_book");
    }
    if telemetry
        .max_leg_snapshot_skew_ms
        .is_some_and(|skew| skew > config.max_leg_snapshot_skew_ms)
    {
        return rejected(bundle, now, telemetry, "leg_snapshot_skew_exceeded");
    }
    if bundle
        .legs
        .iter()
        .any(|leg| !books.contains_key(&leg.token_id))
    {
        return rejected(bundle, now, telemetry, "missing_book");
    }

    let mut best = None::<BundleEvaluation>;
    let mut last_reason = "insufficient_ask_depth";
    for shares in &config.size_ladder_shares {
        let mut fills = Vec::with_capacity(bundle.legs.len());
        let mut raw_cost = Decimal::ZERO;
        let mut fees = Decimal::ZERO;
        let mut complete = true;
        for leg in &bundle.legs {
            let book = &books[&leg.token_id];
            let Some((average_price, cost, fee)) = walk_asks(
                book,
                *shares,
                liquidity_haircut,
                leg.fee_rate,
                leg.fee_exponent,
            ) else {
                complete = false;
                last_reason = "insufficient_ask_depth";
                break;
            };
            raw_cost += cost;
            fees += fee;
            fills.push(LegFill {
                token_id: leg.token_id.clone(),
                condition_id: leg.condition_id.clone(),
                outcome: format!("{:?}", leg.outcome).to_ascii_lowercase(),
                shares: *shares,
                average_price,
                raw_cost_usdc: cost,
                taker_fee_usdc: fee,
                request_latency_ms: book.request_latency_ms,
            });
        }
        if !complete {
            continue;
        }
        let payout = bundle.guaranteed_payout_per_share * *shares;
        let buffer = payout * config.execution_buffer_pct;
        let profit = payout - raw_cost - fees - buffer;
        let edge = profit / payout;
        let enough_notional = raw_cost >= config.min_bundle_notional_usdc;
        let qualifies = enough_notional && edge >= config.min_net_edge;
        let reason = if qualifies {
            None
        } else if !enough_notional {
            last_reason = "bundle_notional_below_threshold";
            Some(last_reason.to_string())
        } else {
            last_reason = "net_edge_below_threshold";
            Some(last_reason.to_string())
        };
        let candidate = BundleEvaluation {
            relationship_id: bundle.relationship_id.clone(),
            direction: bundle.direction,
            observed_at: now,
            size_shares: Some(*shares),
            guaranteed_payout_usdc: Some(payout),
            raw_cost_usdc: Some(raw_cost),
            taker_fee_usdc: Some(fees),
            execution_buffer_usdc: Some(buffer),
            net_profit_usdc: Some(profit),
            net_edge: Some(edge),
            qualifies,
            rejection_reason: reason,
            max_book_age_ms: telemetry.max_book_age_ms,
            max_leg_snapshot_skew_ms: telemetry.max_leg_snapshot_skew_ms,
            max_exchange_snapshot_skew_ms: telemetry.max_exchange_snapshot_skew_ms,
            max_rest_latency_ms: telemetry.max_rest_latency_ms,
            legs: fills,
        };
        if best.as_ref().is_none_or(|current| {
            candidate.qualifies && !current.qualifies
                || candidate.qualifies == current.qualifies
                    && candidate.net_profit_usdc.unwrap_or(Decimal::MIN)
                        > current.net_profit_usdc.unwrap_or(Decimal::MIN)
        }) {
            best = Some(candidate);
        }
    }
    best.unwrap_or_else(|| rejected(bundle, now, telemetry, last_reason))
}

struct Telemetry {
    max_book_age_ms: Option<i64>,
    max_leg_snapshot_skew_ms: Option<i64>,
    max_exchange_snapshot_skew_ms: Option<i64>,
    max_rest_latency_ms: Option<u64>,
}

fn telemetry(
    bundle: &HedgeBundle,
    books: &HashMap<String, BookSnapshot>,
    now: DateTime<Utc>,
) -> Telemetry {
    let selected = bundle
        .legs
        .iter()
        .filter_map(|leg| books.get(&leg.token_id))
        .collect::<Vec<_>>();
    let observed = selected
        .iter()
        .map(|book| book.observed_at.timestamp_millis())
        .collect::<Vec<_>>();
    let exchange = selected
        .iter()
        .filter_map(|book| book.exchange_timestamp_ms)
        .collect::<Vec<_>>();
    Telemetry {
        max_book_age_ms: selected
            .iter()
            .map(|book| (now - book.observed_at).num_milliseconds().max(0))
            .max(),
        max_leg_snapshot_skew_ms: range(&observed),
        max_exchange_snapshot_skew_ms: (exchange.len() == bundle.legs.len())
            .then(|| range(&exchange))
            .flatten(),
        max_rest_latency_ms: selected.iter().map(|book| book.request_latency_ms).max(),
    }
}

fn range(values: &[i64]) -> Option<i64> {
    (values.len() >= 2).then(|| {
        values.iter().max().copied().unwrap_or_default()
            - values.iter().min().copied().unwrap_or_default()
    })
}

fn rejected(
    bundle: &HedgeBundle,
    now: DateTime<Utc>,
    telemetry: Telemetry,
    reason: &str,
) -> BundleEvaluation {
    BundleEvaluation {
        relationship_id: bundle.relationship_id.clone(),
        direction: bundle.direction,
        observed_at: now,
        size_shares: None,
        guaranteed_payout_usdc: None,
        raw_cost_usdc: None,
        taker_fee_usdc: None,
        execution_buffer_usdc: None,
        net_profit_usdc: None,
        net_edge: None,
        qualifies: false,
        rejection_reason: Some(reason.to_string()),
        max_book_age_ms: telemetry.max_book_age_ms,
        max_leg_snapshot_skew_ms: telemetry.max_leg_snapshot_skew_ms,
        max_exchange_snapshot_skew_ms: telemetry.max_exchange_snapshot_skew_ms,
        max_rest_latency_ms: telemetry.max_rest_latency_ms,
        legs: vec![],
    }
}

fn walk_asks(
    book: &BookSnapshot,
    shares: Decimal,
    liquidity_haircut: Decimal,
    fee_rate: Decimal,
    fee_exponent: u32,
) -> Option<(Decimal, Decimal, Decimal)> {
    let mut remaining = shares;
    let mut raw_cost = Decimal::ZERO;
    let mut fee = Decimal::ZERO;
    let mut asks = book.asks.iter().collect::<Vec<_>>();
    asks.sort_by_key(|level| level.price);
    for level in asks {
        if level.price <= Decimal::ZERO
            || level.price >= Decimal::ONE
            || level.size <= Decimal::ZERO
        {
            continue;
        }
        let fill = remaining.min(level.size * liquidity_haircut);
        raw_cost += fill * level.price;
        fee += fill * fee_per_share(fee_rate, fee_exponent, level.price);
        remaining -= fill;
        if remaining <= Decimal::ZERO {
            break;
        }
    }
    (remaining <= Decimal::ZERO && shares > Decimal::ZERO)
        .then(|| (raw_cost / shares, raw_cost, fee))
}

fn fee_per_share(rate: Decimal, exponent: u32, price: Decimal) -> Decimal {
    if rate <= Decimal::ZERO || price <= Decimal::ZERO || price >= Decimal::ONE {
        return Decimal::ZERO;
    }
    let base = price * (dec!(1) - price);
    rate * (0..exponent).fold(Decimal::ONE, |value, _| value * base)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bayesian_consistency_arbitrage::{
        PartitionContract, PartitionRelationship, RelationshipReviewStatus,
    };
    use chrono::Duration;

    fn contract(name: &str) -> PartitionContract {
        PartitionContract {
            condition_id: format!("condition-{name}"),
            question: name.into(),
            yes_token_id: format!("yes-{name}"),
            no_token_id: format!("no-{name}"),
            rules_hash: format!("sha256:{}", "a".repeat(64)),
            fee_rate: Decimal::ZERO,
            fee_exponent: 1,
        }
    }

    fn config() -> BayesianConsistencyConfig {
        BayesianConsistencyConfig {
            gamma_url: "https://gamma-api.polymarket.com".into(),
            clob_url: "https://clob.polymarket.com".into(),
            registry_file: "registry.json".into(),
            discovery_file: "candidates.json".into(),
            historical_discovery_file: "historical-candidates.json".into(),
            boundary_report_file: "boundary-diagnostics.json".into(),
            seed_file: "seeds.json".into(),
            review_file: "reviews.json".into(),
            results_file: "events.ndjson".into(),
            event_pages: 10,
            events_per_page: 100,
            max_discovery_candidates: 200,
            max_children: 12,
            request_concurrency: 12,
            poll_seconds: 10,
            duration_seconds: 0,
            size_ladder_shares: vec![dec!(10)],
            min_bundle_notional_usdc: dec!(1),
            min_net_edge: dec!(0.02),
            execution_buffer_pct: dec!(0.005),
            max_book_age_ms: 1000,
            max_leg_snapshot_skew_ms: 500,
            shadow_delay_ms: vec![100, 500, 1000],
            shadow_liquidity_haircuts: vec![dec!(1), dec!(0.75), dec!(0.5)],
            monitor_interval_seconds: 3600,
            monitor_duration_seconds: 0,
            monitor_results_file: "monitor.ndjson".into(),
            monitor_state_file: "state.json".into(),
            monitor_alert_on_new_candidate: true,
            monitor_alert_on_rule_drift: true,
        }
    }

    fn book(token: &str, ask: Decimal, observed_at: DateTime<Utc>) -> BookSnapshot {
        BookSnapshot {
            token_id: token.into(),
            observed_at,
            exchange_timestamp_ms: Some(observed_at.timestamp_millis()),
            request_latency_ms: 50,
            bids: vec![],
            asks: vec![BookLevel {
                price: ask,
                size: dec!(100),
            }],
        }
    }

    fn bundle() -> HedgeBundle {
        PartitionRelationship {
            relationship_id: "r1".into(),
            title: "partition".into(),
            status: RelationshipReviewStatus::ManuallyApproved,
            disjoint: true,
            exhaustive: true,
            reviewed_at: Some("2026-09-02T00:00:00Z".into()),
            notes: vec![],
            parent: contract("parent"),
            children: vec![contract("a"), contract("b")],
        }
        .validate(12)
        .unwrap()
        .bundles[0]
            .clone()
    }

    #[test]
    fn executable_asks_detect_children_cheap_bundle() {
        let now = Utc::now();
        let bundle = bundle();
        let books = HashMap::from([
            ("no-parent".into(), book("no-parent", dec!(0.40), now)),
            ("yes-a".into(), book("yes-a", dec!(0.20), now)),
            ("yes-b".into(), book("yes-b", dec!(0.20), now)),
        ]);
        let evaluation = evaluate_bundle(&bundle, &books, &config(), now, Decimal::ONE);
        assert!(evaluation.qualifies);
        assert_eq!(evaluation.raw_cost_usdc, Some(dec!(8.0)));
        assert_eq!(evaluation.net_profit_usdc, Some(dec!(1.95)));
    }

    #[test]
    fn stale_or_skewed_books_fail_closed() {
        let now = Utc::now();
        let bundle = bundle();
        let old = now - Duration::seconds(2);
        let books = HashMap::from([
            ("no-parent".into(), book("no-parent", dec!(0.4), old)),
            ("yes-a".into(), book("yes-a", dec!(0.2), now)),
            ("yes-b".into(), book("yes-b", dec!(0.2), now)),
        ]);
        let evaluation = evaluate_bundle(&bundle, &books, &config(), now, Decimal::ONE);
        assert_eq!(evaluation.rejection_reason.as_deref(), Some("stale_book"));
    }

    #[test]
    fn conservative_liquidity_haircut_can_reject_a_signal_size() {
        let now = Utc::now();
        let bundle = bundle();
        let shallow = |token: &str, ask: Decimal| BookSnapshot {
            token_id: token.into(),
            observed_at: now,
            exchange_timestamp_ms: Some(now.timestamp_millis()),
            request_latency_ms: 50,
            bids: vec![],
            asks: vec![BookLevel {
                price: ask,
                size: dec!(10),
            }],
        };
        let books = HashMap::from([
            ("no-parent".into(), shallow("no-parent", dec!(0.4))),
            ("yes-a".into(), shallow("yes-a", dec!(0.2))),
            ("yes-b".into(), shallow("yes-b", dec!(0.2))),
        ]);
        let evaluation = evaluate_bundle(&bundle, &books, &config(), now, dec!(0.5));
        assert_eq!(
            evaluation.rejection_reason.as_deref(),
            Some("insufficient_ask_depth")
        );
    }

    #[test]
    fn fresh_but_desynchronized_legs_fail_closed() {
        let now = Utc::now();
        let bundle = bundle();
        let earlier = now - Duration::milliseconds(600);
        let books = HashMap::from([
            ("no-parent".into(), book("no-parent", dec!(0.4), earlier)),
            ("yes-a".into(), book("yes-a", dec!(0.2), now)),
            ("yes-b".into(), book("yes-b", dec!(0.2), now)),
        ]);
        let evaluation = evaluate_bundle(&bundle, &books, &config(), now, Decimal::ONE);
        assert_eq!(
            evaluation.rejection_reason.as_deref(),
            Some("leg_snapshot_skew_exceeded")
        );
    }
}
