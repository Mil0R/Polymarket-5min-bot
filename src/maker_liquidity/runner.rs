use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use futures::{stream, StreamExt};
use rust_decimal::Decimal;
use serde::Serialize;
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
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
            "version": 2,
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
    active_until: Option<DateTime<Utc>>,
    observed_horizons: HashSet<u64>,
}

struct MarketState {
    selection: MarketSelection,
    last_quote_attempt_at: Option<DateTime<Utc>>,
    consecutive_book_errors: u32,
    consecutive_quote_rejections: u32,
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
    filled_quote_ids: HashSet<String>,
    filled_capital_usdc: Decimal,
}

pub fn run_report(path: &Path) -> Result<()> {
    let reader = BufReader::new(
        File::open(path).with_context(|| format!("failed to open {}", path.display()))?,
    );
    let mut quotes = 0_u64;
    let mut selected = HashSet::new();
    let mut quote_details = HashMap::<String, ShadowQuote>::new();
    let mut cancelled_quotes = 0_u64;
    let mut schema_versions = BTreeSet::<u64>::new();
    let mut visible_reward_share_sum = Decimal::ZERO;
    let mut visible_daily_reward_sum = Decimal::ZERO;
    let mut v2_quote_count = 0_u64;
    let mut horizons = BTreeMap::<u64, HorizonSummary>::new();
    for (line_number, line) in reader.lines().enumerate() {
        let line = line.with_context(|| format!("failed reading line {}", line_number + 1))?;
        if line.trim().is_empty() {
            continue;
        }
        let event: serde_json::Value = serde_json::from_str(&line)
            .with_context(|| format!("invalid NDJSON at line {}", line_number + 1))?;
        if let Some(version) = event.get("version").and_then(serde_json::Value::as_u64) {
            schema_versions.insert(version);
        }
        match event.get("action").and_then(serde_json::Value::as_str) {
            Some("market_selected") => {
                if let Some(id) = event
                    .pointer("/data/condition_id")
                    .and_then(serde_json::Value::as_str)
                {
                    selected.insert(id.to_string());
                }
            }
            Some("shadow_quote") => {
                quotes += 1;
                let quote: ShadowQuote = serde_json::from_value(event["data"].clone())
                    .with_context(|| format!("invalid shadow quote at line {}", line_number + 1))?;
                if event.get("version").and_then(serde_json::Value::as_u64) == Some(2) {
                    visible_reward_share_sum += quote.estimated_visible_reward_share;
                    visible_daily_reward_sum += quote.estimated_daily_liquidity_reward_usdc;
                    v2_quote_count += 1;
                }
                quote_details.insert(quote.quote_id.clone(), quote);
            }
            Some("shadow_quote_cancelled") => cancelled_quotes += 1,
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
                let fill_shares = markout.yes_fill_proxy_shares + markout.no_fill_proxy_shares;
                if fill_shares > Decimal::ZERO {
                    summary.filled_quote_ids.insert(markout.quote_id.clone());
                    if let Some(quote) = quote_details.get(&markout.quote_id) {
                        summary.filled_capital_usdc += quote.yes_bid
                            * markout.yes_fill_proxy_shares
                            + quote.no_bid * markout.no_fill_proxy_shares;
                    }
                }
            }
            _ => {}
        }
    }
    println!("Maker Liquidity Shadow report: {}", path.display());
    println!(
        "markets={} quotes={} cancelled_quotes={} schema_versions={:?}",
        selected.len(),
        quotes,
        cancelled_quotes,
        schema_versions,
    );
    println!(
        "horizon  obs  fills  trade_data  touched  through  proxy_shares  capital  combined  roi_pct  paired"
    );
    for (horizon, summary) in horizons {
        println!(
            "{:>6}s {:>5} {:>6} {:>11} {:>8} {:>8} {:>13} {:>8} {:>9} {:>8} {:>7}",
            horizon,
            summary.observations,
            summary.filled_quote_ids.len(),
            summary.trade_data_observations,
            summary.touched_legs,
            summary.trade_through_legs,
            summary.fill_proxy_shares.round_dp(4),
            summary.filled_capital_usdc.round_dp(4),
            summary.combined_usdc.round_dp(4),
            if summary.filled_capital_usdc > Decimal::ZERO {
                summary.combined_usdc / summary.filled_capital_usdc * Decimal::from(100)
            } else {
                Decimal::ZERO
            }
            .round_dp(4),
            summary.paired_locked_pnl_usdc.round_dp(4),
        );
    }
    if v2_quote_count > 0 {
        let denominator = Decimal::from(v2_quote_count);
        println!(
            "visible_reward_estimate: avg_share={}%, avg_daily_allocation=${}",
            (visible_reward_share_sum / denominator * Decimal::from(100)).round_dp(4),
            (visible_daily_reward_sum / denominator).round_dp(4),
        );
    }
    if schema_versions.contains(&2) {
        println!(
            "Note: v2 fills subtract displayed queue-ahead and enforce quote lifetimes; actual queue changes and Liquidity Rewards allocation remain unavailable."
        );
    }
    if schema_versions.contains(&1) {
        println!(
            "Warning: v1 observations use the legacy optimistic fill proxy and must not be mixed with v2 feasibility decisions."
        );
    }
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
            last_quote_attempt_at: None,
            consecutive_book_errors: 0,
            consecutive_quote_rejections: 0,
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
    let mut last_market_refresh = started_at;
    let mut cooldowns = HashMap::<String, DateTime<Utc>>::new();

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

        for state in &mut states {
            expire_due_quotes(state, now, &config, &journal)?;
        }

        cooldowns.retain(|_, until| *until > now);
        let selected_market_is_cooled = states
            .iter()
            .any(|state| cooldowns.contains_key(&state.selection.market.condition_id));
        if selected_market_is_cooled
            || now.signed_duration_since(last_market_refresh).num_seconds()
                >= i64::try_from(config.market_refresh_secs).unwrap_or(i64::MAX)
        {
            match refresh_market_states(&client, &mut states, &cooldowns, &journal).await {
                Ok(()) => {}
                Err(error) => {
                    error_count += 1;
                    journal.record("market_refresh_error", &json!({"error": error.to_string()}))?;
                }
            }
            last_market_refresh = now;
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
                    states[index].consecutive_book_errors =
                        states[index].consecutive_book_errors.saturating_add(1);
                    let streak = states[index].consecutive_book_errors;
                    if streak == 1 || streak == config.max_consecutive_market_errors {
                        journal.record(
                            "book_error",
                            &json!({
                                "condition_id": states[index].selection.market.condition_id,
                                "yes_error": yes.err().map(|error| error.to_string()),
                                "no_error": no.err().map(|error| error.to_string()),
                                "consecutive_errors": streak,
                                "suppressed_until_threshold": streak == 1,
                            }),
                        )?;
                    }
                    maybe_cool_down_market(
                        &states[index].selection.market.condition_id,
                        streak,
                        "book_error",
                        &config,
                        now,
                        &mut cooldowns,
                        &journal,
                    )?;
                    continue;
                }
            };
            states[index].consecutive_book_errors = 0;
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
            let should_quote = state.last_quote_attempt_at.is_none_or(|last| {
                now.signed_duration_since(last).num_seconds()
                    >= i64::try_from(config.quote_interval_secs).unwrap_or(i64::MAX)
            });
            if should_quote {
                cancel_open_quote(state, now, &journal, "replaced_or_expired")?;
                state.last_quote_attempt_at = Some(now);
                match generate_shadow_quote(&state.selection, &yes_book, &no_book, &config, now) {
                    Ok(quote) => {
                        state.consecutive_quote_rejections = 0;
                        journal.record("shadow_quote", &quote)?;
                        tracing::info!(
                            market = %state.selection.market.slug,
                            yes_bid = %quote.yes_bid,
                            no_bid = %quote.no_bid,
                            size = %quote.size_shares,
                            reward_score = %quote.two_sided_reward_score,
                            visible_reward_share = %quote.estimated_visible_reward_share,
                            estimated_daily_reward = %quote.estimated_daily_liquidity_reward_usdc,
                            pair_discount = %quote.complete_set_discount_per_share,
                            "Shadow maker quote"
                        );
                        state.pending.push(PendingQuote {
                            quote,
                            active_until: None,
                            observed_horizons: HashSet::new(),
                        });
                        quote_count += 1;
                    }
                    Err(error) => {
                        error_count += 1;
                        state.consecutive_quote_rejections =
                            state.consecutive_quote_rejections.saturating_add(1);
                        journal.record(
                            "quote_rejected",
                            &json!({
                                "condition_id": state.selection.market.condition_id,
                                "error": error.to_string(),
                            }),
                        )?;
                        maybe_cool_down_market(
                            &state.selection.market.condition_id,
                            state.consecutive_quote_rejections,
                            "quote_rejected",
                            &config,
                            now,
                            &mut cooldowns,
                            &journal,
                        )?;
                    }
                }
            }
        }
    }

    let stopped_at = Utc::now();
    for state in &mut states {
        cancel_open_quote(state, stopped_at, &journal, "run_stopped")?;
    }
    journal.record(
        "run_stopped",
        &json!({
            "started_at": started_at,
            "stopped_at": stopped_at,
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
                pending.active_until,
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

fn cancel_open_quote(
    state: &mut MarketState,
    now: DateTime<Utc>,
    journal: &ResearchJournal,
    reason: &'static str,
) -> Result<()> {
    if let Some(pending) = state
        .pending
        .iter_mut()
        .rev()
        .find(|pending| pending.active_until.is_none())
    {
        pending.active_until = Some(now);
        journal.record(
            "shadow_quote_cancelled",
            &json!({
                "quote_id": pending.quote.quote_id,
                "condition_id": pending.quote.condition_id,
                "active_until": now,
                "reason": reason,
            }),
        )?;
    }
    Ok(())
}

fn expire_due_quotes(
    state: &mut MarketState,
    now: DateTime<Utc>,
    config: &MakerResearchConfig,
    journal: &ResearchJournal,
) -> Result<()> {
    let lifetime =
        chrono::Duration::seconds(i64::try_from(config.quote_interval_secs).unwrap_or(i64::MAX));
    for pending in &mut state.pending {
        let expires_at = pending.quote.created_at + lifetime;
        if pending.active_until.is_none() && now >= expires_at {
            pending.active_until = Some(expires_at);
            journal.record(
                "shadow_quote_cancelled",
                &json!({
                    "quote_id": pending.quote.quote_id,
                    "condition_id": pending.quote.condition_id,
                    "active_until": expires_at,
                    "reason": "lifetime_expired",
                }),
            )?;
        }
    }
    Ok(())
}

fn maybe_cool_down_market(
    condition_id: &str,
    consecutive_errors: u32,
    reason: &'static str,
    config: &MakerResearchConfig,
    now: DateTime<Utc>,
    cooldowns: &mut HashMap<String, DateTime<Utc>>,
    journal: &ResearchJournal,
) -> Result<()> {
    if consecutive_errors < config.max_consecutive_market_errors {
        return Ok(());
    }
    if cooldowns.contains_key(condition_id) {
        return Ok(());
    }
    let until = now
        + chrono::Duration::seconds(i64::try_from(config.market_cooldown_secs).unwrap_or(i64::MAX));
    cooldowns.insert(condition_id.to_string(), until);
    journal.record(
        "market_cooled_down",
        &json!({
            "condition_id": condition_id,
            "consecutive_errors": consecutive_errors,
            "reason": reason,
            "cooldown_until": until,
        }),
    )
}

async fn refresh_market_states(
    client: &MakerResearchClient,
    states: &mut Vec<MarketState>,
    cooldowns: &HashMap<String, DateTime<Utc>>,
    journal: &ResearchJournal,
) -> Result<()> {
    let excluded = cooldowns.keys().cloned().collect::<HashSet<_>>();
    let selections = client.discover_excluding(&excluded).await?;
    let mut old = states
        .drain(..)
        .map(|state| (state.selection.market.condition_id.clone(), state))
        .collect::<HashMap<_, _>>();
    let mut refreshed = Vec::with_capacity(selections.len());
    for selection in selections {
        let condition_id = selection.market.condition_id.clone();
        if let Some(mut state) = old.remove(&condition_id) {
            state.selection = selection;
            refreshed.push(state);
        } else {
            journal.record("market_selected", &selection)?;
            refreshed.push(MarketState {
                selection,
                last_quote_attempt_at: None,
                consecutive_book_errors: 0,
                consecutive_quote_rejections: 0,
                pending: Vec::new(),
                recent_trades: Vec::new(),
            });
        }
    }
    for (_, mut removed) in old {
        cancel_open_quote(&mut removed, Utc::now(), journal, "market_rotated")?;
        journal.record(
            "market_removed",
            &json!({
                "condition_id": removed.selection.market.condition_id,
                "abandoned_pending_markouts": removed.pending.len(),
            }),
        )?;
    }
    journal.record(
        "market_refresh",
        &json!({
            "active_markets": refreshed.len(),
            "cooldown_markets": cooldowns.len(),
        }),
    )?;
    *states = refreshed;
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
        "market_refresh_secs": config.market_refresh_secs,
        "market_cooldown_secs": config.market_cooldown_secs,
        "max_consecutive_market_errors": config.max_consecutive_market_errors,
        "min_time_to_end_secs": config.min_time_to_end_secs,
        "markout_horizons_secs": config.markout_horizons_secs,
        "duration_secs": config.duration_secs,
    })
}
