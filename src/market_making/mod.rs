mod client;
mod config;
mod model;
mod runner;

pub use client::MarketMakingResearchClient;
pub use config::MarketMakingResearchConfig;
pub use model::{
    classify_market, generate_shadow_quote, markout_observation, rank_candidates, BookSnapshot,
    MarketCandidate, MarketSelection, MarketTrade, ShadowMarkout, ShadowQuote,
};
pub use runner::{run_market_scan, run_report, run_shadow_maker};
