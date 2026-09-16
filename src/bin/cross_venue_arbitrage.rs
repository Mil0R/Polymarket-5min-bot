use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    let command = std::env::args().nth(1).unwrap_or_else(|| "help".into());
    if matches!(command.as_str(), "help" | "--help" | "-h") {
        println!("Usage: cross_venue_arbitrage [scan|leads|inspect|probe|shadow|report]\nPMP discovery + Kalshi/Polymarket read-only research. leads writes an unresolved manual-review queue without changing the registry; inspect prints rule-review fingerprints; probe checks manually reviewed pairs without PMP. Diagnostic results use separate inspect/ and probe/ subdirectories. No orders or signers.");
        return Ok(());
    }
    if matches!(command.as_str(), "inspect" | "probe") {
        dotenvy::dotenv().ok();
        let path = std::env::var("CROSS_VENUE_ARBITRAGE_SYSTEM_LOG_FILE")
            .unwrap_or_else(|_| "logs/cross-venue-arbitrage/shadow-run-001/system.log".into());
        let path = std::path::Path::new(&path).with_file_name(format!("system-{command}.log"));
        std::env::set_var("CROSS_VENUE_ARBITRAGE_SYSTEM_LOG_FILE", path);
    }
    polypulse::init_research_system_logger(
        "CROSS_VENUE_ARBITRAGE",
        "logs/cross-venue-arbitrage/shadow-run-001/system.log",
    )?;
    polypulse::cross_venue_arbitrage::run(&command).await
}
