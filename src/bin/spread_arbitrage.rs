use anyhow::Result;
use polypulse::app::{self, RuntimeProfile};
use polypulse::polybacktest;

/// Stage-2 standalone runner. The shared runtime forcibly enables DRY_RUN and
/// disables every strategy except Spread Arbitrage.
#[tokio::main]
async fn main() -> Result<()> {
    match std::env::args().nth(1).as_deref() {
        Some("backtest") => {
            polybacktest::run_spread_replay()?;
            Ok(())
        }
        Some("dry-run") | None => app::run(RuntimeProfile::SpreadArbitrageDryRun).await,
        Some("help" | "--help" | "-h") => {
            println!("Usage: spread_arbitrage [backtest|dry-run]");
            Ok(())
        }
        Some(command) => anyhow::bail!("unknown command {command}; use backtest or dry-run"),
    }
}
