use anyhow::Result;
use polypulse::app::{self, RuntimeProfile};
use polypulse::polybacktest;

/// Stage-2 standalone runner. The shared runtime forcibly enables DRY_RUN and
/// disables every strategy except Late-Resolution Winner Trading.
#[tokio::main]
async fn main() -> Result<()> {
    match std::env::args().nth(1).as_deref() {
        Some("backtest") => {
            polybacktest::run_late_resolution_winner_replay()?;
            Ok(())
        }
        Some("dry-run") | None => app::run(RuntimeProfile::LateResolutionWinnerTradingDryRun).await,
        Some("help" | "--help" | "-h") => {
            println!("Usage: late_resolution_winner_trading [backtest|dry-run]");
            Ok(())
        }
        Some(command) => anyhow::bail!("unknown command {command}; use backtest or dry-run"),
    }
}
