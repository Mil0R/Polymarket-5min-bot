use super::discovery::{lexical_containment, meaningful_terms, DiscoveredContract, DiscoveryEvent};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NumericInterval {
    pub condition_id: String,
    pub label: String,
    pub minimum: Option<i64>,
    pub maximum: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BoundaryDiagnostic {
    pub diagnostic_id: String,
    pub parent: DiscoveredContract,
    pub partition_event_id: String,
    pub partition_event_title: String,
    pub lexical_score: f64,
    pub inferred_parent_boundary: Option<String>,
    pub intervals: Vec<NumericInterval>,
    pub selected_child_condition_ids: Vec<String>,
    pub straddling_condition_ids: Vec<String>,
    pub numeric_blockers: Vec<String>,
    pub review_warnings: Vec<String>,
    pub numerically_representable: bool,
    pub promotable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BoundaryReport {
    pub version: u32,
    pub generated_at: DateTime<Utc>,
    pub scope: String,
    pub events_seen: usize,
    pub numeric_partition_events: usize,
    pub diagnostics: Vec<BoundaryDiagnostic>,
}

#[derive(Debug, Clone, Copy)]
enum ParentBoundary {
    GreaterEqual(i64),
    LessEqual(i64),
}

pub(crate) fn analyze_boundaries(
    events: &[DiscoveryEvent],
    max_diagnostics: usize,
) -> BoundaryReport {
    let all_markets = events
        .iter()
        .flat_map(|event| event.markets.iter())
        .collect::<Vec<_>>();
    let partitions = events
        .iter()
        .filter_map(|event| numeric_partition(event).map(|intervals| (event, intervals)))
        .collect::<Vec<_>>();
    let mut diagnostics = Vec::new();
    for (event, intervals) in &partitions {
        let event_terms = meaningful_terms(&event.title);
        for parent in &all_markets {
            if parent.event_id == event.id
                || parse_interval(&parent.group_item_title).is_some()
                || incompatible_subjects(&event.title, &parent.question)
            {
                continue;
            }
            let parent_terms = meaningful_terms(&parent.question);
            let common = event_terms.intersection(&parent_terms).count();
            let score = lexical_containment(&event_terms, &parent_terms);
            if common < 2 || score < 0.5 {
                continue;
            }
            if let Some(diagnostic) = diagnose(parent, event, intervals.clone(), score) {
                diagnostics.push(diagnostic);
            }
        }
    }
    diagnostics.sort_by(|left, right| {
        right
            .numerically_representable
            .cmp(&left.numerically_representable)
            .then_with(|| right.lexical_score.total_cmp(&left.lexical_score))
            .then_with(|| left.diagnostic_id.cmp(&right.diagnostic_id))
    });
    diagnostics.truncate(max_diagnostics);
    BoundaryReport {
        version: 1,
        generated_at: Utc::now(),
        scope: "active_boundary_diagnostics_research_only".into(),
        events_seen: events.len(),
        numeric_partition_events: partitions.len(),
        diagnostics,
    }
}

fn incompatible_subjects(left: &str, right: &str) -> bool {
    let left = format!(" {} ", left.to_ascii_lowercase());
    let right = format!(" {} ", right.to_ascii_lowercase());
    let exclusive_pairs = [
        (" senate ", " house "),
        (" house ", " governorship"),
        (" democratic ", " republican "),
        (" israel ", " united states "),
        (" israel ", " us "),
        (" israel ", " u.s. "),
        (" russia ", " ukraine "),
    ];
    exclusive_pairs.iter().any(|(first, second)| {
        left.contains(first) && right.contains(second)
            || left.contains(second) && right.contains(first)
    })
}

fn diagnose(
    parent: &DiscoveredContract,
    event: &DiscoveryEvent,
    intervals: Vec<NumericInterval>,
    lexical_score: f64,
) -> Option<BoundaryDiagnostic> {
    let mut numeric_blockers = partition_blockers(&intervals);
    let mut review_warnings = vec![
        "Numeric alignment does not prove identical resolution rules or parent/child semantics."
            .into(),
        "Cancellation, ties, vacancies, Other outcomes and clarification clauses require review."
            .into(),
    ];
    let (boundary, boundary_warning) = infer_parent_boundary(parent);
    if boundary.is_none() && boundary_warning.is_none() {
        return None;
    }
    if let Some(warning) = boundary_warning {
        review_warnings.push(warning);
    }
    let mut selected = Vec::new();
    let mut straddling = Vec::new();
    if let Some(boundary) = boundary {
        for interval in &intervals {
            match relation_to_boundary(interval, boundary) {
                IntervalRelation::Inside => selected.push(interval.condition_id.clone()),
                IntervalRelation::Straddles => straddling.push(interval.condition_id.clone()),
                IntervalRelation::Outside => {}
            }
        }
        if !straddling.is_empty() {
            numeric_blockers.push("parent_boundary_splits_child_interval".into());
        }
        if selected.len() < 2 {
            numeric_blockers.push("fewer_than_two_whole_child_contracts_match_parent".into());
        }
    } else {
        numeric_blockers.push("parent_numeric_boundary_not_provable".into());
    }
    numeric_blockers.sort();
    numeric_blockers.dedup();
    let numerically_representable = numeric_blockers.is_empty();
    Some(BoundaryDiagnostic {
        diagnostic_id: format!("boundary:{}:{}", parent.condition_id, event.id),
        parent: parent.clone(),
        partition_event_id: event.id.clone(),
        partition_event_title: event.title.clone(),
        lexical_score,
        inferred_parent_boundary: boundary.map(format_boundary),
        intervals,
        selected_child_condition_ids: selected,
        straddling_condition_ids: straddling,
        numeric_blockers,
        review_warnings,
        numerically_representable,
        promotable: false,
    })
}

fn numeric_partition(event: &DiscoveryEvent) -> Option<Vec<NumericInterval>> {
    if event.markets.len() < 2 {
        return None;
    }
    let mut intervals = event
        .markets
        .iter()
        .map(|market| {
            let (minimum, maximum) = parse_interval(&market.group_item_title)?;
            Some(NumericInterval {
                condition_id: market.condition_id.clone(),
                label: market.group_item_title.clone(),
                minimum,
                maximum,
            })
        })
        .collect::<Option<Vec<_>>>()?;
    intervals.sort_by_key(|interval| (interval.minimum.is_some(), interval.minimum));
    Some(intervals)
}

fn partition_blockers(intervals: &[NumericInterval]) -> Vec<String> {
    let mut blockers = Vec::new();
    if intervals
        .first()
        .is_none_or(|interval| interval.minimum.is_some())
    {
        blockers.push("partition_has_no_lower_tail".into());
    }
    if intervals
        .last()
        .is_none_or(|interval| interval.maximum.is_some())
    {
        blockers.push("partition_has_no_upper_tail".into());
    }
    for pair in intervals.windows(2) {
        match (pair[0].maximum, pair[1].minimum) {
            (Some(left), Some(right)) if right > left.saturating_add(1) => {
                blockers.push("partition_has_numeric_gap".into());
            }
            (Some(left), Some(right)) if right <= left => {
                blockers.push("partition_has_numeric_overlap".into());
            }
            (None, _) | (_, None) => blockers.push("partition_has_unordered_open_tail".into()),
            _ => {}
        }
    }
    blockers
}

fn parse_interval(value: &str) -> Option<(Option<i64>, Option<i64>)> {
    let lower = value.trim().to_ascii_lowercase();
    let numbers = extract_numbers(&lower);
    if (lower.starts_with("below ")
        || lower.starts_with("under ")
        || lower.starts_with("less than "))
        && numbers.len() == 1
    {
        return Some((None, Some(numbers[0].saturating_sub(1))));
    }
    if (lower.ends_with('+') || lower.contains(" or more") || lower.starts_with("at least "))
        && numbers.len() == 1
    {
        return Some((Some(numbers[0]), None));
    }
    if numbers.len() == 2 && (lower.contains('-') || lower.contains(" to ")) {
        return Some((Some(numbers[0]), Some(numbers[1])));
    }
    if numbers.len() == 1 && lower.chars().all(|character| character.is_ascii_digit()) {
        return Some((Some(numbers[0]), Some(numbers[0])));
    }
    None
}

fn infer_parent_boundary(parent: &DiscoveredContract) -> (Option<ParentBoundary>, Option<String>) {
    let lower = parent.question.to_ascii_lowercase();
    if let Some(value) = number_after_phrase(&lower, "at least ")
        .or_else(|| number_before_phrase(&lower, " or more"))
    {
        return (Some(ParentBoundary::GreaterEqual(value)), None);
    }
    if let Some(value) =
        number_after_phrase(&lower, "more than ").or_else(|| number_after_phrase(&lower, "over "))
    {
        return (
            Some(ParentBoundary::GreaterEqual(value.saturating_add(1))),
            None,
        );
    }
    if let Some(value) = number_after_phrase(&lower, "at most ")
        .or_else(|| number_before_phrase(&lower, " or fewer"))
    {
        return (Some(ParentBoundary::LessEqual(value)), None);
    }
    if let Some(value) = number_after_phrase(&lower, "below ")
        .or_else(|| number_after_phrase(&lower, "less than "))
        .or_else(|| number_after_phrase(&lower, "under "))
    {
        return (
            Some(ParentBoundary::LessEqual(value.saturating_sub(1))),
            None,
        );
    }
    let rules = parent.description.to_ascii_lowercase();
    if lower.contains("control the house") && rules.contains("majority") {
        return (
            Some(ParentBoundary::GreaterEqual(218)),
            Some(
                "House 218-seat boundary is inferred and must be confirmed against full rules."
                    .into(),
            ),
        );
    }
    if lower.contains("control the senate") && rules.contains("vice presidency") {
        return (
            None,
            Some("Senate control at 50 seats depends on the Vice Presidency; seat count alone is not a partition.".into()),
        );
    }
    (None, None)
}

fn number_after_phrase(value: &str, phrase: &str) -> Option<i64> {
    let remainder = value.split_once(phrase)?.1.trim_start();
    let number = remainder
        .trim_start_matches(['$', '€', '£'])
        .split(|character: char| !character.is_ascii_digit())
        .next()?;
    (!number.is_empty()).then(|| number.parse().ok()).flatten()
}

fn number_before_phrase(value: &str, phrase: &str) -> Option<i64> {
    let prefix = value.split_once(phrase)?.0;
    prefix
        .split(|character: char| !character.is_ascii_digit())
        .rfind(|part| !part.is_empty())?
        .parse()
        .ok()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IntervalRelation {
    Inside,
    Outside,
    Straddles,
}

fn relation_to_boundary(interval: &NumericInterval, boundary: ParentBoundary) -> IntervalRelation {
    match boundary {
        ParentBoundary::GreaterEqual(threshold) => {
            if interval.maximum.is_some_and(|maximum| maximum < threshold) {
                IntervalRelation::Outside
            } else if interval.minimum.is_some_and(|minimum| minimum >= threshold) {
                IntervalRelation::Inside
            } else {
                IntervalRelation::Straddles
            }
        }
        ParentBoundary::LessEqual(threshold) => {
            if interval.minimum.is_some_and(|minimum| minimum > threshold) {
                IntervalRelation::Outside
            } else if interval.maximum.is_some_and(|maximum| maximum <= threshold) {
                IntervalRelation::Inside
            } else {
                IntervalRelation::Straddles
            }
        }
    }
}

fn format_boundary(boundary: ParentBoundary) -> String {
    match boundary {
        ParentBoundary::GreaterEqual(value) => format!(">={value}"),
        ParentBoundary::LessEqual(value) => format!("<={value}"),
    }
}

fn extract_numbers(value: &str) -> Vec<i64> {
    value
        .split(|character: char| !character.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.parse().ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn interval(label: &str) -> (Option<i64>, Option<i64>) {
        parse_interval(label).unwrap()
    }

    #[test]
    fn parses_integer_partition_labels() {
        assert_eq!(interval("Below 190"), (None, Some(189)));
        assert_eq!(interval("190-194"), (Some(190), Some(194)));
        assert_eq!(interval("230+"), (Some(230), None));
        assert_eq!(interval("48"), (Some(48), Some(48)));
    }

    #[test]
    fn detects_house_control_boundary_splitting_a_bucket() {
        let intervals = vec![NumericInterval {
            condition_id: "bucket".into(),
            label: "215-219".into(),
            minimum: Some(215),
            maximum: Some(219),
        }];
        assert_eq!(
            relation_to_boundary(&intervals[0], ParentBoundary::GreaterEqual(218)),
            IntervalRelation::Straddles
        );
    }

    #[test]
    fn detects_gap_in_apparent_partition() {
        let intervals = vec![
            NumericInterval {
                condition_id: "low".into(),
                label: "Below 10".into(),
                minimum: None,
                maximum: Some(9),
            },
            NumericInterval {
                condition_id: "high".into(),
                label: "12+".into(),
                minimum: Some(12),
                maximum: None,
            },
        ];
        assert!(partition_blockers(&intervals).contains(&"partition_has_numeric_gap".into()));
    }

    #[test]
    fn rejects_cross_chamber_and_cross_country_false_matches() {
        assert!(incompatible_subjects(
            "Republican House seats",
            "Republican Senate control"
        ));
        assert!(incompatible_subjects(
            "How many countries will Israel strike?",
            "Will the US strike 10 countries?"
        ));
        assert!(!incompatible_subjects(
            "Republican House seats",
            "Will Republicans control the House?"
        ));
    }

    #[test]
    fn comparison_parser_never_uses_an_unrelated_year() {
        assert_eq!(
            number_after_phrase("more than 16 launches in 2026", "more than "),
            Some(16)
        );
        assert_eq!(
            number_after_phrase("more than fifteen launches in 2026", "more than "),
            None
        );
        assert_eq!(
            number_before_phrase("57 or more seats in 2026", " or more"),
            Some(57)
        );
    }
}
