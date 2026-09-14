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
        Some("calibrate") => {
            polypulse::digital_fair_value_calibration::run(
                std::env::args().any(|a| a == "--refresh"),
            )
            .await?;
            Ok(())
        }
        Some("validate") => {
            if std::env::args().any(|arg| arg == "--refresh") {
                polypulse::digital_fair_value_calibration::run(true).await?;
            }
            polypulse::digital_fair_value_calibration::run_split_validation()?;
            Ok(())
        }
        Some("report") => {
            let report = digital_fair_value::run_dry_run_report()?;
            println!("report={}", report.display());
            Ok(())
        }
        Some("help" | "--help" | "-h") => {
            println!("Usage: short_duration_digital_fair_value [backtest|dry-run|report|calibrate [--refresh]|validate [--refresh]]");
            Ok(())
        }
        Some(command) => {
            anyhow::bail!(
                "unknown command {command}; use backtest, dry-run, report, calibrate or validate"
            )
        }
    }
}
