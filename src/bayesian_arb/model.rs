use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Yes,
    No,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RelationshipKind {
    DeadlineImplication,
    NegRiskCompleteSetYes,
    NegRiskCompleteSetNo,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FeePolicy {
    pub enabled: bool,
    pub rate: Decimal,
    pub exponent: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RelationLeg {
    pub condition_id: String,
    pub question: String,
    pub outcome: Outcome,
    pub token_id: String,
    pub fee: FeePolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MarketRelationship {
    pub relationship_id: String,
    pub event_id: String,
    pub event_slug: String,
    pub event_title: String,
    pub kind: RelationshipKind,
    pub semantic_status: String,
    pub evidence: Vec<String>,
    pub minimum_payout_per_share: Decimal,
    pub live_eligible: bool,
    pub legs: Vec<RelationLeg>,
}

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
    pub asks: Vec<BookLevel>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EvaluatedLeg {
    pub condition_id: String,
    pub question: String,
    pub outcome: Outcome,
    pub token_id: String,
    pub average_price: Decimal,
    pub shares: Decimal,
    pub raw_cost_usdc: Decimal,
    pub taker_fee_usdc: Decimal,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RelationshipEvaluation {
    pub relationship_id: String,
    pub event_id: String,
    pub event_slug: String,
    pub event_title: String,
    pub kind: RelationshipKind,
    pub semantic_status: String,
    pub live_eligible: bool,
    pub observed_at: DateTime<Utc>,
    pub minimum_payout_per_share: Decimal,
    pub size_shares: Option<Decimal>,
    pub guaranteed_payout_usdc: Option<Decimal>,
    pub raw_cost_usdc: Option<Decimal>,
    pub taker_fee_usdc: Option<Decimal>,
    pub execution_buffer_usdc: Option<Decimal>,
    pub total_cost_usdc: Option<Decimal>,
    pub net_profit_usdc: Option<Decimal>,
    pub net_edge: Option<Decimal>,
    pub qualifies: bool,
    pub rejection_reason: Option<String>,
    pub legs: Vec<EvaluatedLeg>,
    /// Complete research curve for every configured size. Older journals omit it.
    #[serde(default)]
    pub size_evaluations: Vec<SizeEvaluation>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SizeEvaluation {
    pub size_shares: Decimal,
    pub guaranteed_payout_usdc: Decimal,
    pub raw_cost_usdc: Option<Decimal>,
    pub taker_fee_usdc: Option<Decimal>,
    pub execution_buffer_usdc: Option<Decimal>,
    pub total_cost_usdc: Option<Decimal>,
    pub net_profit_usdc: Option<Decimal>,
    pub net_edge: Option<Decimal>,
    pub bundle_notional_sufficient: bool,
    pub qualifies: bool,
    pub rejection_reason: Option<String>,
    /// Average execution price for each relationship leg, in relationship order.
    pub leg_average_prices: Vec<Decimal>,
    #[serde(default)]
    pub buffer_evaluations: Vec<BufferEvaluation>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BufferEvaluation {
    pub execution_buffer_pct: Decimal,
    pub execution_buffer_usdc: Decimal,
    pub total_cost_usdc: Decimal,
    pub net_profit_usdc: Decimal,
    pub net_edge: Decimal,
    pub qualifies: bool,
}

struct FillQuote {
    average_price: Decimal,
    raw_cost: Decimal,
    fee: Decimal,
}

pub fn evaluate_relationship(
    relationship: &MarketRelationship,
    books: &HashMap<String, BookSnapshot>,
    size_ladder: &[Decimal],
    min_bundle_notional: Decimal,
    min_net_edge: Decimal,
    execution_buffer_pct: Decimal,
    now: DateTime<Utc>,
) -> RelationshipEvaluation {
    evaluate_relationship_with_buffers(
        relationship,
        books,
        size_ladder,
        min_bundle_notional,
        min_net_edge,
        execution_buffer_pct,
        &[execution_buffer_pct],
        now,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn evaluate_relationship_with_buffers(
    relationship: &MarketRelationship,
    books: &HashMap<String, BookSnapshot>,
    size_ladder: &[Decimal],
    min_bundle_notional: Decimal,
    min_net_edge: Decimal,
    execution_buffer_pct: Decimal,
    execution_buffer_curve_pct: &[Decimal],
    now: DateTime<Utc>,
) -> RelationshipEvaluation {
    let mut best: Option<RelationshipEvaluation> = None;
    let mut last_failure = "no_size_has_complete_ask_depth".to_string();
    let mut size_evaluations = Vec::with_capacity(size_ladder.len());
    for size in size_ladder {
        let mut legs = Vec::with_capacity(relationship.legs.len());
        let mut raw_cost = Decimal::ZERO;
        let mut taker_fee = Decimal::ZERO;
        let mut complete = true;
        for leg in &relationship.legs {
            let Some(book) = books.get(&leg.token_id) else {
                last_failure = format!("missing_book:{}", leg.token_id);
                complete = false;
                break;
            };
            let Some(fill) = walk_asks(book, *size, &leg.fee) else {
                last_failure = format!("insufficient_ask_depth:{}", leg.token_id);
                complete = false;
                break;
            };
            raw_cost += fill.raw_cost;
            taker_fee += fill.fee;
            legs.push(EvaluatedLeg {
                condition_id: leg.condition_id.clone(),
                question: leg.question.clone(),
                outcome: leg.outcome,
                token_id: leg.token_id.clone(),
                average_price: fill.average_price,
                shares: *size,
                raw_cost_usdc: fill.raw_cost,
                taker_fee_usdc: fill.fee,
            });
        }
        if !complete {
            size_evaluations.push(SizeEvaluation {
                size_shares: *size,
                guaranteed_payout_usdc: relationship.minimum_payout_per_share * *size,
                raw_cost_usdc: None,
                taker_fee_usdc: None,
                execution_buffer_usdc: None,
                total_cost_usdc: None,
                net_profit_usdc: None,
                net_edge: None,
                bundle_notional_sufficient: false,
                qualifies: false,
                rejection_reason: Some(last_failure.clone()),
                leg_average_prices: Vec::new(),
                buffer_evaluations: Vec::new(),
            });
            continue;
        }
        let payout = relationship.minimum_payout_per_share * *size;
        let buffer = payout * execution_buffer_pct;
        let total_cost = raw_cost + taker_fee + buffer;
        let profit = payout - total_cost;
        let edge = if payout > Decimal::ZERO {
            profit / payout
        } else {
            Decimal::ZERO
        };
        let enough_notional = raw_cost >= min_bundle_notional;
        let qualifies = enough_notional && edge >= min_net_edge;
        let rejection_reason = if qualifies {
            None
        } else if !enough_notional {
            Some("bundle_notional_below_threshold".into())
        } else {
            Some("net_edge_below_threshold".into())
        };
        let buffer_evaluations = execution_buffer_curve_pct
            .iter()
            .map(|buffer_pct| {
                let scenario_buffer = payout * *buffer_pct;
                let scenario_total = raw_cost + taker_fee + scenario_buffer;
                let scenario_profit = payout - scenario_total;
                let scenario_edge = if payout > Decimal::ZERO {
                    scenario_profit / payout
                } else {
                    Decimal::ZERO
                };
                BufferEvaluation {
                    execution_buffer_pct: *buffer_pct,
                    execution_buffer_usdc: scenario_buffer,
                    total_cost_usdc: scenario_total,
                    net_profit_usdc: scenario_profit,
                    net_edge: scenario_edge,
                    qualifies: enough_notional && scenario_edge >= min_net_edge,
                }
            })
            .collect();
        size_evaluations.push(SizeEvaluation {
            size_shares: *size,
            guaranteed_payout_usdc: payout,
            raw_cost_usdc: Some(raw_cost),
            taker_fee_usdc: Some(taker_fee),
            execution_buffer_usdc: Some(buffer),
            total_cost_usdc: Some(total_cost),
            net_profit_usdc: Some(profit),
            net_edge: Some(edge),
            bundle_notional_sufficient: enough_notional,
            qualifies,
            rejection_reason: rejection_reason.clone(),
            leg_average_prices: legs.iter().map(|leg| leg.average_price).collect(),
            buffer_evaluations,
        });
        let candidate = RelationshipEvaluation {
            relationship_id: relationship.relationship_id.clone(),
            event_id: relationship.event_id.clone(),
            event_slug: relationship.event_slug.clone(),
            event_title: relationship.event_title.clone(),
            kind: relationship.kind,
            semantic_status: relationship.semantic_status.clone(),
            live_eligible: relationship.live_eligible,
            observed_at: now,
            minimum_payout_per_share: relationship.minimum_payout_per_share,
            size_shares: Some(*size),
            guaranteed_payout_usdc: Some(payout),
            raw_cost_usdc: Some(raw_cost),
            taker_fee_usdc: Some(taker_fee),
            execution_buffer_usdc: Some(buffer),
            total_cost_usdc: Some(total_cost),
            net_profit_usdc: Some(profit),
            net_edge: Some(edge),
            qualifies,
            rejection_reason,
            legs,
            size_evaluations: Vec::new(),
        };
        let replace = best.as_ref().is_none_or(|current| {
            if candidate.qualifies != current.qualifies {
                return candidate.qualifies;
            }
            if candidate.qualifies {
                return candidate.net_profit_usdc.unwrap_or_default()
                    > current.net_profit_usdc.unwrap_or_default();
            }
            let candidate_has_research_notional =
                candidate.rejection_reason.as_deref() != Some("bundle_notional_below_threshold");
            let current_has_research_notional =
                current.rejection_reason.as_deref() != Some("bundle_notional_below_threshold");
            if candidate_has_research_notional != current_has_research_notional {
                return candidate_has_research_notional;
            }
            candidate.net_edge.unwrap_or(Decimal::MIN) > current.net_edge.unwrap_or(Decimal::MIN)
        });
        if replace {
            best = Some(candidate);
        }
    }
    let mut result = best.unwrap_or_else(|| RelationshipEvaluation {
        relationship_id: relationship.relationship_id.clone(),
        event_id: relationship.event_id.clone(),
        event_slug: relationship.event_slug.clone(),
        event_title: relationship.event_title.clone(),
        kind: relationship.kind,
        semantic_status: relationship.semantic_status.clone(),
        live_eligible: relationship.live_eligible,
        observed_at: now,
        minimum_payout_per_share: relationship.minimum_payout_per_share,
        size_shares: None,
        guaranteed_payout_usdc: None,
        raw_cost_usdc: None,
        taker_fee_usdc: None,
        execution_buffer_usdc: None,
        total_cost_usdc: None,
        net_profit_usdc: None,
        net_edge: None,
        qualifies: false,
        rejection_reason: Some(last_failure),
        legs: Vec::new(),
        size_evaluations: Vec::new(),
    });
    result.size_evaluations = size_evaluations;
    result
}

fn walk_asks(book: &BookSnapshot, shares: Decimal, fee: &FeePolicy) -> Option<FillQuote> {
    let mut remaining = shares;
    let mut raw_cost = Decimal::ZERO;
    let mut fee_cost = Decimal::ZERO;
    let mut levels = book.asks.iter().collect::<Vec<_>>();
    levels.sort_by_key(|level| level.price);
    for level in levels {
        if remaining <= Decimal::ZERO {
            break;
        }
        if level.price <= Decimal::ZERO || level.price >= dec!(1) || level.size <= Decimal::ZERO {
            continue;
        }
        let filled = remaining.min(level.size);
        raw_cost += filled * level.price;
        if fee.enabled {
            fee_cost += filled * fee_per_share(fee.rate, fee.exponent, level.price);
        }
        remaining -= filled;
    }
    if remaining > Decimal::ZERO || shares <= Decimal::ZERO {
        return None;
    }
    Some(FillQuote {
        average_price: raw_cost / shares,
        raw_cost,
        fee: fee_cost,
    })
}

pub fn fee_per_share(rate: Decimal, exponent: u32, price: Decimal) -> Decimal {
    if rate <= Decimal::ZERO || price <= Decimal::ZERO || price >= dec!(1) {
        return Decimal::ZERO;
    }
    let base = price * (dec!(1) - price);
    let factor = (0..exponent).fold(dec!(1), |value, _| value * base);
    rate * factor
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leg(condition: &str, token: &str, outcome: Outcome) -> RelationLeg {
        RelationLeg {
            condition_id: condition.into(),
            question: condition.into(),
            outcome,
            token_id: token.into(),
            fee: FeePolicy {
                enabled: false,
                rate: Decimal::ZERO,
                exponent: 1,
            },
        }
    }

    fn book(token: &str, ask: Decimal, size: Decimal) -> BookSnapshot {
        BookSnapshot {
            token_id: token.into(),
            observed_at: Utc::now(),
            exchange_timestamp_ms: None,
            asks: vec![BookLevel { price: ask, size }],
        }
    }

    #[test]
    fn implication_bundle_detects_guaranteed_profit() {
        let relation = MarketRelationship {
            relationship_id: "r1".into(),
            event_id: "e1".into(),
            event_slug: "deadline".into(),
            event_title: "Deadline".into(),
            kind: RelationshipKind::DeadlineImplication,
            semantic_status: "template_inferred".into(),
            evidence: vec![],
            minimum_payout_per_share: dec!(1),
            live_eligible: false,
            legs: vec![
                leg("broad", "yes-broad", Outcome::Yes),
                leg("narrow", "no-narrow", Outcome::No),
            ],
        };
        let books = HashMap::from([
            ("yes-broad".into(), book("yes-broad", dec!(0.55), dec!(100))),
            ("no-narrow".into(), book("no-narrow", dec!(0.40), dec!(100))),
        ]);
        let result = evaluate_relationship(
            &relation,
            &books,
            &[dec!(10)],
            dec!(1),
            dec!(0.01),
            dec!(0.002),
            Utc::now(),
        );
        assert!(result.qualifies);
        assert_eq!(result.net_profit_usdc, Some(dec!(0.48)));
    }

    #[test]
    fn fees_can_remove_nominal_edge() {
        let mut fee_leg = leg("a", "a", Outcome::Yes);
        fee_leg.fee = FeePolicy {
            enabled: true,
            rate: dec!(0.07),
            exponent: 1,
        };
        let relation = MarketRelationship {
            relationship_id: "r".into(),
            event_id: "e".into(),
            event_slug: "e".into(),
            event_title: "e".into(),
            kind: RelationshipKind::DeadlineImplication,
            semantic_status: "test".into(),
            evidence: vec![],
            minimum_payout_per_share: dec!(1),
            live_eligible: false,
            legs: vec![
                fee_leg.clone(),
                RelationLeg {
                    token_id: "b".into(),
                    ..fee_leg
                },
            ],
        };
        let books = HashMap::from([
            ("a".into(), book("a", dec!(0.49), dec!(10))),
            ("b".into(), book("b", dec!(0.49), dec!(10))),
        ]);
        let result = evaluate_relationship(
            &relation,
            &books,
            &[dec!(10)],
            dec!(1),
            dec!(0),
            dec!(0),
            Utc::now(),
        );
        assert!(!result.qualifies);
        assert!(result.net_profit_usdc.unwrap() < Decimal::ZERO);
    }

    #[test]
    fn incomplete_depth_fails_closed() {
        let relation = MarketRelationship {
            relationship_id: "r".into(),
            event_id: "e".into(),
            event_slug: "e".into(),
            event_title: "e".into(),
            kind: RelationshipKind::DeadlineImplication,
            semantic_status: "test".into(),
            evidence: vec![],
            minimum_payout_per_share: dec!(1),
            live_eligible: false,
            legs: vec![leg("a", "a", Outcome::Yes), leg("b", "b", Outcome::No)],
        };
        let books = HashMap::from([("a".into(), book("a", dec!(0.4), dec!(10)))]);
        let result = evaluate_relationship(
            &relation,
            &books,
            &[dec!(5)],
            dec!(1),
            dec!(0),
            dec!(0),
            Utc::now(),
        );
        assert!(!result.qualifies);
        assert!(result
            .rejection_reason
            .unwrap()
            .starts_with("missing_book:"));
    }

    #[test]
    fn records_every_configured_size_in_research_curve() {
        let relation = MarketRelationship {
            relationship_id: "curve".into(),
            event_id: "e".into(),
            event_slug: "e".into(),
            event_title: "e".into(),
            kind: RelationshipKind::DeadlineImplication,
            semantic_status: "test".into(),
            evidence: vec![],
            minimum_payout_per_share: dec!(1),
            live_eligible: false,
            legs: vec![leg("a", "a", Outcome::Yes), leg("b", "b", Outcome::No)],
        };
        let books = HashMap::from([
            ("a".into(), book("a", dec!(0.45), dec!(10))),
            ("b".into(), book("b", dec!(0.45), dec!(10))),
        ]);
        let result = evaluate_relationship(
            &relation,
            &books,
            &[dec!(1), dec!(5), dec!(20)],
            dec!(0.5),
            dec!(0.01),
            dec!(0.003),
            Utc::now(),
        );
        assert_eq!(result.size_evaluations.len(), 3);
        assert!(result.size_evaluations[0].qualifies);
        assert!(result.size_evaluations[1].qualifies);
        assert!(result.size_evaluations[2]
            .rejection_reason
            .as_deref()
            .is_some_and(|reason| reason.starts_with("insufficient_ask_depth:")));
    }

    #[test]
    fn execution_buffer_curve_is_recorded_without_changing_primary_decision() {
        let relation = MarketRelationship {
            relationship_id: "buffer-curve".into(),
            event_id: "e".into(),
            event_slug: "e".into(),
            event_title: "e".into(),
            kind: RelationshipKind::DeadlineImplication,
            semantic_status: "test".into(),
            evidence: vec![],
            minimum_payout_per_share: dec!(1),
            live_eligible: false,
            legs: vec![leg("a", "a", Outcome::Yes), leg("b", "b", Outcome::No)],
        };
        let books = HashMap::from([
            ("a".into(), book("a", dec!(0.48), dec!(10))),
            ("b".into(), book("b", dec!(0.48), dec!(10))),
        ]);
        let result = evaluate_relationship_with_buffers(
            &relation,
            &books,
            &[dec!(10)],
            dec!(1),
            dec!(0.01),
            dec!(0.003),
            &[dec!(0), dec!(0.003), dec!(0.01)],
            Utc::now(),
        );
        let curve = &result.size_evaluations[0].buffer_evaluations;
        assert_eq!(curve.len(), 3);
        assert!(curve[0].net_edge > curve[1].net_edge);
        assert!(curve[1].net_edge > curve[2].net_edge);
        assert_eq!(result.net_edge, Some(curve[1].net_edge));
        assert!(result.qualifies);
    }
}
