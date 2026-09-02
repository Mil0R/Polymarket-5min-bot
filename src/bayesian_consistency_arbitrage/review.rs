use super::discovery::{DiscoveryCandidate, DiscoveryReport};
use super::model::{PartitionContract, PartitionRelationship, RelationshipReviewStatus};
use super::registry::load_registry;
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::fs::File;
use std::io::Write;
use std::path::Path;

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CandidateReviewDecision {
    #[default]
    Pending,
    Approved,
    Rejected,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CandidateReview {
    pub candidate_id: String,
    pub candidate_digest: String,
    #[serde(default)]
    pub decision: CandidateReviewDecision,
    #[serde(default)]
    pub reviewer: String,
    #[serde(default)]
    pub reviewed_at: String,
    #[serde(default)]
    pub mutual_exclusivity_proof: String,
    #[serde(default)]
    pub exhaustiveness_proof: String,
    #[serde(default)]
    pub omitted_states_review: String,
    #[serde(default)]
    pub resolution_alignment_proof: String,
    #[serde(default)]
    pub assumed_fee_rate: Option<Decimal>,
    #[serde(default)]
    pub assumed_fee_exponent: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReviewFile {
    pub version: u32,
    pub source_generated_at: DateTime<Utc>,
    pub generated_at: DateTime<Utc>,
    pub reviews: Vec<CandidateReview>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PromotionReport {
    pub reviewed: usize,
    pub approved: usize,
    pub rejected: usize,
    pub pending: usize,
    pub promoted: usize,
    pub registry_total: usize,
}

pub fn load_discovery(path: &Path) -> Result<DiscoveryReport> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("failed to read discovery report {}", path.display()))?;
    let report: DiscoveryReport = serde_json::from_slice(&bytes)
        .with_context(|| format!("invalid discovery report {}", path.display()))?;
    if report.version != 1 {
        bail!("unsupported discovery report version {}", report.version);
    }
    Ok(report)
}

pub fn load_review(path: &Path) -> Result<ReviewFile> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("failed to read review file {}", path.display()))?;
    let review: ReviewFile = serde_json::from_slice(&bytes)
        .with_context(|| format!("invalid review file {}", path.display()))?;
    if review.version != 1 {
        bail!("unsupported review file version {}", review.version);
    }
    Ok(review)
}

pub fn candidate_digest(candidate: &DiscoveryCandidate) -> Result<String> {
    let bytes = serde_json::to_vec(candidate)?;
    Ok(format!("sha256:{:x}", Sha256::digest(bytes)))
}

pub fn prepare_review(discovery_path: &Path, review_path: &Path) -> Result<ReviewFile> {
    let discovery = load_discovery(discovery_path)?;
    let previous = if review_path.exists() {
        load_review(review_path)?.reviews
    } else {
        Vec::new()
    };
    let previous = previous
        .into_iter()
        .map(|review| (review.candidate_id.clone(), review))
        .collect::<BTreeMap<_, _>>();
    let mut reviews = Vec::with_capacity(discovery.candidates.len());
    for candidate in &discovery.candidates {
        let digest = candidate_digest(candidate)?;
        let review = previous
            .get(&candidate.candidate_id)
            .filter(|review| review.candidate_digest == digest)
            .cloned()
            .unwrap_or_else(|| blank_review(candidate.candidate_id.clone(), digest));
        reviews.push(review);
    }
    let file = ReviewFile {
        version: 1,
        source_generated_at: discovery.generated_at,
        generated_at: Utc::now(),
        reviews,
    };
    write_json_atomic(review_path, &file)?;
    Ok(file)
}

