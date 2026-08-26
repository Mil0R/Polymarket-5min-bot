use anyhow::{bail, Result};
use polypulse::maker_liquidity::{
    run_market_scan, run_report, run_shadow_maker, MakerResearchClient, MakerResearchConfig,
};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();
    let command = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "shadow".to_string());
    let config = MakerResearchConfig::from_env()?;
    match command.as_str() {
        "scan" => {
            let client = MakerResearchClient::new(config)?;
            run_market_scan(&client).await?;
        }
        "shadow" => run_shadow_maker(MakerResearchClient::new(config)?).await?,
        "report" => {
            let path = std::env::args()
                .nth(2)
                .map(std::path::PathBuf::from)
                .unwrap_or(config.results_file);
            run_report(&path)?;
        }
        "help" | "--help" | "-h" => print_help(),
        _ => bail!("unknown maker_liquidity command '{command}'; use scan, shadow, or report"),
    }
    Ok(())
}

fn print_help() {
    println!(
        "maker_liquidity — read-only Polymarket maker feasibility research\n\n\
         Usage:\n  maker_liquidity scan\n  maker_liquidity shadow\n  maker_liquidity report [events.ndjson]\n\n\
         scan    Discover and rank currently rewarded markets, then exit.\n\
         shadow  Continuously emit hypothetical post-only quotes and markouts.\n\n\
         report  Summarize fill proxies, markouts, and estimated maker rebates.\n\n\
         This binary never signs, posts, or cancels an order."
    );
}
