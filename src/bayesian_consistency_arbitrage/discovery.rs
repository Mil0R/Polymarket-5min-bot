use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashSet};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DiscoveredContract {
    #[serde(default)]
    pub market_id: String,
    pub event_id: String,
    pub event_title: String,
    pub event_slug: String,
    #[serde(default)]
    pub group_item_title: String,
    #[serde(default)]
    pub end_date: String,
    pub condition_id: String,
    pub question: String,
    pub yes_token_id: String,
    pub no_token_id: String,
    pub description: String,
    pub resolution_source: String,
    pub rules_hash: String,
    #[serde(default)]
    pub tradable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DiscoveryCandidate {
    pub candidate_id: String,
    pub status: String,
    #[serde(default = "default_discovery_source")]
    pub discovery_source: String,
    pub lexical_score: f64,
    pub parent: DiscoveredContract,
    pub children: Vec<DiscoveredContract>,
    pub evidence: Vec<String>,
    pub required_reviews: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DiscoveryReport {
    pub version: u32,
    #[serde(default = "default_discovery_scope")]
    pub scope: String,
    pub generated_at: chrono::DateTime<chrono::Utc>,
    pub events_seen: usize,
    pub tradable_markets_seen: usize,
    #[serde(default)]
    pub seed_errors: Vec<String>,
    pub candidates: Vec<DiscoveryCandidate>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CandidateSeedFile {
    pub version: u32,
    #[serde(default)]
    pub seeds: Vec<CandidateSeed>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CandidateSeed {
    pub seed_id: String,
    pub title: String,
    pub parent_condition_id: String,
    pub child_condition_ids: Vec<String>,
    #[serde(default)]
    pub notes: Vec<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct DiscoveryEvent {
    pub id: String,
    pub title: String,
    pub markets: Vec<DiscoveredContract>,
}

pub(crate) fn discover_partition_candidates(
    events: &[DiscoveryEvent],
    max_children: usize,
    max_candidates: usize,
) -> Vec<DiscoveryCandidate> {
    let all_markets = events
        .iter()
        .flat_map(|event| event.markets.iter())
        .collect::<Vec<_>>();
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();

    for event in events {
        let children = event
            .markets
            .iter()
            .filter(|market| has_partition_cue(&market.question))
            .cloned()
            .collect::<Vec<_>>();
        if children.len() < 2
            || children.len() > max_children
            || children.len() != event.markets.len()
        {
            continue;
        }
        let event_terms = meaningful_terms(&event.title);
        for parent in &all_markets {
            if children
                .iter()
                .any(|child| child.condition_id == parent.condition_id)
                || parent.event_id == event.id
                || !is_broad_parent(&parent.question)
            {
                continue;
            }
            let parent_terms = meaningful_terms(&parent.question);
            let score = lexical_containment(&event_terms, &parent_terms);
            let common = event_terms.intersection(&parent_terms).count();
            if score < 0.75 || common < 2 {
                continue;
            }
            let candidate_id = format!("partition:{}:{}", parent.condition_id, event.id);
            if !seen.insert(candidate_id.clone()) {
                continue;
            }
            candidates.push(DiscoveryCandidate {
                candidate_id,
                status: "pending".into(),
                discovery_source: "lexical_active_events".into(),
                lexical_score: score,
                parent: (*parent).clone(),
                children: children.clone(),
                evidence: vec![
                    "Parent question and child event title share a deterministic lexical anchor."
                        .into(),
                    "At least two child questions contain range/margin partition cues.".into(),
                ],
                required_reviews: vec![
                    "Prove children are mutually exclusive.".into(),
                    "Prove children exhaust every state in which the parent resolves YES.".into(),
                    "Review Other, tie, cancellation, invalid and clarification clauses.".into(),
                    "Compare resolution sources and save the reviewed rule text externally.".into(),
                    "Replace pending with a manually authored registry entry; discovery never approves it."
                        .into(),
                ],
            });
        }
    }
    candidates.sort_by(|left, right| {
        right
            .lexical_score
            .total_cmp(&left.lexical_score)
            .then_with(|| left.candidate_id.cmp(&right.candidate_id))
    });
    candidates.truncate(max_candidates);
    candidates
}

pub fn load_seed_file(path: &Path, max_children: usize) -> anyhow::Result<CandidateSeedFile> {
    let bytes = std::fs::read(path)
        .map_err(|error| anyhow::anyhow!("failed to read seed file {}: {error}", path.display()))?;
    let seeds: CandidateSeedFile = serde_json::from_slice(&bytes)
        .map_err(|error| anyhow::anyhow!("invalid seed file {}: {error}", path.display()))?;
    if seeds.version != 1 {
        anyhow::bail!("unsupported candidate seed version {}", seeds.version);
    }
    validate_seeds(&seeds, max_children)?;
    Ok(seeds)
}

fn validate_seeds(seeds: &CandidateSeedFile, max_children: usize) -> anyhow::Result<()> {
    let mut seed_ids = HashSet::new();
    for seed in &seeds.seeds {
        if seed.seed_id.trim().is_empty() || seed.title.trim().is_empty() {
            anyhow::bail!("candidate seed id and title cannot be empty");
        }
        if !seed_ids.insert(seed.seed_id.as_str()) {
            anyhow::bail!("duplicate candidate seed id {}", seed.seed_id);
        }
        if !valid_condition_id(&seed.parent_condition_id) {
            anyhow::bail!("seed {} has an invalid parent condition ID", seed.seed_id);
        }
        if seed.child_condition_ids.len() < 2 || seed.child_condition_ids.len() > max_children {
            anyhow::bail!(
                "seed {} must contain 2..={max_children} child condition IDs",
                seed.seed_id
            );
        }
        let mut conditions = HashSet::from([seed.parent_condition_id.as_str()]);
        if seed
            .child_condition_ids
            .iter()
            .any(|condition| !valid_condition_id(condition) || !conditions.insert(condition))
        {
            anyhow::bail!(
                "seed {} contains invalid or duplicate condition IDs",
                seed.seed_id
            );
        }
    }
    Ok(())
}

fn valid_condition_id(value: &str) -> bool {
    value.len() == 66
        && value.starts_with("0x")
        && value[2..]
            .chars()
            .all(|character| character.is_ascii_hexdigit())
}

fn default_discovery_source() -> String {
    "legacy".into()
}

fn default_discovery_scope() -> String {
    "active".into()
}

pub(crate) fn rules_hash(question: &str, description: &str, resolution_source: &str) -> String {
    let canonical = format!(
        "question={}\ndescription={}\nresolution_source={}",
        question.trim(),
        description.trim(),
        resolution_source.trim()
    );
    format!("sha256:{:x}", Sha256::digest(canonical.as_bytes()))
}

fn has_partition_cue(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    [
        " margin",
        " by ",
        "between ",
        "less than ",
        "more than ",
        "at least ",
        "at most ",
        "exactly ",
        "vote share",
        "percentage",
        " seats",
        "%",
    ]
    .iter()
    .any(|cue| lower.contains(cue))
}

fn is_broad_parent(value: &str) -> bool {
    let lower = format!(" {} ", value.to_ascii_lowercase());
    !has_partition_cue(value)
        && !lower.chars().any(|character| character.is_ascii_digit())
        && ![
            " no ",
            " not ",
            " before ",
            " after ",
            " above ",
            " below ",
            " under ",
            " over ",
            " fewer ",
            " greater ",
            "$",
        ]
        .iter()
        .any(|cue| lower.contains(cue))
}

pub(crate) fn meaningful_terms(value: &str) -> BTreeSet<String> {
    const STOP: &[&str] = &[
        "a",
        "an",
        "and",
        "be",
        "by",
        "for",
        "in",
        "is",
        "of",
        "on",
        "or",
        "the",
        "to",
        "will",
        "win",
        "wins",
        "winner",
        "market",
        "election",
        "presidential",
    ];
    value
        .to_ascii_lowercase()
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|term| {
            term.len() >= 3
                && !term.chars().all(|character| character.is_ascii_digit())
                && !STOP.contains(term)
        })
        .map(str::to_string)
        .collect()
}

pub(crate) fn lexical_containment(left: &BTreeSet<String>, right: &BTreeSet<String>) -> f64 {
    let denominator = left.len().min(right.len());
    if denominator == 0 {
        return 0.0;
    }
    left.intersection(right).count() as f64 / denominator as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contract(event: &str, condition: &str, question: &str) -> DiscoveredContract {
        DiscoveredContract {
            market_id: condition.into(),
            event_id: event.into(),
            event_title: String::new(),
            event_slug: String::new(),
            group_item_title: String::new(),
            end_date: String::new(),
            condition_id: condition.into(),
            question: question.into(),
            yes_token_id: format!("yes-{condition}"),
            no_token_id: format!("no-{condition}"),
            description: "Rules".into(),
            resolution_source: "UMA".into(),
            rules_hash: rules_hash(question, "Rules", "UMA"),
            tradable: true,
        }
    }

    #[test]
    fn range_children_generate_pending_candidate_but_never_approval() {
        let events = vec![
            DiscoveryEvent {
                id: "parent-event".into(),
                title: "Democratic party election".into(),
                markets: vec![contract(
                    "parent-event",
                    "parent",
                    "Will the Democratic party win the election?",
                )],
            },
            DiscoveryEvent {
                id: "margin-event".into(),
                title: "Democratic party winning margin".into(),
                markets: vec![
                    contract(
                        "margin-event",
                        "child-a",
                        "Democratic margin between 0 and 2%",
                    ),
                    contract("margin-event", "child-b", "Democratic margin more than 2%"),
                ],
            },
        ];
        let candidates = discover_partition_candidates(&events, 12, 200);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].status, "pending");
        assert_eq!(candidates[0].parent.condition_id, "parent");
    }

    #[test]
    fn rules_hash_detects_rule_text_changes() {
        assert_ne!(
            rules_hash("Question", "Rule A", "UMA"),
            rules_hash("Question", "Rule B", "UMA")
        );
    }

    #[test]
    fn sibling_outcomes_without_partition_cues_are_not_inferred() {
        let events = vec![DiscoveryEvent {
            id: "event".into(),
            title: "Election winner".into(),
            markets: vec![
                contract("event", "a", "Will Alice win?"),
                contract("event", "b", "Will Bob win?"),
            ],
        }];
        assert!(discover_partition_candidates(&events, 12, 200).is_empty());
    }

    #[test]
    fn condition_seed_rejects_duplicate_or_malformed_ids() {
        let valid = |character: char| format!("0x{}", character.to_string().repeat(64));
        let seeds = CandidateSeedFile {
            version: 1,
            seeds: vec![CandidateSeed {
                seed_id: "seed".into(),
                title: "Candidate".into(),
                parent_condition_id: valid('a'),
                child_condition_ids: vec![valid('b'), valid('c')],
                notes: vec![],
            }],
        };
        assert!(validate_seeds(&seeds, 12).is_ok());
        let mut duplicate = seeds.clone();
        duplicate.seeds[0].child_condition_ids[1] = valid('b');
        assert!(validate_seeds(&duplicate, 12).is_err());
        let mut malformed = seeds;
        malformed.seeds[0].parent_condition_id = "not-a-condition".into();
        assert!(validate_seeds(&malformed, 12).is_err());
    }
}
