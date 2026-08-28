mod client;
mod config;
mod model;
mod runner;

pub use client::{BayesianResearchClient, DiscoveryDiagnostics, DiscoverySnapshot};
pub use config::BayesianArbConfig;
pub use model::{
    evaluate_relationship, BookLevel, BookSnapshot, FeePolicy, MarketRelationship, Outcome,
    RelationLeg, RelationshipEvaluation, RelationshipKind, SizeEvaluation,
};
pub use runner::{run_report, run_scan, run_shadow};
