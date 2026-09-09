use anyhow::{bail, Result};
use polypulse::market_making::{
    record_reward_snapshot, run_market_scan, run_report, run_shadow_maker, watch_reward_snapshots,
    MarketMakingResearchClient, MarketMakingResearchConfig,
};

#[tokio::main]
async fn main() -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("failed to install rustls crypto provider");
    let _ = dotenvy::dotenv();
    polypulse::init_research_system_logger("MARKET_MAKING", "logs/market-making/system.log")?;
    let command = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "shadow".to_string());
    let config = MarketMakingResearchConfig::from_env()?;
    match command.as_str() {
        "scan" => {
            let client = MarketMakingResearchClient::new(config)?;
            run_market_scan(&client).await?;
        }
        "shadow" => run_shadow_maker(MarketMakingResearchClient::new(config)?).await?,
        "report" => {
            let path = std::env::args()
                .nth(2)
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| config.results_file.clone());
            run_report(&path, &config)?;
        }
        "reward-audit" => {
            let date = std::env::args()
                .nth(2)
                .map(|value| chrono::NaiveDate::parse_from_str(&value, "%Y-%m-%d"))
                .transpose()?
                .unwrap_or_else(|| chrono::Utc::now().date_naive());
            record_reward_snapshot(&config, date).await?;
        }
        "reward-watch" => watch_reward_snapshots(config).await?,
        "help" | "--help" | "-h" => print_help(),
        _ => bail!(
            "unknown market_making command '{command}'; use scan, shadow, report, reward-audit, or reward-watch"
        ),
    }
    Ok(())
}

fn print_help() {
    println!(
        "market_making — read-only Polymarket maker feasibility research\n\n\
         Usage:\n  market_making scan\n  market_making shadow\n  market_making report [events.ndjson]\n  market_making reward-audit [YYYY-MM-DD]\n  market_making reward-watch\n\n\
         scan    Discover and rank currently rewarded markets, then exit.\n\
         shadow  Continuously emit hypothetical post-only quotes and markouts.\n\n\
         report        Summarize queue-aware fills, 5m/30m and exit/settlement PnL, and readiness gates.\n\
         reward-audit  Append one authenticated per-market Liquidity Rewards snapshot.\n\
         reward-watch  Append the current UTC day's reward snapshot once per configured interval.\n\n\
         This binary never signs, posts, or cancels an order."
    );
}
