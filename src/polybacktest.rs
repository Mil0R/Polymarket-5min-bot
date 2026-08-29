//! Deterministic stage-1 replay against the SQLite database produced by
//! `polybacktest_downloader`. No network client or signer is reachable here.

use crate::late_resolution_winner_model::{
    ChainlinkTwapStore, TwapSample, WinnerModelInput, WinnerModelPolicy,
};
use crate::late_resolution_winner_trading::detect_winner_asks;
use anyhow::{bail, Context, Result};
use chrono::Utc;
use rusqlite::{Connection, Row};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::str::FromStr;

#[derive(Debug, Clone)]
pub struct ReplayConfig {
    database_file: PathBuf,
    output_dir: PathBuf,
    order_usdc: Decimal,
    execution_delay_ms: i64,
    liquidity_haircut: Decimal,
    fee_rate: Decimal,
    fee_exponent: u64,
    spread_min_edge: Decimal,
    spread_min_net_profit: Decimal,
    winner_before_end_secs: i64,
    winner_min_price: Decimal,
    winner_max_price: Decimal,
    winner_confirmations: u32,
    winner_policy: WinnerModelPolicy,
}

impl ReplayConfig {
    pub fn from_env() -> Result<Self> {
        let _ = dotenvy::dotenv();
        let data_dir = std::env::var("POLYBACKTEST_DATA_DIR")
            .unwrap_or_else(|_| "data/polybacktest".to_string());
        let database_file = std::env::var_os("BACKTEST_DATABASE_FILE")
            .or_else(|| std::env::var_os("POLYBACKTEST_DATABASE_FILE"))
            .map(PathBuf::from)
            .unwrap_or_else(|| Path::new(&data_dir).join("polybacktest.sqlite3"));
        let fee_rate = env_decimal("BACKTEST_FEE_RATE", "0.25")?;
        let fee_exponent = env_parse("BACKTEST_FEE_EXPONENT", 2u64)?;
        let winner_fee = dynamic_fee_per_share(fee_rate, fee_exponent, dec!(0.97));
        Ok(Self {
            database_file,
            output_dir: std::env::var("BACKTEST_OUTPUT_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("results/backtests")),
            order_usdc: env_decimal("BACKTEST_ORDER_USDC", "5")?,
            execution_delay_ms: env_parse("BACKTEST_EXECUTION_DELAY_MS", 250i64)?,
            liquidity_haircut: env_decimal("BACKTEST_LIQUIDITY_HAIRCUT", "0.75")?,
            fee_rate,
            fee_exponent,
            spread_min_edge: env_decimal("BINARY_COMPLEMENT_ARBITRAGE_BACKTEST_MIN_EDGE", "0.005")?,
            spread_min_net_profit: env_decimal(
                "BINARY_COMPLEMENT_ARBITRAGE_BACKTEST_MIN_NET_PROFIT_USDC",
                "0.01",
            )?,
            winner_before_end_secs: env_parse(
                "LATE_RESOLUTION_WINNER_TRADING_BEFORE_END_SECS",
                30i64,
            )?,
            winner_min_price: env_decimal("LATE_RESOLUTION_WINNER_TRADING_MIN_PRICE", "0.95")?,
            winner_max_price: env_decimal("LATE_RESOLUTION_WINNER_TRADING_MAX_PRICE", "0.98")?,
            winner_confirmations: env_parse(
                "LATE_RESOLUTION_WINNER_TRADING_CONFIRMATION_OBSERVATIONS",
                3u32,
            )?,
            winner_policy: WinnerModelPolicy {
                max_feed_age_ms: env_parse(
                    "LATE_RESOLUTION_WINNER_TRADING_FEED_MAX_AGE_MS",
                    5000i64,
                )?,
                max_opening_skew_ms: env_parse(
                    "LATE_RESOLUTION_WINNER_TRADING_OPENING_MAX_SKEW_MS",
                    5000i64,
                )?,
                volatility_lookback_secs: env_parse(
                    "LATE_RESOLUTION_WINNER_TRADING_VOLATILITY_LOOKBACK_SECS",
                    120u64,
                )?,
                min_observations: env_parse(
                    "LATE_RESOLUTION_WINNER_TRADING_MIN_OBSERVATIONS",
                    10usize,
                )?,
                min_volatility_bps_per_sqrt_second: env_parse(
                    "LATE_RESOLUTION_WINNER_TRADING_BTC_MIN_VOLATILITY_BPS",
                    0.8f64,
                )?,
                min_margin_bps: env_parse("LATE_RESOLUTION_WINNER_TRADING_MIN_MARGIN_BPS", 3.0f64)?,
                reversal_sigma_multiplier: env_parse(
                    "LATE_RESOLUTION_WINNER_TRADING_REVERSAL_SIGMA",
                    2.0f64,
                )?,
                max_spread: env_decimal("LATE_RESOLUTION_WINNER_TRADING_MAX_SPREAD", "0.03")?,
                min_depth_usdc: env_decimal("LATE_RESOLUTION_WINNER_TRADING_MIN_DEPTH_USDC", "10")?,
                fee_buffer: winner_fee,
                model_risk_buffer: env_decimal(
                    "LATE_RESOLUTION_WINNER_TRADING_BTC_MODEL_RISK_BUFFER",
                    "0.01",
                )?,
                min_net_ev: env_decimal("LATE_RESOLUTION_WINNER_TRADING_MIN_NET_EV", "0.002")?,
            },
        })
    }

    fn validate(&self) -> Result<()> {
        if !self.database_file.is_file() {
            bail!(
                "PolyBacktest database not found: {}; run polybacktest_downloader sync first",
                self.database_file.display()
            );
        }
        if self.order_usdc <= Decimal::ZERO {
            bail!("BACKTEST_ORDER_USDC must be greater than zero");
        }
        if self.execution_delay_ms < 0 {
            bail!("BACKTEST_EXECUTION_DELAY_MS cannot be negative");
        }
        if self.liquidity_haircut <= Decimal::ZERO || self.liquidity_haircut > Decimal::ONE {
            bail!("BACKTEST_LIQUIDITY_HAIRCUT must be in (0,1]");
        }
        if self.winner_confirmations == 0 {
            bail!("LATE_RESOLUTION_WINNER_TRADING_CONFIRMATION_OBSERVATIONS must be greater than zero");
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct Market {
    id: String,
    coin: String,
    start_ms: i64,
    end_ms: i64,
    winner: Option<String>,
}

#[derive(Debug, Clone)]
struct Snapshot {
    time_ms: i64,
    twap: Option<Decimal>,
    twap_observed_at_ms: Option<i64>,
    twap_source: Option<String>,
    up_bid: Option<Decimal>,
    up_ask: Option<Decimal>,
    up_ask_size: Option<Decimal>,
    down_bid: Option<Decimal>,
    down_ask: Option<Decimal>,
    down_ask_size: Option<Decimal>,
    up_book: Option<Vec<u8>>,
    down_book: Option<Vec<u8>>,
}

#[derive(Debug, Serialize)]
pub struct ReplayReport {
    strategy: &'static str,
    generated_at: String,
    database: String,
    markets: usize,
    snapshots: usize,
    candidates: usize,
    admitted: usize,
    fills: usize,
    wins: usize,
    losses: usize,
    rejections: BTreeMap<String, usize>,
    gross_pnl_usdc: String,
    fees_usdc: String,
    net_pnl_usdc: String,
    output_file: String,
    limitations: Vec<&'static str>,
}

#[derive(Default)]
struct Totals {
    snapshots: usize,
    candidates: usize,
    admitted: usize,
    fills: usize,
    wins: usize,
    losses: usize,
    rejections: BTreeMap<String, usize>,
    gross_pnl: Decimal,
    fees: Decimal,
}

pub fn run_binary_complement_replay() -> Result<PathBuf> {
    let config = ReplayConfig::from_env()?;
    config.validate()?;
    let connection = open_database(&config)?;
    let markets = load_markets(&connection)?;
    let mut totals = Totals::default();
    for (index, market) in markets.iter().enumerate() {
        let snapshots = load_snapshots(&connection, &market.id)?;
        totals.snapshots += snapshots.len();
        replay_spread_market(&config, &snapshots, &mut totals)?;
        print_progress(
            "binary-complement-arbitrage",
            index + 1,
            markets.len(),
            &totals,
        );
    }
    write_report(
        &config,
        "binary-complement-arbitrage",
        markets.len(),
        totals,
        vec![
            "Fee rate/exponent are configured assumptions because the local dataset does not carry historical per-market fee metadata.",
            "Replay uses the first snapshot at or after the configured latency and does not infer fills between snapshots.",
        ],
    )
}

pub fn run_late_resolution_winner_replay() -> Result<PathBuf> {
    let config = ReplayConfig::from_env()?;
    config.validate()?;
    let connection = open_database(&config)?;
    let markets = load_markets(&connection)?;
    let mut totals = Totals::default();
    for (index, market) in markets.iter().enumerate() {
        let snapshots = load_snapshots(&connection, &market.id)?;
        totals.snapshots += snapshots.len();
        replay_winner_market(&config, market, &snapshots, &mut totals)?;
        print_progress(
            "late-resolution-winner-trading",
            index + 1,
            markets.len(),
            &totals,
        );
    }
    write_report(
        &config,
        "late-resolution-winner-trading",
        markets.len(),
        totals,
        vec![
            "Only snapshots whose TWAP source is chainlink_twap are model observations.",
            "Historical order-book snapshots cannot prove queue position or fills between snapshots.",
            "Fee rate/exponent are configured assumptions because historical fee metadata is absent.",
        ],
    )
}

fn open_database(config: &ReplayConfig) -> Result<Connection> {
    let connection = Connection::open_with_flags(
        &config.database_file,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version != 1 {
        bail!("unsupported PolyBacktest schema version {version}; expected 1");
    }
    Ok(connection)
}

fn load_markets(connection: &Connection) -> Result<Vec<Market>> {
    let mut statement = connection.prepare(
        "SELECT market_id,coin,start_time,end_time,winner FROM markets
         WHERE status='complete' AND start_time IS NOT NULL AND end_time IS NOT NULL
         ORDER BY start_time,market_id",
    )?;
    let markets = statement
        .query_map([], |row| {
            Ok(Market {
                id: row.get(0)?,
                coin: row.get(1)?,
                start_ms: normalize_ms(row.get(2)?),
                end_ms: normalize_ms(row.get(3)?),
                winner: row.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(markets)
}

fn load_snapshots(connection: &Connection, market_id: &str) -> Result<Vec<Snapshot>> {
    let mut statement = connection.prepare(
        "SELECT time,coin_twap,coin_twap_observed_at,coin_twap_source,
                up_best_bid,up_best_ask,up_best_ask_size,
                down_best_bid,down_best_ask,down_best_ask_size,
                orderbook_up_zstd,orderbook_down_zstd
         FROM snapshots WHERE market_id=?1 ORDER BY time,snapshot_id",
    )?;
    let snapshots = statement
        .query_map([market_id], snapshot_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(snapshots)
}

fn snapshot_from_row(row: &Row<'_>) -> rusqlite::Result<Snapshot> {
    Ok(Snapshot {
        time_ms: normalize_ms(row.get(0)?),
        twap: decimal_column(row, 1)?,
        twap_observed_at_ms: row.get::<_, Option<i64>>(2)?.map(normalize_ms),
        twap_source: row.get(3)?,
        up_bid: decimal_column(row, 4)?,
        up_ask: decimal_column(row, 5)?,
        up_ask_size: decimal_column(row, 6)?,
        down_bid: decimal_column(row, 7)?,
        down_ask: decimal_column(row, 8)?,
        down_ask_size: decimal_column(row, 9)?,
        up_book: row.get(10)?,
        down_book: row.get(11)?,
    })
}

fn replay_spread_market(
    config: &ReplayConfig,
    snapshots: &[Snapshot],
    totals: &mut Totals,
) -> Result<()> {
    for signal in snapshots {
        let (Some(up_ask), Some(down_ask)) = (signal.up_ask, signal.down_ask) else {
            continue;
        };
        let signal_sum = up_ask + down_ask;
        if Decimal::ONE - signal_sum < config.spread_min_edge {
            continue;
        }
        totals.candidates += 1;
        let Some(execution) = snapshots.iter().find(|snapshot| {
            snapshot.time_ms >= signal.time_ms.saturating_add(config.execution_delay_ms)
        }) else {
            reject(totals, "no_delayed_snapshot");
            return Ok(());
        };
        let signal_fee_per_share =
            dynamic_fee_per_share(config.fee_rate, config.fee_exponent, up_ask)
                + dynamic_fee_per_share(config.fee_rate, config.fee_exponent, down_ask);
        let shares = (config.order_usdc / (signal_sum + signal_fee_per_share)).round_dp(6);
        let Some(mut up_cost) = walk_asks(
            execution.up_book.as_deref(),
            shares,
            config.liquidity_haircut,
        )?
        else {
            reject(totals, "up_depth_insufficient");
            continue;
        };
        let Some(mut down_cost) = walk_asks(
            execution.down_book.as_deref(),
            shares,
            config.liquidity_haircut,
        )?
        else {
            reject(totals, "down_depth_insufficient");
            continue;
        };
        let mut filled_shares = shares;
        let mut up_average = up_cost / filled_shares;
        let mut down_average = down_cost / filled_shares;
        let mut fees = (dynamic_fee_per_share(config.fee_rate, config.fee_exponent, up_average)
            + dynamic_fee_per_share(config.fee_rate, config.fee_exponent, down_average))
            * filled_shares;
        if up_cost + down_cost + fees > config.order_usdc {
            filled_shares = (filled_shares * config.order_usdc / (up_cost + down_cost + fees)
                * dec!(0.999))
            .round_dp(6);
            let (Some(resized_up_cost), Some(resized_down_cost)) = (
                walk_asks(
                    execution.up_book.as_deref(),
                    filled_shares,
                    config.liquidity_haircut,
                )?,
                walk_asks(
                    execution.down_book.as_deref(),
                    filled_shares,
                    config.liquidity_haircut,
                )?,
            ) else {
                reject(totals, "budget_resize_depth_insufficient");
                continue;
            };
            up_cost = resized_up_cost;
            down_cost = resized_down_cost;
            up_average = up_cost / filled_shares;
            down_average = down_cost / filled_shares;
            fees = (dynamic_fee_per_share(config.fee_rate, config.fee_exponent, up_average)
                + dynamic_fee_per_share(config.fee_rate, config.fee_exponent, down_average))
                * filled_shares;
        }
        if up_cost + down_cost + fees > config.order_usdc
            || up_cost < dec!(1)
            || down_cost < dec!(1)
        {
            reject(totals, "budget_or_minimum_leg_notional");
            continue;
        }
        let gross_pnl = filled_shares - up_cost - down_cost;
        if gross_pnl - fees < config.spread_min_net_profit {
            reject(totals, "net_profit_below_threshold");
            continue;
        }
        totals.admitted += 1;
        totals.fills += 1;
        totals.wins += 1;
        totals.gross_pnl += gross_pnl;
        totals.fees += fees;
        return Ok(()); // one non-overlapping position per five-minute market
    }
    Ok(())
}

fn replay_winner_market(
    config: &ReplayConfig,
    market: &Market,
    snapshots: &[Snapshot],
    totals: &mut Totals,
) -> Result<()> {
    let store = ChainlinkTwapStore::default();
    let mut last_confirmation = None;
    let mut confirmation_direction = None;
    let mut confirmations = 0u32;
    for signal in snapshots {
        if signal.twap_source.as_deref() == Some("chainlink_twap") {
            if let (Some(value), Some(observed_at_ms)) = (signal.twap, signal.twap_observed_at_ms) {
                store.ingest(
                    &market.coin,
                    TwapSample {
                        observed_at_ms,
                        value,
                    },
                );
            }
        }
        let remaining_ms = market.end_ms.saturating_sub(signal.time_ms);
        if remaining_ms < 0 || remaining_ms > config.winner_before_end_secs * 1_000 {
            continue;
        }
        let Some(direction) = detect_winner_asks(
            signal.up_ask,
            signal.down_ask,
            config.winner_min_price,
            config.winner_max_price,
        ) else {
            continue;
        };
        totals.candidates += 1;
        let (bid, ask_size) = if direction.buy_yes {
            (signal.up_bid, signal.up_ask_size)
        } else {
            (signal.down_bid, signal.down_ask_size)
        };
        let (Some(bid), Some(ask_size), Some(observed_at_ms)) =
            (bid, ask_size, signal.twap_observed_at_ms)
        else {
            reject(totals, "missing_bid_depth_or_twap_observation");
            continue;
        };
        let mut winner_policy = config.winner_policy;
        winner_policy.fee_buffer =
            dynamic_fee_per_share(config.fee_rate, config.fee_exponent, direction.price);
        let evaluation = store.evaluate(
            WinnerModelInput {
                asset: &market.coin,
                window_start_ms: market.start_ms,
                now_ms: signal.time_ms,
                seconds_remaining: (remaining_ms / 1_000).max(1),
                buy_yes: direction.buy_yes,
                bid_price: bid,
                ask_price: direction.price,
                ask_size,
            },
            winner_policy,
        );
        if !evaluation.admitted() {
            if let Some(reason) = evaluation.rejection {
                reject(totals, &format!("model_{reason:?}").to_ascii_lowercase());
            }
            confirmations = 0;
            confirmation_direction = None;
            continue;
        }
        totals.admitted += 1;
        if confirmation_direction != Some(direction.buy_yes) {
            confirmations = 0;
            last_confirmation = None;
            confirmation_direction = Some(direction.buy_yes);
        }
        if last_confirmation != Some(observed_at_ms) {
            confirmations += 1;
            last_confirmation = Some(observed_at_ms);
        }
        if confirmations < config.winner_confirmations {
            continue;
        }
        let Some(execution) = snapshots.iter().find(|snapshot| {
            snapshot.time_ms >= signal.time_ms.saturating_add(config.execution_delay_ms)
        }) else {
            reject(totals, "no_delayed_snapshot");
            return Ok(());
        };
        let execution_book = if direction.buy_yes {
            execution.up_book.as_deref()
        } else {
            execution.down_book.as_deref()
        };
        let signal_fee_per_share =
            dynamic_fee_per_share(config.fee_rate, config.fee_exponent, direction.price);
        let mut filled_shares =
            (config.order_usdc / (direction.price + signal_fee_per_share)).round_dp(6);
        let Some(mut cost) = walk_asks(execution_book, filled_shares, config.liquidity_haircut)?
        else {
            reject(totals, "execution_depth_insufficient");
            continue;
        };
        let mut average = cost / filled_shares;
        if average > config.winner_max_price {
            reject(totals, "execution_price_above_limit");
            continue;
        }
        let mut fees =
            dynamic_fee_per_share(config.fee_rate, config.fee_exponent, average) * filled_shares;
        if cost + fees > config.order_usdc {
            filled_shares =
                (filled_shares * config.order_usdc / (cost + fees) * dec!(0.999)).round_dp(6);
            let Some(resized_cost) =
                walk_asks(execution_book, filled_shares, config.liquidity_haircut)?
            else {
                reject(totals, "budget_resize_depth_insufficient");
                continue;
            };
            cost = resized_cost;
            average = cost / filled_shares;
            fees = dynamic_fee_per_share(config.fee_rate, config.fee_exponent, average)
                * filled_shares;
        }
        if cost + fees > config.order_usdc || cost < dec!(1) {
            reject(totals, "budget_or_minimum_order_notional");
            continue;
        }
        let won = winner_matches(market.winner.as_deref(), direction.buy_yes);
        let payout = if won { filled_shares } else { Decimal::ZERO };
        totals.fills += 1;
        totals.wins += usize::from(won);
        totals.losses += usize::from(!won);
        totals.gross_pnl += payout - cost;
        totals.fees += fees;
        return Ok(());
    }
    Ok(())
}

fn walk_asks(
    book_zstd: Option<&[u8]>,
    target: Decimal,
    haircut: Decimal,
) -> Result<Option<Decimal>> {
    let Some(book_zstd) = book_zstd else {
        return Ok(None);
    };
    let decoded = zstd::stream::decode_all(book_zstd).context("decode compressed order book")?;
    let value: Value = serde_json::from_slice(&decoded).context("parse compressed order book")?;
    let Some(levels) = value.get("asks").and_then(Value::as_array) else {
        return Ok(None);
    };
    let mut asks = levels
        .iter()
        .filter_map(|level| {
            Some((
                value_decimal(level.get("price")?)?,
                value_decimal(level.get("size")?)? * haircut,
            ))
        })
        .filter(|(price, size)| *price > Decimal::ZERO && *size > Decimal::ZERO)
        .collect::<Vec<_>>();
    asks.sort_by_key(|(price, _)| *price);
    let mut remaining = target;
    let mut cost = Decimal::ZERO;
    for (price, size) in asks {
        let filled = remaining.min(size);
        cost += filled * price;
        remaining -= filled;
        if remaining <= Decimal::ZERO {
            return Ok(Some(cost));
        }
    }
    Ok(None)
}

fn dynamic_fee_per_share(rate: Decimal, exponent: u64, price: Decimal) -> Decimal {
    let base = price * (Decimal::ONE - price);
    let powered = (0..exponent).fold(Decimal::ONE, |value, _| value * base);
    rate * powered
}

fn winner_matches(winner: Option<&str>, buy_yes: bool) -> bool {
    match winner.map(|value| value.trim().to_ascii_lowercase()) {
        Some(value) if matches!(value.as_str(), "up" | "yes") => buy_yes,
        Some(value) if matches!(value.as_str(), "down" | "no") => !buy_yes,
        _ => false,
    }
}

fn write_report(
    config: &ReplayConfig,
    strategy: &'static str,
    markets: usize,
    totals: Totals,
    limitations: Vec<&'static str>,
) -> Result<PathBuf> {
    let directory = config.output_dir.join(strategy);
    fs::create_dir_all(&directory)?;
    let path = directory.join(format!(
        "run-{}-{}.json",
        Utc::now().format("%Y%m%dT%H%M%SZ"),
        uuid::Uuid::new_v4()
    ));
    let report = ReplayReport {
        strategy,
        generated_at: Utc::now().to_rfc3339(),
        database: config.database_file.display().to_string(),
        markets,
        snapshots: totals.snapshots,
        candidates: totals.candidates,
        admitted: totals.admitted,
        fills: totals.fills,
        wins: totals.wins,
        losses: totals.losses,
        rejections: totals.rejections,
        gross_pnl_usdc: totals.gross_pnl.round_dp(6).to_string(),
        fees_usdc: totals.fees.round_dp(6).to_string(),
        net_pnl_usdc: (totals.gross_pnl - totals.fees).round_dp(6).to_string(),
        output_file: path.display().to_string(),
        limitations,
    };
    let bytes = serde_json::to_vec_pretty(&report)?;
    let mut file = File::create(&path)?;
    file.write_all(&bytes)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(path)
}

fn print_progress(strategy: &str, completed: usize, total: usize, totals: &Totals) {
    let percent = if total == 0 {
        100.0
    } else {
        completed as f64 / total as f64 * 100.0
    };
    println!(
        "{} INFO strategy={} market_progress={}/{} ({:.1}%) candidates={} fills={}",
        Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ"),
        strategy,
        completed,
        total,
        percent,
        totals.candidates,
        totals.fills
    );
}

fn reject(totals: &mut Totals, reason: &str) {
    *totals.rejections.entry(reason.to_string()).or_default() += 1;
}

fn normalize_ms(value: i64) -> i64 {
    if value.abs() < 10_000_000_000 {
        value.saturating_mul(1_000)
    } else {
        value
    }
}

fn decimal_column(row: &Row<'_>, index: usize) -> rusqlite::Result<Option<Decimal>> {
    row.get::<_, Option<String>>(index)
        .map(|value| value.and_then(|value| Decimal::from_str(&value).ok()))
}

fn value_decimal(value: &Value) -> Option<Decimal> {
    match value {
        Value::String(value) => Decimal::from_str(value).ok(),
        Value::Number(value) => Decimal::from_str(&value.to_string()).ok(),
        _ => None,
    }
}

fn env_decimal(name: &str, default: &str) -> Result<Decimal> {
    Decimal::from_str(&std::env::var(name).unwrap_or_else(|_| default.to_string()))
        .with_context(|| format!("{name} must be a decimal"))
}

fn env_parse<T>(name: &str, default: T) -> Result<T>
where
    T: FromStr + std::fmt::Display,
    T::Err: std::error::Error + Send + Sync + 'static,
{
    std::env::var(name)
        .unwrap_or_else(|_| default.to_string())
        .parse()
        .with_context(|| format!("{name} has an invalid value"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fee_and_time_normalization_are_deterministic() {
        assert_eq!(normalize_ms(1_700_000_000), 1_700_000_000_000);
        assert_eq!(normalize_ms(1_700_000_000_123), 1_700_000_000_123);
        assert_eq!(
            dynamic_fee_per_share(dec!(0.25), 2, dec!(0.5)),
            dec!(0.015625)
        );
    }

    #[test]
    fn winner_labels_map_to_binary_side() {
        assert!(winner_matches(Some("Up"), true));
        assert!(winner_matches(Some("NO"), false));
        assert!(!winner_matches(Some("Down"), true));
        assert!(!winner_matches(None, false));
    }
}
