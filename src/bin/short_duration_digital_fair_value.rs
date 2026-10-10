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
        Some("opening-probe") => {
            let args = std::env::args().skip(2).collect::<Vec<_>>();
            if args.len() > 2 {
                anyhow::bail!("opening-probe [btc,eth] [NEW_REPORT_FILE]");
            }
            let symbols = args.first().map(String::as_str).unwrap_or("btc,eth");
            let output = args
                .get(1)
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| {
                    std::path::PathBuf::from(format!(
                        "results/short-duration-digital-fair-value/opening-probes/probe-{}.json",
                        chrono::Utc::now().format("%Y%m%dT%H%M%S%.3fZ")
                    ))
                });
            polypulse::digital_fair_value_opening::run_probe(symbols, &output).await
        }
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
        Some("terminal-parity") => {
            let args = std::env::args().skip(2).collect::<Vec<_>>();
            if args.len() != 4 {
                anyhow::bail!(
                    "terminal-parity requires MODEL_FILE SHA256 FIXTURE_FILE REPORT_FILE"
                );
            }
            polypulse::digital_fair_value_terminal_model::run_parity(
                std::path::Path::new(&args[0]),
                &args[1],
                std::path::Path::new(&args[2]),
                std::path::Path::new(&args[3]),
            )
        }
        Some("help" | "--help" | "-h") => {
            println!("Usage: short_duration_digital_fair_value [backtest|dry-run|report|calibrate [--refresh]|validate [--refresh]|opening-probe [btc,eth] [NEW_REPORT_FILE]|terminal-parity MODEL_FILE SHA256 FIXTURE_FILE REPORT_FILE]");
            Ok(())
        }
        Some(command) => {
            anyhow::bail!(
                "unknown command {command}; use backtest, dry-run, report, calibrate, validate, opening-probe or terminal-parity"
            )
        }
    }
}
