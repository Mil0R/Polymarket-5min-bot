use anyhow::{bail, Context, Result};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

fn default_fee_exponent() -> u32 {
    1
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RelationshipReviewStatus {
    Pending,
    ManuallyApproved,
    Rejected,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PartitionContract {
    pub condition_id: String,
    pub question: String,
    pub yes_token_id: String,
    pub no_token_id: String,
    /// Stable digest of the reviewed resolution rules, normally `sha256:<hex>`.
    pub rules_hash: String,
    #[serde(default)]
    pub fee_rate: Decimal,
    #[serde(default = "default_fee_exponent")]
    pub fee_exponent: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PartitionRelationship {
    pub relationship_id: String,
    pub title: String,
    pub status: RelationshipReviewStatus,
    pub disjoint: bool,
    pub exhaustive: bool,
    pub reviewed_at: Option<String>,
    #[serde(default)]
    pub notes: Vec<String>,
    pub parent: PartitionContract,
    pub children: Vec<PartitionContract>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HedgeDirection {
    /// Buy Parent NO and every Child YES.  Every valid state pays one unit.
    ParentNoChildrenYes,
    /// Buy Parent YES and every Child NO.  With N children every state pays N.
    ParentYesChildrenNo,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HedgeOutcome {
    Yes,
    No,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HedgeLeg {
    pub condition_id: String,
    pub question: String,
    pub token_id: String,
    pub outcome: HedgeOutcome,
    pub fee_rate: Decimal,
    pub fee_exponent: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HedgeBundle {
    pub relationship_id: String,
    pub direction: HedgeDirection,
    pub guaranteed_payout_per_share: Decimal,
    pub legs: Vec<HedgeLeg>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ValidatedPartition {
    pub relationship: PartitionRelationship,
    pub bundles: [HedgeBundle; 2],
}

impl PartitionRelationship {
    pub fn validate(self, max_children: usize) -> Result<ValidatedPartition> {
        if self.status != RelationshipReviewStatus::ManuallyApproved {
            bail!(
                "relationship {} is not manually approved",
                self.relationship_id
            );
        }
        if !self.disjoint || !self.exhaustive {
            bail!(
                "relationship {} must be explicitly disjoint and exhaustive",
                self.relationship_id
            );
        }
        if self.reviewed_at.as_deref().is_none_or(str::is_empty) {
            bail!("relationship {} has no reviewed_at", self.relationship_id);
        }
        if self.relationship_id.trim().is_empty() || self.title.trim().is_empty() {
            bail!("relationship id and title cannot be empty");
        }
        if self.children.len() < 2 || self.children.len() > max_children {
            bail!(
                "relationship {} must contain 2..={max_children} children",
                self.relationship_id
            );
        }

        let mut condition_ids = HashSet::new();
        let mut token_ids = HashSet::new();
        validate_contract(&self.parent, &mut condition_ids, &mut token_ids)
            .context("invalid parent contract")?;
        for (index, child) in self.children.iter().enumerate() {
            validate_contract(child, &mut condition_ids, &mut token_ids)
                .with_context(|| format!("invalid child contract at index {index}"))?;
        }

        let cheap_children = HedgeBundle {
            relationship_id: self.relationship_id.clone(),
            direction: HedgeDirection::ParentNoChildrenYes,
            guaranteed_payout_per_share: Decimal::ONE,
            legs: std::iter::once(hedge_leg(&self.parent, false))
                .chain(self.children.iter().map(|child| hedge_leg(child, true)))
                .collect(),
        };
        let rich_children = HedgeBundle {
            relationship_id: self.relationship_id.clone(),
            direction: HedgeDirection::ParentYesChildrenNo,
            guaranteed_payout_per_share: Decimal::from(self.children.len() as u64),
            legs: std::iter::once(hedge_leg(&self.parent, true))
                .chain(self.children.iter().map(|child| hedge_leg(child, false)))
                .collect(),
        };

        verify_constant_payout(&cheap_children, self.children.len())?;
        verify_constant_payout(&rich_children, self.children.len())?;
        Ok(ValidatedPartition {
            relationship: self,
            bundles: [cheap_children, rich_children],
        })
    }
}

fn validate_contract(
    contract: &PartitionContract,
    condition_ids: &mut HashSet<String>,
    token_ids: &mut HashSet<String>,
) -> Result<()> {
    if contract.condition_id.trim().is_empty()
        || contract.question.trim().is_empty()
        || contract.yes_token_id.trim().is_empty()
        || contract.no_token_id.trim().is_empty()
    {
        bail!("condition, question and token ids are required");
    }
    if !contract.rules_hash.starts_with("sha256:") || contract.rules_hash.len() != 71 {
        bail!("rules_hash must be sha256:<64 lowercase-or-uppercase hex characters>");
    }
    if !contract.rules_hash[7..]
        .chars()
        .all(|character| character.is_ascii_hexdigit())
    {
        bail!("rules_hash contains non-hex characters");
    }
    if contract.yes_token_id == contract.no_token_id {
        bail!("YES and NO token ids must differ");
    }
    if contract.fee_rate < Decimal::ZERO || contract.fee_rate >= Decimal::ONE {
        bail!("fee_rate must be in [0, 1)");
    }
    if contract.fee_exponent > 8 {
        bail!("fee_exponent must be <= 8");
    }
    if !condition_ids.insert(contract.condition_id.clone()) {
        bail!("condition id is duplicated in the relationship");
    }
    if !token_ids.insert(contract.yes_token_id.clone())
        || !token_ids.insert(contract.no_token_id.clone())
    {
        bail!("token id is duplicated in the relationship");
    }
    Ok(())
}

fn hedge_leg(contract: &PartitionContract, yes: bool) -> HedgeLeg {
    HedgeLeg {
        condition_id: contract.condition_id.clone(),
        question: contract.question.clone(),
        token_id: if yes {
            contract.yes_token_id.clone()
        } else {
            contract.no_token_id.clone()
        },
        outcome: if yes {
            HedgeOutcome::Yes
        } else {
            HedgeOutcome::No
        },
        fee_rate: contract.fee_rate,
        fee_exponent: contract.fee_exponent,
    }
}

/// Enumerate every state allowed by an exact partition: parent false, or one
/// and only one child true.  Both v1 hedge constructions must pay a constant.
fn verify_constant_payout(bundle: &HedgeBundle, child_count: usize) -> Result<()> {
    for winning_child in std::iter::once(None).chain((0..child_count).map(Some)) {
        let parent_true = winning_child.is_some();
        let mut payout = Decimal::ZERO;
        for (index, leg) in bundle.legs.iter().enumerate() {
            let contract_true = if index == 0 {
                parent_true
            } else {
                winning_child == Some(index - 1)
            };
            let leg_wins = (leg.outcome == HedgeOutcome::Yes && contract_true)
                || (leg.outcome == HedgeOutcome::No && !contract_true);
            if leg_wins {
                payout += Decimal::ONE;
            }
        }
        if payout != bundle.guaranteed_payout_per_share {
            bail!(
                "bundle {:?} is not payout invariant: expected {}, got {}",
                bundle.direction,
                bundle.guaranteed_payout_per_share,
                payout
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn contract(name: &str) -> PartitionContract {
        PartitionContract {
            condition_id: format!("condition-{name}"),
            question: name.to_string(),
            yes_token_id: format!("yes-{name}"),
            no_token_id: format!("no-{name}"),
            rules_hash: format!("sha256:{}", "a".repeat(64)),
            fee_rate: dec!(0.07),
            fee_exponent: 1,
        }
    }

    fn relationship() -> PartitionRelationship {
        PartitionRelationship {
            relationship_id: "election-margin".into(),
            title: "Winning margin partitions parent victory".into(),
            status: RelationshipReviewStatus::ManuallyApproved,
            disjoint: true,
            exhaustive: true,
            reviewed_at: Some("2026-09-02T00:00:00Z".into()),
            notes: vec![],
            parent: contract("parent"),
            children: vec![contract("large"), contract("narrow")],
        }
    }

    #[test]
    fn exact_partition_builds_two_constant_payout_bundles() {
        let validated = relationship().validate(8).unwrap();
        assert_eq!(validated.bundles[0].guaranteed_payout_per_share, dec!(1));
        assert_eq!(validated.bundles[1].guaranteed_payout_per_share, dec!(2));
        assert_eq!(validated.bundles[0].legs.len(), 3);
        assert_eq!(validated.bundles[1].legs.len(), 3);
    }

    #[test]
    fn inferred_or_incomplete_relationships_fail_closed() {
        let mut pending = relationship();
        pending.status = RelationshipReviewStatus::Pending;
        assert!(pending.validate(8).is_err());

        let mut incomplete = relationship();
        incomplete.exhaustive = false;
        assert!(incomplete.validate(8).is_err());
    }

    #[test]
    fn duplicate_contract_or_token_is_rejected() {
        let mut duplicate = relationship();
        duplicate.children[1].condition_id = duplicate.children[0].condition_id.clone();
        assert!(duplicate.validate(8).is_err());
    }
}
