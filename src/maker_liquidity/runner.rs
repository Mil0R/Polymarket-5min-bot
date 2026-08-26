use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use futures::{stream, StreamExt};
use rust_decimal::Decimal;
use serde::Serialize;
use serde_json::json;
use std::collections::BTreeMap;
use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;
use tokio::time::{interval, MissedTickBehavior};

use super::client::MakerResearchClient;
use super::config::MakerResearchConfig;
use super::model::{
    generate_shadow_quote, markout_observation, BookSnapshot, MarketSelection, MarketTrade,
    ShadowMarkout, ShadowQuote,
};

struct ResearchJournal {
    writer: Mutex<File>,
}

impl ResearchJournal {
    fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)?;
        }
        let writer = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            writer: Mutex::new(writer),
        })
    }

    fn record(&self, action: &'static str, data: &impl Serialize) -> Result<()> {
        let event = json!({
            "action": action,
            "version": 1,
            "timestamp": Utc::now(),
            "data": data,
        });
        let mut writer = self.writer.lock().expect("maker journal lock poisoned");
        serde_json::to_writer(&mut *writer, &event)?;
        writer.write_all(b"\n")?;
        writer.flush()?;
        Ok(())
    }
}

struct PendingQuote {
    quote: ShadowQuote,
    observed_horizons: HashSet<u64>,
}

struct MarketState {
    selection: MarketSelection,
    last_quote_at: Option<DateTime<Utc>>,
    pending: Vec<PendingQuote>,
    recent_trades: Vec<MarketTrade>,
}

pub async fn run_market_scan(client: &MakerResearchClient) -> Result<Vec<MarketSelection>> {
    let selected = client.discover().await?;
    print_selections(&selected);
    let journal = ResearchJournal::open(&client.config().results_file)?;
    journal.record(
        "market_scan",
        &json!({
            "selected_count": selected.len(),
            "config": config_summary(client.config()),
        }),
    )?;
    for selection in &selected {
        journal.record("market_selected", selection)?;
    }
    Ok(selected)
}

#[derive(Default)]
struct HorizonSummary {
    observations: u64,
    trade_data_observations: u64,
    touched_legs: u64,
    trade_through_legs: u64,
    fill_proxy_shares: Decimal,
    inventory_markout_usdc: Decimal,
    estimated_rebate_usdc: Decimal,
    combined_usdc: Decimal,
    paired_locked_pnl_usdc: Decimal,
}

pub fn run_report(path: &Path) -> Result<()> {
    let reader = BufReader::new(
        File::open(path).with_context(|| format!("failed to open {}", path.display()))?,
    );
    let mut quotes = 0_u64;
    let mut selected = HashSet::new();
    let mut horizons = BTreeMap::<u64, HorizonSummary>::new();
    for (line_number, line) in reader.lines().enumerate() {
        let line = line.with_context(|| format!("failed reading line {}", line_number + 1))?;
        if line.trim().is_empty() {
            continue;
        }
        let event: serde_json::Value = serde_json::from_str(&line)
            .with_context(|| format!("invalid NDJSON at line {}", line_number + 1))?;
        match event.get("action").and_then(serde_json::Value::as_str) {
            Some("market_selected") => {
                if let Some(id) = event
                    .pointer("/data/condition_id")
                    .and_then(serde_json::Value::as_str)
                {
                    selected.insert(id.to_string());
                }
            }
            Some("shadow_quote") => quotes += 1,
            Some("markout") => {
                let markout: ShadowMarkout = serde_json::from_value(event["data"].clone())
                    .with_context(|| format!("invalid markout at line {}", line_number + 1))?;
                let summary = horizons.entry(markout.target_horizon_secs).or_default();
                summary.observations += 1;
                summary.trade_data_observations += u64::from(markout.trade_data_available);
                summary.touched_legs +=
                    u64::from(markout.yes_touched) + u64::from(markout.no_touched);
                summary.trade_through_legs +=
                    u64::from(markout.yes_trade_through) + u64::from(markout.no_trade_through);
                summary.fill_proxy_shares +=
                    markout.yes_fill_proxy_shares + markout.no_fill_proxy_shares;
                summary.inventory_markout_usdc += markout.inventory_markout_usdc;
                summary.estimated_rebate_usdc += markout.estimated_rebate_usdc;
                summary.combined_usdc += markout.markout_plus_rebate_usdc;
                summary.paired_locked_pnl_usdc +=
                    markout.paired_locked_pnl_usdc.unwrap_or_default();
            }
            _ => {}
        }
    }
    println!("Maker Liquidity Shadow report: {}", path.display());
    println!("markets={} quotes={}", selected.len(), quotes);
    println!(
        "horizon  obs  trade_data  touched_legs  through_legs  proxy_shares  markout  rebate  combined  paired"
    );
    for (horizon, summary) in horizons {
        println!(
            "{:>6}s {:>5} {:>11} {:>13} {:>13} {:>13} {:>8} {:>7} {:>9} {:>7}",
            horizon,
            summary.observations,
            summary.trade_data_observations,
            summary.touched_legs,
            summary.trade_through_legs,
            summary.fill_proxy_shares.round_dp(4),
            summary.inventory_markout_usdc.round_dp(4),
            summary.estimated_rebate_usdc.round_dp(4),
            summary.combined_usdc.round_dp(4),
            summary.paired_locked_pnl_usdc.round_dp(4),
        );
    }
    println!(
        "Note: fill is a public-trade proxy; queue position and Liquidity Rewards allocation are not included."
    );
    Ok(())
}

