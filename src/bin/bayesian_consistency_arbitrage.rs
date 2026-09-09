use anyhow::{bail, Result};
use polypulse::bayesian_consistency_arbitrage::{
    load_registry, run_audit, run_boundary_analysis, run_discover, run_discover_history,
    run_monitor, run_prepare_review, run_promote, run_report, run_scan, run_shadow,
    validate_registry, BayesianConsistencyConfig,
};

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    let command = std::env::args().nth(1).unwrap_or_else(|| "validate".into());
    if matches!(command.as_str(), "help" | "--help" | "-h") {
        print_help();
        return Ok(());
    }
    let config = BayesianConsistencyConfig::from_env()?;
    polypulse::init_research_system_logger(
        "BAYESIAN_CONSISTENCY_ARBITRAGE",
        "logs/bayesian-consistency-arbitrage/system.log",
    )?;
    match command.as_str() {
        "discover" => {
            run_discover(config).await?;
        }
        "discover-history" => {
            run_discover_history(config).await?;
        }
        "analyze-boundaries" => {
            run_boundary_analysis(config).await?;
        }
        "monitor" => {
            run_monitor(config).await?;
        }
        "prepare-review" => {
            run_prepare_review(&config)?;
        }
        "promote" => {
            run_promote(&config)?;
        }
        "validate" => {
            let registry = load_registry(&config.registry_file)?;
            let (report, _) = validate_registry(&registry, config.max_children);
            println!("{}", serde_json::to_string_pretty(&report)?);
            if !report.invalid.is_empty() {
                bail!("relationship registry contains invalid approved entries");
            }
        }
        "audit" => {
            run_audit(&config).await?;
        }
        "scan" => {
            run_scan(config).await?;
        }
        "shadow" => run_shadow(config).await?,
        "report" => {
            let path = std::env::args()
                .nth(2)
                .map(std::path::PathBuf::from)
                .unwrap_or(config.results_file);
            run_report(&path)?;
        }
        _ => bail!(
            "unknown bayesian_consistency_arbitrage command '{command}'; use discover, discover-history, analyze-boundaries, monitor, prepare-review, promote, validate, audit, scan, shadow, or report"
        ),
    }
    Ok(())
}

fn print_help() {
    println!(
        "bayesian_consistency_arbitrage — read-only exact-partition research\n\n\
         Usage:\n  bayesian_consistency_arbitrage discover\n  bayesian_consistency_arbitrage discover-history\n  bayesian_consistency_arbitrage analyze-boundaries\n  bayesian_consistency_arbitrage monitor\n  bayesian_consistency_arbitrage prepare-review\n  bayesian_consistency_arbitrage promote\n  bayesian_consistency_arbitrage validate\n  bayesian_consistency_arbitrage audit\n  bayesian_consistency_arbitrage scan\n  bayesian_consistency_arbitrage shadow\n  bayesian_consistency_arbitrage report [events.ndjson]\n\n\
         discover  Generate pending lexical candidates without changing the registry.\n\n\
         discover-history  Research closed markets; output can never be promoted.\n\n\
         analyze-boundaries  Explain numeric gaps, overlaps and threshold-splitting buckets.\n\n\
         monitor  Run hourly incremental discovery and alert on changes; never scans or trades.\n\n\
         prepare-review  Create or safely merge the human review checklist.\n\n\
         promote   Append fully reviewed, snapshot-bound candidates to the registry.\n\n\
         validate  Validate the manually reviewed parent/child relationship registry.\n\n\
         audit     Re-fetch Gamma metadata and fail on rule, token, or status drift.\n\n\
         scan      Evaluate one synchronized read-only CLOB snapshot.\n\n\
         shadow    Continuously evaluate and delay-recheck independent opportunities.\n\n\
         report    Summarize opportunities, delayed completion, and rejection reasons.\n\n\
         Safety: v1 has no signer and never places, cancels, splits, merges, or converts."
    );
}