pub fn promote_reviewed(
    discovery_path: &Path,
    review_path: &Path,
    registry_path: &Path,
    max_children: usize,
) -> Result<PromotionReport> {
    let discovery = load_discovery(discovery_path)?;
    if discovery.scope != "active_tradable" {
        bail!(
            "discovery scope '{}' is research-only; only active_tradable can be promoted",
            discovery.scope
        );
    }
    let review_file = load_review(review_path)?;
    if review_file.source_generated_at != discovery.generated_at {
        bail!("review file refers to an obsolete discovery run; run prepare-review again");
    }
    let candidates = discovery
        .candidates
        .iter()
        .map(|candidate| (candidate.candidate_id.as_str(), candidate))
        .collect::<BTreeMap<_, _>>();
    let mut review_ids = HashSet::new();
    let mut promoted = Vec::new();
    let mut approved = 0;
    let mut rejected = 0;
    let mut pending = 0;
    for review in &review_file.reviews {
        if !review_ids.insert(review.candidate_id.as_str()) {
            bail!("duplicate candidate review {}", review.candidate_id);
        }
        match review.decision {
            CandidateReviewDecision::Pending => pending += 1,
            CandidateReviewDecision::Rejected => rejected += 1,
            CandidateReviewDecision::Approved => {
                approved += 1;
                let candidate =
                    candidates
                        .get(review.candidate_id.as_str())
                        .with_context(|| {
                            format!(
                                "approved candidate {} is absent from discovery",
                                review.candidate_id
                            )
                        })?;
                promoted.push(review_to_relationship(candidate, review, max_children)?);
            }
        }
    }
    if promoted.is_empty() {
        bail!("no fully approved candidate is eligible for promotion");
    }

    let mut registry = load_registry(registry_path)?;
    let existing = registry
        .relationships
        .iter()
        .map(|relationship| relationship.relationship_id.as_str())
        .collect::<HashSet<_>>();
    for relationship in &promoted {
        if existing.contains(relationship.relationship_id.as_str()) {
            bail!(
                "registry already contains relationship {}; promotion will not overwrite it",
                relationship.relationship_id
            );
        }
    }
    let promoted_count = promoted.len();
    registry.relationships.extend(promoted);
    write_json_atomic(registry_path, &registry)?;
    Ok(PromotionReport {
        reviewed: review_file.reviews.len(),
        approved,
        rejected,
        pending,
        promoted: promoted_count,
        registry_total: registry.relationships.len(),
    })
}

fn review_to_relationship(
    candidate: &DiscoveryCandidate,
    review: &CandidateReview,
    max_children: usize,
) -> Result<PartitionRelationship> {
    if candidate.status != "pending" {
        bail!("discovery candidate status must remain pending");
    }
    if candidate.children.len() < 2 || candidate.children.len() > max_children {
        bail!(
            "candidate {} must contain 2..={max_children} children",
            candidate.candidate_id
        );
    }
    if !candidate.parent.tradable || candidate.children.iter().any(|child| !child.tradable) {
        bail!("historical or non-tradable candidates cannot be promoted");
    }
    if candidate_digest(candidate)? != review.candidate_digest {
        bail!("candidate {} digest mismatch", candidate.candidate_id);
    }
    for (label, value) in [
        ("reviewer", review.reviewer.as_str()),
        ("reviewed_at", review.reviewed_at.as_str()),
        (
            "mutual_exclusivity_proof",
            review.mutual_exclusivity_proof.as_str(),
        ),
        ("exhaustiveness_proof", review.exhaustiveness_proof.as_str()),
        (
            "omitted_states_review",
            review.omitted_states_review.as_str(),
        ),
        (
            "resolution_alignment_proof",
            review.resolution_alignment_proof.as_str(),
        ),
    ] {
        if value.trim().is_empty() {
            bail!(
                "approved candidate {} has empty {label}",
                candidate.candidate_id
            );
        }
    }
    DateTime::parse_from_rfc3339(&review.reviewed_at).with_context(|| {
        format!(
            "candidate {} reviewed_at is not RFC3339",
            candidate.candidate_id
        )
    })?;
    let fee_rate = review
        .assumed_fee_rate
        .context("approved review has no fee rate")?;
    let fee_exponent = review
        .assumed_fee_exponent
        .context("approved review has no fee exponent")?;
    let contract = |item: &super::discovery::DiscoveredContract| PartitionContract {
        condition_id: item.condition_id.clone(),
        question: item.question.clone(),
        yes_token_id: item.yes_token_id.clone(),
        no_token_id: item.no_token_id.clone(),
        rules_hash: item.rules_hash.clone(),
        fee_rate,
        fee_exponent,
    };
    let relationship = PartitionRelationship {
        relationship_id: candidate.candidate_id.clone(),
        title: format!(
            "{} -> {}",
            candidate.parent.question, candidate.children[0].event_title
        ),
        status: RelationshipReviewStatus::ManuallyApproved,
        disjoint: true,
        exhaustive: true,
        reviewed_at: Some(review.reviewed_at.clone()),
        notes: vec![
            format!("reviewer: {}", review.reviewer.trim()),
            format!(
                "mutual exclusivity: {}",
                review.mutual_exclusivity_proof.trim()
            ),
            format!("exhaustiveness: {}", review.exhaustiveness_proof.trim()),
            format!("omitted states: {}", review.omitted_states_review.trim()),
            format!(
                "resolution alignment: {}",
                review.resolution_alignment_proof.trim()
            ),
            format!("source candidate digest: {}", review.candidate_digest),
        ],
        parent: contract(&candidate.parent),
        children: candidate.children.iter().map(contract).collect(),
    };
    relationship.clone().validate(max_children)?;
    Ok(relationship)
}

