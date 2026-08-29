mod client;
mod config;
mod model;
mod runner;

pub use client::{CrossMarketProbabilityResearchClient, DiscoveryDiagnostics, DiscoverySnapshot};
pub use config::CrossMarketProbabilityArbitrageConfig;
pub use model::{
    evaluate_relationship, BookLevel, BookSnapshot, FeePolicy, MarketRelationship, Outcome,
    RelationLeg, RelationshipEvaluation, RelationshipKind, SizeEvaluation,
};
pub use runner::{run_report, run_scan, run_shadow};
