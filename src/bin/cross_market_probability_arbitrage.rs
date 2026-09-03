use anyhow::{bail, Result};
use polypulse::cross_market_probability_arbitrage::{
    run_report, run_scan, run_shadow, CrossMarketProbabilityArbitrageConfig,
    CrossMarketProbabilityResearchClient,
};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("failed to install rustls crypto provider");
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();
    let command = std::env::args().nth(1).unwrap_or_else(|| "scan".into());
    if matches!(command.as_str(), "help" | "--help" | "-h") {
        print_help();
        return Ok(());
    }
    let config = CrossMarketProbabilityArbitrageConfig::from_env()?;
    match command.as_str() {
        "scan" => {
            run_scan(&CrossMarketProbabilityResearchClient::new(config)?).await?;
        }
        "shadow" => run_shadow(CrossMarketProbabilityResearchClient::new(config)?).await?,
        "report" => {
            let path = std::env::args()
                .nth(2)
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| config.results_file.clone());
            run_report(&path, &config)?;
        }
        _ => bail!("unknown cross_market_probability_arbitrage command '{command}'; use scan, shadow, or report"),
    }
    Ok(())
}

fn print_help() {
    println!(
        "cross_market_probability_arbitrage — read-only cross-market probability arbitrage research\n\n\
         Usage:\n  cross_market_probability_arbitrage scan\n  cross_market_probability_arbitrage shadow\n  cross_market_probability_arbitrage report [events.ndjson]\n\n\
         scan    Discover relationships and evaluate one live order-book snapshot.\n\
         shadow  Continuously evaluate and journal materially changed snapshots.\n\
         report  Summarize synchronized edges, independent episodes, delayed survival, and failure stress; also writes REPORT_FILE.\n\n\
         Safety: this binary has no signer and never submits, cancels, splits, merges, or converts positions."
    );
}
