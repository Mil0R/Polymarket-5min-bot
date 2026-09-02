//! Read-only research for exact parent/child partition consistency.
//!
//! Despite the strategy name, v1 deliberately excludes inferred conditional
//! probabilities.  A relationship is admitted only when manually reviewed
//! child markets form a mutually-exclusive, exhaustive partition of the
//! parent market.

mod boundary;
mod client;
mod config;
mod discovery;
mod evaluator;
mod model;
mod registry;
mod review;
mod runner;

pub use boundary::{BoundaryDiagnostic, BoundaryReport, NumericInterval};
pub use client::{BayesianBookClient, ContractAudit, RegistryAuditReport};
pub use config::BayesianConsistencyConfig;
pub use discovery::{CandidateSeed, CandidateSeedFile, DiscoveryCandidate, DiscoveryReport};
pub use evaluator::{evaluate_bundle, BookLevel, BookSnapshot, BundleEvaluation, LegFill};
pub use model::{
    HedgeBundle, HedgeDirection, HedgeOutcome, PartitionContract, PartitionRelationship,
    RelationshipReviewStatus, ValidatedPartition,
};
pub use registry::{load_registry, validate_registry, RegistryFile, RegistryValidationReport};
pub use review::{
    candidate_digest, load_discovery, load_review, prepare_review, promote_reviewed,
    CandidateReview, CandidateReviewDecision, PromotionReport, ReviewFile,
};
pub use runner::{
    run_audit, run_boundary_analysis, run_discover, run_discover_history, run_monitor,
    run_prepare_review, run_promote, run_report, run_scan, run_shadow, BayesianReport,
};
pub use runner::{MonitorDiff, MonitorSnapshot, MonitorState};
