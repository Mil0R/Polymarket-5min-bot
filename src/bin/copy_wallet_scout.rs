use anyhow::{Context, Result};
use dotenvy::dotenv;
use polypulse::copy_trading::{CandidateScout, ScoutConfig, ScoutProfile};
use std::env;
use std::path::PathBuf;

const HELP: &str = r#"copy-wallet-scout — read-only Polymarket copy candidate discovery

USAGE:
  copy_wallet_scout [OPTIONS]

DISCOVERY:
  --profile conservative|research  Hard-gate preset (default: conservative)
  --category LIST                  Official categories, comma-separated
  --subcategory LIST               Inferred secondary categories, comma-separated
  --period LIST                    DAY,WEEK,MONTH,ALL (default: WEEK,MONTH)
  --order LIST                     PNL,VOL (default: PNL,VOL)
  --leaderboard-limit N            Rows per leaderboard slice, 1..50
  --max-candidates N               Maximum wallets enriched after union/dedup
  --candidate-offset N             Skip this many ranked wallets before enrichment
  --wallets LIST                   Explicit addresses; may be used with leaderboard discovery

HARD GATES:
  --lookback-days N
  --recent-days N
  --max-last-trade-hours N
  --min-recent-trades N
  --min-active-days N
  --min-resolved-markets N
  --min-data-coverage FLOAT
  --min-net-roi FLOAT
  --min-profit-factor FLOAT
  --max-drawdown FLOAT
  --max-single-profit-ratio FLOAT
  --min-profitable-windows N

EXECUTION FIT:
  --live-book-sample N             Recent tokens sampled from current CLOB books
  --copy-notional-usdc FLOAT       Required Top-5 ask depth (default: 50)
  --max-live-spread FLOAT
  --min-live-execution-fit FLOAT
  --min-compatible-market-ratio FLOAT  Share passing binary/NegRisk/expiry/volume gates
  --min-actionable-signal-ratio FLOAT  Recent BUY share above COPY_MIN_SIGNAL_NOTIONAL_USDC

OUTPUT:
  --top N                          Accepted wallets written to recommended PAPER env
  --output DIR                     Default: results/copy-candidates
  -h, --help

The generated recommended-paper.env always contains COPY_DRY_RUN=true and never
enables live trading. Historical delay P&L is not claimed: live_execution_fit is
a current-book spread/depth measurement for recent markets.
"#;

#[tokio::main]
async fn main() -> Result<()> {
    dotenv().ok();
    init_logging();
    let args: Vec<String> = env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        print!("{HELP}");
        return Ok(());
    }

    let profile = option_value(&args, "--profile")
        .map(|v| ScoutProfile::parse(v))
        .transpose()?
        .unwrap_or(ScoutProfile::Conservative);
    let mut config = match profile {
        ScoutProfile::Conservative => ScoutConfig::conservative(),
        ScoutProfile::Research => ScoutConfig::research(),
    };

    let mut index = 0;
    while index < args.len() {
        let flag = &args[index];
        let value = || {
            args.get(index + 1)
                .with_context(|| format!("missing value for {flag}"))
        };
        match flag.as_str() {
            "--profile" => {
                value()?;
                index += 2;
            }
            "--category" => {
                config.categories = list(value()?);
                index += 2;
            }
            "--subcategory" => {
                config.subcategories = list(value()?);
                index += 2;
            }
            "--period" => {
                config.periods = list(value()?);
                index += 2;
            }
            "--order" => {
                config.order_by = list(value()?);
                index += 2;
            }
            "--wallets" => {
                config.explicit_wallets = list(value()?);
                index += 2;
            }
            "--leaderboard-limit" => {
                config.leaderboard_limit = parse(value()?, flag)?;
                index += 2;
            }
            "--max-candidates" => {
                config.max_candidates = parse(value()?, flag)?;
                index += 2;
            }
            "--candidate-offset" => {
                config.candidate_offset = parse(value()?, flag)?;
                index += 2;
            }
            "--top" => {
                config.top = parse(value()?, flag)?;
                index += 2;
            }
            "--lookback-days" => {
                config.lookback_days = parse(value()?, flag)?;
                index += 2;
            }
            "--recent-days" => {
                config.recent_days = parse(value()?, flag)?;
                index += 2;
            }
            "--max-last-trade-hours" => {
                config.max_last_trade_hours = parse(value()?, flag)?;
                index += 2;
            }
            "--min-recent-trades" => {
                config.min_recent_trades = parse(value()?, flag)?;
                index += 2;
            }
            "--min-active-days" => {
                config.min_recent_active_days = parse(value()?, flag)?;
                index += 2;
            }
            "--min-resolved-markets" => {
                config.min_resolved_markets = parse(value()?, flag)?;
                index += 2;
            }
            "--min-data-coverage" => {
                config.min_data_coverage = parse(value()?, flag)?;
                index += 2;
            }
            "--min-net-roi" => {
                config.min_net_roi = parse(value()?, flag)?;
                index += 2;
            }
            "--min-profit-factor" => {
                config.min_profit_factor = parse(value()?, flag)?;
                index += 2;
            }
            "--max-drawdown" => {
                config.max_drawdown = parse(value()?, flag)?;
                index += 2;
            }
            "--max-single-profit-ratio" => {
                config.max_single_profit_ratio = parse(value()?, flag)?;
                index += 2;
            }
            "--min-profitable-windows" => {
                config.min_profitable_windows = parse(value()?, flag)?;
                index += 2;
            }
            "--live-book-sample" => {
                config.live_book_sample = parse(value()?, flag)?;
                index += 2;
            }
            "--copy-notional-usdc" => {
                config.copy_notional_usdc = parse(value()?, flag)?;
                index += 2;
            }
            "--max-live-spread" => {
                config.max_live_spread = parse(value()?, flag)?;
                index += 2;
            }
            "--min-live-execution-fit" => {
                config.min_live_execution_fit = parse(value()?, flag)?;
                index += 2;
            }
            "--min-compatible-market-ratio" => {
                config.min_compatible_market_ratio = parse(value()?, flag)?;
                index += 2;
            }
            "--min-actionable-signal-ratio" => {
                config.min_actionable_signal_ratio = parse(value()?, flag)?;
                index += 2;
            }
            "--output" => {
                config.output_dir = PathBuf::from(value()?);
                index += 2;
            }
            unknown => anyhow::bail!("unknown option '{unknown}'\n\n{HELP}"),
        }
    }

    let output = config.output_dir.clone();
    let report = CandidateScout::new(config)?.run().await?;
    println!(
        "Scout complete: discovered={}, evaluated={}, accepted={}, partial_failures={}\nOutput: {}",
        report.candidates_discovered,
        report.candidates_evaluated,
        report.candidates_accepted,
        report.partial_failures.len(),
        output.display()
    );
    if report.candidates_accepted == 0 {
        println!("No wallet passed all hard gates. Inspect rejections.ndjson; do not weaken several gates at once.");
    }
    Ok(())
}

fn option_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|arg| arg == flag)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

fn list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
        .collect()
}

fn parse<T>(value: &str, flag: &str) -> Result<T>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    value
        .parse::<T>()
        .map_err(|error| anyhow::anyhow!("invalid value for {flag}: {error}"))
}

fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}
