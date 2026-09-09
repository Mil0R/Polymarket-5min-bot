use anyhow::Result;
#[tokio::main]
async fn main() -> Result<()> {
    let command = std::env::args().nth(1).unwrap_or_else(|| "help".into());
    if matches!(command.as_str(), "help" | "--help" | "-h") {
        println!("Usage: neg_risk_conversion_arbitrage [scan|shadow|report]\nStage-1 read-only economics research. No signer, orders or on-chain conversions.");
        return Ok(());
    }
    polypulse::init_research_system_logger(
        "NEG_RISK_CONVERSION_ARBITRAGE",
        "logs/neg-risk-conversion-arbitrage/system.log",
    )?;
    polypulse::cross_market_probability_arbitrage::conversion::run(&command).await
}
