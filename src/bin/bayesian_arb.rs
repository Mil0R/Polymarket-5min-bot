use anyhow::{bail, Result};
use polypulse::bayesian_arb::{
    run_report, run_scan, run_shadow, BayesianArbConfig, BayesianResearchClient,
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
    let config = BayesianArbConfig::from_env()?;
    match command.as_str() {
        "scan" => {
            run_scan(&BayesianResearchClient::new(config)?).await?;
        }
        "shadow" => run_shadow(BayesianResearchClient::new(config)?).await?,
        "report" => {
            let path = std::env::args()
                .nth(2)
                .map(std::path::PathBuf::from)
                .unwrap_or(config.results_file);
            run_report(&path)?;
        }
        _ => bail!("unknown bayesian_arb command '{command}'; use scan, shadow, or report"),
    }
    Ok(())
}

fn print_help() {
    println!(
        "bayesian_arb — read-only cross-market coherence research\n\n\
         Usage:\n  bayesian_arb scan\n  bayesian_arb shadow\n  bayesian_arb report [events.ndjson]\n\n\
         scan    Discover relationships and evaluate one live order-book snapshot.\n\
         shadow  Continuously evaluate and journal materially changed snapshots.\n\
         report  Summarize executable edges, depth failures, and fee-adjusted profit.\n\n\
         Safety: this binary has no signer and never submits, cancels, splits, merges, or converts positions."
    );
}
