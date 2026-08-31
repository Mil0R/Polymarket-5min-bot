use anyhow::Result;
use polypulse::digital_fair_value;

#[tokio::main]
async fn main() -> Result<()> {
    match std::env::args().nth(1).as_deref() {
        Some("backtest") | None => {
            let report = digital_fair_value::run_backtest()?;
            println!("report={}", report.display());
            Ok(())
        }
        Some("dry-run") => digital_fair_value::run_dry_run().await,
        Some("help" | "--help" | "-h") => {
            println!("Usage: short_duration_digital_fair_value [backtest|dry-run]");
            Ok(())
        }
        Some(command) => anyhow::bail!("unknown command {command}; use backtest or dry-run"),
    }
}