pub async fn run_shadow_maker(client: MakerResearchClient) -> Result<()> {
    let config = client.config().clone();
    let selections = client.discover().await?;
    if selections.is_empty() {
        bail!("no reward markets passed Maker Liquidity research filters");
    }
    print_selections(&selections);
    let journal = ResearchJournal::open(&config.results_file)?;
    journal.record(
        "run_started",
        &json!({
            "mode": "shadow",
            "selected_count": selections.len(),
            "config": config_summary(&config),
        }),
    )?;
    for selection in &selections {
        journal.record("market_selected", selection)?;
    }
    let mut states = selections
        .into_iter()
        .map(|selection| MarketState {
            selection,
            last_quote_at: None,
            pending: Vec::new(),
            recent_trades: Vec::new(),
        })
        .collect::<Vec<_>>();
    let started_at = Utc::now();
    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);
    let mut ticker = interval(Duration::from_secs(config.book_poll_secs));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut quote_count = 0_u64;
    let mut markout_count = 0_u64;
    let mut error_count = 0_u64;

    loop {
        tokio::select! {
            signal = &mut shutdown => {
                signal.context("failed to listen for Ctrl-C")?;
                break;
            }
            _ = ticker.tick() => {}
        }
        let now = Utc::now();
        if config.duration_secs > 0
            && now.signed_duration_since(started_at).num_seconds()
                >= i64::try_from(config.duration_secs).unwrap_or(i64::MAX)
        {
            break;
        }

        let requests = states.iter().enumerate().map(|(index, state)| {
            let client = client.clone();
            let yes_token = state.selection.market.yes_token_id.clone();
            let no_token = state.selection.market.no_token_id.clone();
            let condition_id = state.selection.market.condition_id.clone();
            async move {
                let (yes, no, trades) = tokio::join!(
                    client.fetch_book(&yes_token),
                    client.fetch_book(&no_token),
                    client.fetch_market_trades(&condition_id)
                );
                (index, yes, no, trades)
            }
        });
        let books = stream::iter(requests)
            .buffer_unordered(config.request_concurrency)
            .collect::<Vec<_>>()
            .await;

        for (index, yes_result, no_result, trades_result) in books {
            let (yes_book, no_book) = match (yes_result, no_result) {
                (Ok(yes), Ok(no)) => (yes, no),
                (yes, no) => {
                    error_count += 1;
                    journal.record(
                        "book_error",
                        &json!({
                            "condition_id": states[index].selection.market.condition_id,
                            "yes_error": yes.err().map(|error| error.to_string()),
                            "no_error": no.err().map(|error| error.to_string()),
                        }),
                    )?;
                    continue;
                }
            };
            let (trades, trade_data_available) = match trades_result {
                Ok(trades) => (trades, true),
                Err(error) => {
                    error_count += 1;
                    journal.record(
                        "trade_data_error",
                        &json!({
                            "condition_id": states[index].selection.market.condition_id,
                            "error": error.to_string(),
                        }),
                    )?;
                    (Vec::new(), false)
                }
            };
            let state = &mut states[index];
            if trade_data_available {
                merge_recent_trades(state, trades, now, &config);
            }
            observe_pending_markouts(
                state,
                &yes_book,
                &no_book,
                trade_data_available,
                &config,
                now,
                &journal,
                &mut markout_count,
                &mut error_count,
            )?;
            let should_quote = state.last_quote_at.is_none_or(|last| {
                now.signed_duration_since(last).num_seconds()
                    >= i64::try_from(config.quote_interval_secs).unwrap_or(i64::MAX)
            });
            if should_quote {
                match generate_shadow_quote(&state.selection, &yes_book, &no_book, &config, now) {
                    Ok(quote) => {
                        journal.record("shadow_quote", &quote)?;
                        tracing::info!(
                            market = %state.selection.market.slug,
                            yes_bid = %quote.yes_bid,
                            no_bid = %quote.no_bid,
                            size = %quote.size_shares,
                            reward_score = %quote.two_sided_reward_score,
                            pair_discount = %quote.complete_set_discount_per_share,
                            "Shadow maker quote"
                        );
                        state.pending.push(PendingQuote {
                            quote,
                            observed_horizons: HashSet::new(),
                        });
                        state.last_quote_at = Some(now);
                        quote_count += 1;
                    }
                    Err(error) => {
                        error_count += 1;
                        journal.record(
                            "quote_rejected",
                            &json!({
                                "condition_id": state.selection.market.condition_id,
                                "error": error.to_string(),
                            }),
                        )?;
                    }
                }
            }
        }
    }

    journal.record(
        "run_stopped",
        &json!({
            "started_at": started_at,
            "stopped_at": Utc::now(),
            "quotes": quote_count,
            "markouts": markout_count,
            "errors": error_count,
            "pending_quotes": states.iter().map(|state| state.pending.len()).sum::<usize>(),
        }),
    )?;
    tracing::info!(
        quote_count,
        markout_count,
        error_count,
        "Shadow Maker stopped"
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn observe_pending_markouts(
    state: &mut MarketState,
    yes_book: &BookSnapshot,
    no_book: &BookSnapshot,
    trade_data_available: bool,
    config: &MakerResearchConfig,
    now: DateTime<Utc>,
    journal: &ResearchJournal,
    markout_count: &mut u64,
    error_count: &mut u64,
) -> Result<()> {
    let recent_trades = &state.recent_trades;
    for pending in &mut state.pending {
        let age_secs = now
            .signed_duration_since(pending.quote.created_at)
            .num_seconds()
            .max(0) as u64;
        for horizon in &config.markout_horizons_secs {
            if age_secs < *horizon || pending.observed_horizons.contains(horizon) {
                continue;
            }
            match markout_observation(
                &pending.quote,
                yes_book,
                no_book,
                recent_trades,
                trade_data_available,
                *horizon,
                now,
            ) {
                Ok(markout) => {
                    journal.record("markout", &markout)?;
                    pending.observed_horizons.insert(*horizon);
                    *markout_count += 1;
                }
                Err(error) => {
                    journal.record(
                        "markout_error",
                        &json!({
                            "quote_id": pending.quote.quote_id,
                            "condition_id": pending.quote.condition_id,
                            "horizon_secs": horizon,
                            "error": error.to_string(),
                        }),
                    )?;
                    pending.observed_horizons.insert(*horizon);
                    *error_count += 1;
                }
            }
        }
    }
    let horizon_count = config.markout_horizons_secs.len();
    state
        .pending
        .retain(|pending| pending.observed_horizons.len() < horizon_count);
    Ok(())
}

fn merge_recent_trades(
    state: &mut MarketState,
    incoming: Vec<MarketTrade>,
    now: DateTime<Utc>,
    config: &MakerResearchConfig,
) {
    let mut known = state
        .recent_trades
        .iter()
        .map(trade_key)
        .collect::<HashSet<_>>();
    for trade in incoming {
        if known.insert(trade_key(&trade)) {
            state.recent_trades.push(trade);
        }
    }
    let retention_secs = config
        .markout_horizons_secs
        .last()
        .copied()
        .unwrap_or_default()
        .saturating_add(config.quote_interval_secs)
        .saturating_add(5);
    let cutoff = now.timestamp() - i64::try_from(retention_secs).unwrap_or(i64::MAX);
    state
        .recent_trades
        .retain(|trade| trade.timestamp >= cutoff);
}

fn trade_key(trade: &MarketTrade) -> String {
    format!(
        "{}|{}|{}|{}|{}|{}",
        trade.transaction_hash, trade.asset, trade.side, trade.price, trade.size, trade.timestamp
    )
}

fn print_selections(selected: &[MarketSelection]) {
    println!(
        "Selected {} Maker Liquidity research markets:",
        selected.len()
    );
    for (index, selection) in selected.iter().enumerate() {
        println!(
            "{:>2}. {:<8} reward=${}/day min={} max_spread={}c liquidity=${} spread={} score={} {}",
            index + 1,
            selection.market.category,
            selection.market.daily_reward_usdc,
            selection.market.rewards_min_size,
            selection.market.rewards_max_spread_cents,
            selection.market.liquidity_usdc,
            selection.market.observed_spread,
            selection.selection_score.round_dp(6),
            selection.market.question,
        );
    }
}

fn config_summary(config: &MakerResearchConfig) -> serde_json::Value {
    json!({
        "max_markets": config.max_markets,
        "min_daily_reward_usdc": config.min_daily_reward_usdc,
        "max_rewards_min_size": config.max_rewards_min_size,
        "min_liquidity_usdc": config.min_liquidity_usdc,
        "max_observed_spread": config.max_observed_spread,
        "midpoint_range": [config.min_midpoint, config.max_midpoint],
        "market_filter": config.market_filter,
        "quote_size_shares": config.quote_size_shares,
        "quote_half_spread": config.quote_half_spread,
        "reward_band_fraction": config.reward_band_fraction,
        "book_poll_secs": config.book_poll_secs,
        "quote_interval_secs": config.quote_interval_secs,
        "markout_horizons_secs": config.markout_horizons_secs,
        "duration_secs": config.duration_secs,
    })
}