fn blank_review(candidate_id: String, candidate_digest: String) -> CandidateReview {
    CandidateReview {
        candidate_id,
        candidate_digest,
        decision: CandidateReviewDecision::Pending,
        reviewer: String::new(),
        reviewed_at: String::new(),
        mutual_exclusivity_proof: String::new(),
        exhaustiveness_proof: String::new(),
        omitted_states_review: String::new(),
        resolution_alignment_proof: String::new(),
        assumed_fee_rate: None,
        assumed_fee_exponent: None,
    }
}

fn write_json_atomic(path: &Path, value: &impl Serialize) -> Result<()> {
    if let Some(parent) = path.parent().filter(|path| !path.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("json.tmp");
    {
        let mut file = File::create(&temporary)
            .with_context(|| format!("failed to create temporary file {}", temporary.display()))?;
        serde_json::to_writer_pretty(&mut file, value)?;
        writeln!(file)?;
        file.sync_all()?;
    }
    std::fs::rename(&temporary, path)
        .with_context(|| format!("failed to publish {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bayesian_consistency_arbitrage::discovery::{rules_hash, DiscoveredContract};

    fn contract(event: &str, condition: &str, question: &str) -> DiscoveredContract {
        DiscoveredContract {
            market_id: condition.into(),
            event_id: event.into(),
            event_title: format!("event {event}"),
            event_slug: event.into(),
            group_item_title: String::new(),
            end_date: String::new(),
            condition_id: condition.into(),
            question: question.into(),
            yes_token_id: format!("yes-{condition}"),
            no_token_id: format!("no-{condition}"),
            description: "rules".into(),
            resolution_source: "source".into(),
            rules_hash: rules_hash(question, "rules", "source"),
            tradable: true,
        }
    }

    fn candidate() -> DiscoveryCandidate {
        DiscoveryCandidate {
            candidate_id: "candidate-1".into(),
            status: "pending".into(),
            discovery_source: "test".into(),
            lexical_score: 1.0,
            parent: contract("parent", "p", "Parent"),
            children: vec![
                contract("children", "a", "Child A"),
                contract("children", "b", "Child B"),
            ],
            evidence: vec![],
            required_reviews: vec![],
        }
    }

    #[test]
    fn approval_requires_complete_proof_and_fee_assumption() {
        let candidate = candidate();
        let mut review = blank_review(
            candidate.candidate_id.clone(),
            candidate_digest(&candidate).unwrap(),
        );
        review.decision = CandidateReviewDecision::Approved;
        assert!(review_to_relationship(&candidate, &review, 12).is_err());
        review.reviewer = "reviewer".into();
        review.reviewed_at = "2026-09-02T00:00:00Z".into();
        review.mutual_exclusivity_proof = "non-overlapping rules".into();
        review.exhaustiveness_proof = "all outcomes covered".into();
        review.omitted_states_review = "ties and cancellation covered".into();
        review.resolution_alignment_proof = "same source and deadline".into();
        review.assumed_fee_rate = Some(Decimal::new(7, 2));
        review.assumed_fee_exponent = Some(1);
        assert!(review_to_relationship(&candidate, &review, 12).is_ok());
    }

    #[test]
    fn editing_candidate_after_review_breaks_digest_binding() {
        let candidate = candidate();
        let mut review = blank_review(
            candidate.candidate_id.clone(),
            candidate_digest(&candidate).unwrap(),
        );
        review.decision = CandidateReviewDecision::Approved;
        review.reviewer = "reviewer".into();
        review.reviewed_at = "2026-09-02T00:00:00Z".into();
        review.mutual_exclusivity_proof = "proof".into();
        review.exhaustiveness_proof = "proof".into();
        review.omitted_states_review = "proof".into();
        review.resolution_alignment_proof = "proof".into();
        review.assumed_fee_rate = Some(Decimal::new(7, 2));
        review.assumed_fee_exponent = Some(1);
        let mut edited = candidate;
        edited.parent.question = "Changed parent".into();
        assert!(review_to_relationship(&edited, &review, 12).is_err());
    }
}
