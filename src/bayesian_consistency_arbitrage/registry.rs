use super::model::{PartitionRelationship, RelationshipReviewStatus, ValidatedPartition};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryFile {
    pub version: u32,
    #[serde(default)]
    pub relationships: Vec<PartitionRelationship>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RegistryValidationReport {
    pub total: usize,
    pub approved: usize,
    pub pending: usize,
    pub rejected: usize,
    pub valid_approved: usize,
    pub invalid: Vec<String>,
}

pub fn load_registry(path: &Path) -> Result<RegistryFile> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("failed to read relationship registry {}", path.display()))?;
    let registry: RegistryFile = serde_json::from_slice(&bytes)
        .with_context(|| format!("invalid relationship registry {}", path.display()))?;
    if registry.version != 1 {
        bail!(
            "unsupported relationship registry version {}",
            registry.version
        );
    }
    Ok(registry)
}

pub fn validate_registry(
    registry: &RegistryFile,
    max_children: usize,
) -> (RegistryValidationReport, Vec<ValidatedPartition>) {
    let mut ids = HashSet::new();
    let mut valid = Vec::new();
    let mut invalid = Vec::new();
    let mut approved = 0;
    let mut pending = 0;
    let mut rejected = 0;
    for relationship in &registry.relationships {
        match relationship.status {
            RelationshipReviewStatus::ManuallyApproved => approved += 1,
            RelationshipReviewStatus::Pending => pending += 1,
            RelationshipReviewStatus::Rejected => rejected += 1,
        }
        if !ids.insert(relationship.relationship_id.clone()) {
            invalid.push(format!(
                "{}: duplicate relationship_id",
                relationship.relationship_id
            ));
            continue;
        }
        if relationship.status == RelationshipReviewStatus::ManuallyApproved {
            match relationship.clone().validate(max_children) {
                Ok(item) => valid.push(item),
                Err(error) => invalid.push(format!("{}: {error:#}", relationship.relationship_id)),
            }
        }
    }
    (
        RegistryValidationReport {
            total: registry.relationships.len(),
            approved,
            pending,
            rejected,
            valid_approved: valid.len(),
            invalid,
        },
        valid,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_v1_registry_is_valid_but_has_no_live_research_relationships() {
        let registry = RegistryFile {
            version: 1,
            relationships: vec![],
        };
        let (report, valid) = validate_registry(&registry, 12);
        assert!(report.invalid.is_empty());
        assert!(valid.is_empty());
    }
}
