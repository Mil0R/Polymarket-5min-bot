use anyhow::Result;
use polypulse::app::{self, RuntimeProfile};
use polypulse::polybacktest;

/// Stage-2 standalone runner. The shared runtime forcibly enables DRY_RUN and
/// disables every strategy except Late-Resolution Winner Trading.
fn main() -> Result<()> {
    match std::env::args().nth(1).as_deref() {
        Some("tui") => {
            polypulse::winner_dashboard::run(&std::env::args().skip(2).collect::<Vec<_>>())
        }
        Some("backtest") => {
            polybacktest::run_late_resolution_winner_replay()?;
            Ok(())
        }
        Some("dry-run") | None => tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?
            .block_on(app::run(RuntimeProfile::LateResolutionWinnerTradingDryRun)),
        Some("resolve-no-fill") => {
            let args: Vec<_> = std::env::args().skip(2).collect();
            anyhow::ensure!(
                args.len() == 3,
                "Usage: late_resolution_winner_trading resolve-no-fill JOURNAL POSITION_ID REASON"
            );
            app::resolve_winner_no_fill(&args[0], &args[1], &args[2])
        }
        Some("attach-order") => {
            let args: Vec<_> = std::env::args().skip(2).collect();
            anyhow::ensure!(args.len() == 4, "Usage: late_resolution_winner_trading attach-order JOURNAL POSITION_ID ORDER_ID REASON");
            app::attach_winner_order(&args[0], &args[1], &args[2], &args[3])
        }
        Some("reset-risk") => {
            let args: Vec<_> = std::env::args().skip(2).collect();
            anyhow::ensure!(
                args.len() == 2,
                "Usage: late_resolution_winner_trading reset-risk JOURNAL REASON"
            );
            app::reset_winner_risk(&args[0], &args[1])
        }
        Some("operator-info") => {
            println!("{}", polypulse::winner_dashboard::operator_specification());
            Ok(())
        }
        Some("price-cap-info") => {
            println!(
                "{}",
                polypulse::late_resolution_winner_price_cap::specification()
            );
            Ok(())
        }
        Some("help" | "--help" | "-h") => {
            println!("Usage: late_resolution_winner_trading [backtest|dry-run|price-cap-info|operator-info|tui|reset-risk JOURNAL REASON|attach-order JOURNAL POSITION_ID ORDER_ID REASON|resolve-no-fill JOURNAL POSITION_ID REASON]");
            println!("{}", polypulse::winner_dashboard::help());
            Ok(())
        }
        Some(command) => anyhow::bail!("unknown command {command}; use --help"),
    }
}
