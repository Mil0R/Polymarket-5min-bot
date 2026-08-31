use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use rust_decimal::prelude::{FromPrimitive, ToPrimitive};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};

use super::config::MarketMakingResearchConfig;

pub const REWARD_BAND_INELIGIBLE_ERROR: &str =
    "the proposed two-sided quote is outside the reward scoring band";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BookLevel {
    pub price: Decimal,
    pub size: Decimal,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BookSnapshot {
    pub token_id: String,
    pub observed_at: DateTime<Utc>,
    pub exchange_timestamp_ms: Option<i64>,
    pub bids: Vec<BookLevel>,
    pub asks: Vec<BookLevel>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MarketTrade {
    pub asset: String,
    pub side: String,
    pub price: Decimal,
    pub size: Decimal,
    pub timestamp: i64,
    pub transaction_hash: String,
}

impl BookSnapshot {
    pub fn best_bid(&self) -> Option<Decimal> {
        self.bids
            .iter()
            .filter(|level| level.size > Decimal::ZERO)
            .map(|level| level.price)
            .max()
    }

    pub fn best_ask(&self) -> Option<Decimal> {
        self.asks
            .iter()
            .filter(|level| level.size > Decimal::ZERO)
            .map(|level| level.price)
            .min()
    }

    pub fn midpoint(&self) -> Option<Decimal> {
        Some((self.best_bid()? + self.best_ask()?) / dec!(2))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MarketCandidate {
    pub condition_id: String,
    pub question: String,
    pub slug: String,
    pub category: String,
    pub yes_token_id: String,
    pub no_token_id: String,
    pub end_date: Option<DateTime<Utc>>,
    pub rewards_max_spread_cents: Decimal,
    pub rewards_min_size: Decimal,
    pub daily_reward_usdc: Decimal,
    pub liquidity_usdc: Decimal,
    pub volume_24h_usdc: Decimal,
    pub observed_spread: Decimal,
    pub yes_reference_price: Decimal,
    pub tick_size: Decimal,
    pub fees_enabled: bool,
    pub fee_rate: Decimal,
    pub fee_exponent: u32,
    pub maker_rebate_rate: Decimal,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MarketSelection {
    #[serde(flatten)]
    pub market: MarketCandidate,
    pub selection_score: Decimal,
    pub reward_per_required_share: Decimal,
    pub reward_per_liquidity_dollar: Decimal,
    pub estimated_max_rebate_per_100_shares: Decimal,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ShadowQuote {
    pub quote_id: String,
    pub condition_id: String,
    pub created_at: DateTime<Utc>,
    pub yes_token_id: String,
    pub no_token_id: String,
    pub fair_yes: Decimal,
    pub fair_no: Decimal,
    pub yes_bid: Decimal,
    pub no_bid: Decimal,
    pub size_shares: Decimal,
    /// Per-leg sizes after applying the worst-case inventory headroom. These
    /// default to `size_shares` when reading legacy journal rows.
    #[serde(default)]
    pub yes_size_shares: Decimal,
    #[serde(default)]
    pub no_size_shares: Decimal,
    #[serde(default)]
    pub base_quote_half_spread: Decimal,
    #[serde(default = "default_true")]
    pub yes_active: bool,
    #[serde(default = "default_true")]
    pub no_active: bool,
    #[serde(default)]
    pub inventory_net_before: Decimal,
    #[serde(default)]
    pub quote_mode: String,
    /// Resting bid size ahead of the shadow quote at better or equal prices.
    #[serde(default)]
    pub yes_queue_ahead_shares: Decimal,
    #[serde(default)]
    pub no_queue_ahead_shares: Decimal,
    pub yes_distance: Decimal,
    pub no_distance: Decimal,
    pub reward_max_distance: Decimal,
    pub yes_reward_score: Decimal,
    pub no_reward_score: Decimal,
    pub two_sided_reward_score: Decimal,
    /// Approximation based only on currently visible competing bid depth.
    #[serde(default)]
    pub visible_competition_reward_score: Decimal,
    #[serde(default)]
    pub estimated_visible_reward_share: Decimal,
    #[serde(default)]
    pub estimated_daily_liquidity_reward_usdc: Decimal,
    pub quoted_capital_usdc: Decimal,
    pub complete_set_discount_per_share: Decimal,
    pub yes_rebate_if_filled: Decimal,
    pub no_rebate_if_filled: Decimal,
    pub initial_yes_mid: Decimal,
    pub initial_no_mid: Decimal,
}

impl ShadowQuote {
    pub fn effective_yes_size_shares(&self) -> Decimal {
        if self.yes_size_shares > Decimal::ZERO {
            self.yes_size_shares
        } else if self.yes_active {
            self.size_shares
        } else {
            Decimal::ZERO
        }
    }

    pub fn effective_no_size_shares(&self) -> Decimal {
        if self.no_size_shares > Decimal::ZERO {
            self.no_size_shares
        } else if self.no_active {
            self.size_shares
        } else {
            Decimal::ZERO
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ShadowMarkout {
    pub quote_id: String,
    pub condition_id: String,
    pub observed_at: DateTime<Utc>,
    pub target_horizon_secs: u64,
    /// Time at which this quote was replaced/cancelled. Later trades are ignored.
    #[serde(default)]
    pub active_until: Option<DateTime<Utc>>,
    pub actual_age_ms: i64,
    #[serde(default)]
    pub trade_data_available: bool,
    #[serde(default)]
    pub yes_book_crossed: bool,
    #[serde(default)]
    pub no_book_crossed: bool,
    pub yes_touched: bool,
    pub no_touched: bool,
    pub both_touched: bool,
    #[serde(default)]
    pub yes_trade_through: bool,
    #[serde(default)]
    pub no_trade_through: bool,
    #[serde(default)]
    pub yes_fill_proxy_shares: Decimal,
    #[serde(default)]
    pub no_fill_proxy_shares: Decimal,
    pub current_yes_mid: Decimal,
    pub current_no_mid: Decimal,
    /// Best executable bids at the observation time. Unlike midpoint markout,
    /// these are conservative top-of-book exit prices (depth is not assumed).
    #[serde(default)]
    pub current_yes_best_bid: Option<Decimal>,
    #[serde(default)]
    pub current_no_best_bid: Option<Decimal>,
    pub yes_markout_per_share: Option<Decimal>,
    pub no_markout_per_share: Option<Decimal>,
    pub inventory_markout_usdc: Decimal,
    pub estimated_rebate_usdc: Decimal,
    pub markout_plus_rebate_usdc: Decimal,
    pub paired_locked_pnl_usdc: Option<Decimal>,
    /// PnL if filled inventory were immediately sold through visible bid depth.
    /// Any inventory beyond visible depth is conservatively valued at zero.
    #[serde(default)]
    pub executable_exit_pnl_usdc: Option<Decimal>,
    /// Final payout scenarios. The realized settlement PnL must fall between
    /// these values for a valid binary market, before any additional exit cost.
    #[serde(default)]
    pub settlement_pnl_if_yes_usdc: Option<Decimal>,
    #[serde(default)]
    pub settlement_pnl_if_no_usdc: Option<Decimal>,
}

pub fn classify_market(question: &str, slug: &str) -> String {
    let text = format!("{} {}", question, slug).to_ascii_lowercase();
    let tokens = text
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty())
        .collect::<std::collections::HashSet<_>>();
    let has_token = |words: &[&str]| words.iter().any(|word| tokens.contains(word));
    let has_phrase = |phrases: &[&str]| phrases.iter().any(|phrase| text.contains(phrase));
    if has_token(&[
        "bitcoin",
        "ethereum",
        "crypto",
        "btc",
        "eth",
        "solana",
        "xrp",
        "hyperliquid",
    ]) {
        "crypto"
    } else if has_token(&[
        "temperature",
        "weather",
        "rain",
        "snow",
        "hurricane",
        "degrees",
    ]) || has_phrase(&["highest temperature", "lowest temperature"])
    {
        "weather"
    } else if has_token(&["nba", "nfl", "nhl", "mlb", "match", "league", "cup"])
        || has_phrase(&["win the game"])
    {
        "sports"
    } else if has_token(&[
        "election",
        "president",
        "senate",
        "governor",
        "republican",
        "democrat",
    ]) || has_phrase(&["house election"])
    {
        "politics"
    } else if has_token(&["fed", "gdp", "inflation", "stock", "nasdaq", "spx"])
        || has_phrase(&["interest rate", "s&p"])
    {
        "finance"
    } else if has_token(&[
        "ukraine",
        "russia",
        "israel",
        "iran",
        "war",
        "ceasefire",
        "missile",
    ]) || has_phrase(&["target moscow"])
    {
        "geopolitics"
    } else {
        "general"
    }
    .to_string()
}

pub fn rank_candidates(
    candidates: Vec<MarketCandidate>,
    config: &MarketMakingResearchConfig,
) -> Vec<MarketSelection> {
    let mut selected = candidates
        .into_iter()
        .filter(|market| market.daily_reward_usdc >= config.min_daily_reward_usdc)
        .filter(|market| market.rewards_min_size <= config.max_rewards_min_size)
        // If the minimum scoring size is larger than the hard inventory cap,
        // this strategy cannot place even one reward-eligible leg safely.
        .filter(|market| market.rewards_min_size <= config.max_inventory_shares)
        .filter(|market| market.liquidity_usdc >= config.min_liquidity_usdc)
        .filter(|market| {
            let required_volume = if market.fee_rate >= config.high_fee_threshold {
                config.min_volume_24h_usdc * config.high_fee_volume_multiplier
            } else {
                config.min_volume_24h_usdc
            };
            market.volume_24h_usdc >= required_volume
        })
        .filter(|market| market.fee_rate <= config.max_fee_rate)
        .filter(|market| market.observed_spread <= config.max_observed_spread)
        .filter(|market| {
            market.yes_reference_price >= config.min_midpoint
                && market.yes_reference_price <= config.max_midpoint
        })
        .filter(|market| {
            config.market_filter.as_ref().is_none_or(|filter| {
                format!("{} {} {}", market.category, market.question, market.slug)
                    .to_ascii_lowercase()
                    .contains(filter)
            })
        })
        .filter(|market| {
            market.end_date.is_none_or(|end| {
                end.signed_duration_since(Utc::now()).num_seconds()
                    >= i64::try_from(config.min_time_to_end_secs).unwrap_or(i64::MAX)
            })
        })
        .map(|market| {
            let required_shares = market.rewards_min_size.max(config.quote_size_shares);
            let reward_per_required_share = market.daily_reward_usdc / required_shares;
            let liquidity_denominator = market.liquidity_usdc.max(dec!(1));
            let reward_per_liquidity_dollar = market.daily_reward_usdc / liquidity_denominator;
            let activity = Decimal::from_f64(
                (market.volume_24h_usdc.to_f64().unwrap_or_default() + 1.0)
                    .ln_1p()
                    .max(1.0),
            )
            .unwrap_or(dec!(1));
            let competition_penalty =
                dec!(1) + market.liquidity_usdc / (required_shares * dec!(20)).max(dec!(1));
            let selection_score = reward_per_required_share * activity / competition_penalty;
            let estimated_max_rebate_per_100_shares =
                fee_per_share(market.fee_rate, market.fee_exponent, dec!(0.5))
                    * market.maker_rebate_rate
                    * dec!(100);
            MarketSelection {
                market,
                selection_score,
                reward_per_required_share,
                reward_per_liquidity_dollar,
                estimated_max_rebate_per_100_shares,
            }
        })
        .collect::<Vec<_>>();
    selected.sort_by_key(|selection| std::cmp::Reverse(selection.selection_score));
    selected.truncate(config.max_markets);
    selected
}

pub fn generate_shadow_quote(
    selection: &MarketSelection,
    yes_book: &BookSnapshot,
    no_book: &BookSnapshot,
    config: &MarketMakingResearchConfig,
    now: DateTime<Utc>,
) -> Result<ShadowQuote> {
    generate_shadow_quote_controlled(
        selection,
        yes_book,
        no_book,
        config,
        now,
        config.quote_half_spread,
        Decimal::ZERO,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn generate_shadow_quote_controlled(
    selection: &MarketSelection,
    yes_book: &BookSnapshot,
    no_book: &BookSnapshot,
    config: &MarketMakingResearchConfig,
    now: DateTime<Utc>,
    base_quote_half_spread: Decimal,
    inventory_net: Decimal,
) -> Result<ShadowQuote> {
    let (fair_yes, fair_no) = fair_prices(yes_book, no_book)?;
    let yes_mid = yes_book.midpoint().expect("fair_prices validated YES book");
    let no_mid = no_book.midpoint().expect("fair_prices validated NO book");
    let reward_max_distance = selection.market.rewards_max_spread_cents / dec!(100);
    if reward_max_distance <= Decimal::ZERO {
        bail!("market reward distance is zero");
    }
    let book_spread = yes_book
        .best_ask()
        .zip(yes_book.best_bid())
        .map(|(ask, bid)| (ask - bid).max(Decimal::ZERO))
        .into_iter()
        .chain(
            no_book
                .best_ask()
                .zip(no_book.best_bid())
                .map(|(ask, bid)| (ask - bid).max(Decimal::ZERO)),
        )
        .max()
        .unwrap_or_default();
    let dynamic_padding = if config.dynamic_quote_distance {
        book_spread / dec!(4)
    } else {
        Decimal::ZERO
    };
    let desired_distance = (base_quote_half_spread + dynamic_padding)
        .min(reward_max_distance * config.reward_band_fraction);
    let inventory_skew = (inventory_net.abs() * config.inventory_skew_per_share)
        .min(reward_max_distance * dec!(0.4));
    let minimum_distance = selection.market.tick_size.max(dec!(0.001));
    let (yes_distance_target, no_distance_target) = if inventory_net > Decimal::ZERO {
        (
            desired_distance + inventory_skew,
            (desired_distance - inventory_skew).max(minimum_distance),
        )
    } else {
        (
            (desired_distance - inventory_skew).max(minimum_distance),
            desired_distance + inventory_skew,
        )
    };
    let desired_size_shares = config
        .quote_size_shares
        .max(selection.market.rewards_min_size);
    // Cap each leg by the inventory remaining after its own worst-case fill.
    // A YES fill increases net inventory while a NO fill decreases it.
    let yes_size_shares =
        desired_size_shares.min((config.max_inventory_shares - inventory_net).max(Decimal::ZERO));
    let no_size_shares =
        desired_size_shares.min((config.max_inventory_shares + inventory_net).max(Decimal::ZERO));
    let yes_active = yes_size_shares > Decimal::ZERO;
    let no_active = no_size_shares > Decimal::ZERO;
    let tick = selection.market.tick_size.max(dec!(0.001));
    let yes_bid = maker_bid(
        fair_yes,
        yes_distance_target,
        yes_book.best_bid(),
        yes_book.best_ask(),
        tick,
    )?;
    let no_bid = maker_bid(
        fair_no,
        no_distance_target,
        no_book.best_bid(),
        no_book.best_ask(),
        tick,
    )?;
    let yes_queue_ahead_shares = queue_ahead(yes_book, yes_bid);
    let no_queue_ahead_shares = queue_ahead(no_book, no_bid);
    let size_shares = desired_size_shares;
    let yes_distance = (fair_yes - yes_bid).abs();
    let no_distance = (fair_no - no_bid).abs();
    let yes_reward_score = if yes_active && yes_size_shares >= selection.market.rewards_min_size {
        reward_score(reward_max_distance, yes_distance, yes_size_shares)
    } else {
        Decimal::ZERO
    };
    let no_reward_score = if no_active && no_size_shares >= selection.market.rewards_min_size {
        reward_score(reward_max_distance, no_distance, no_size_shares)
    } else {
        Decimal::ZERO
    };
    let two_sided_reward_score = yes_reward_score.min(no_reward_score);
    if (yes_active && yes_distance >= reward_max_distance)
        || (no_active && no_distance >= reward_max_distance)
    {
        bail!(REWARD_BAND_INELIGIBLE_ERROR);
    }
    let visible_yes_competition = visible_reward_score(yes_book, fair_yes, reward_max_distance);
    let visible_no_competition = visible_reward_score(no_book, fair_no, reward_max_distance);
    let visible_competition_reward_score = visible_yes_competition.min(visible_no_competition);
    let estimated_visible_reward_share = if two_sided_reward_score > Decimal::ZERO {
        two_sided_reward_score
            / (two_sided_reward_score + visible_competition_reward_score)
                .max(two_sided_reward_score)
    } else {
        Decimal::ZERO
    };
    let estimated_daily_liquidity_reward_usdc =
        selection.market.daily_reward_usdc * estimated_visible_reward_share;
    let yes_rebate_if_filled = fee_per_share(
        selection.market.fee_rate,
        selection.market.fee_exponent,
        yes_bid,
    ) * selection.market.maker_rebate_rate
        * yes_size_shares;
    let no_rebate_if_filled = fee_per_share(
        selection.market.fee_rate,
        selection.market.fee_exponent,
        no_bid,
    ) * selection.market.maker_rebate_rate
        * no_size_shares;
    Ok(ShadowQuote {
        quote_id: uuid::Uuid::new_v4().to_string(),
        condition_id: selection.market.condition_id.clone(),
        created_at: now,
        yes_token_id: selection.market.yes_token_id.clone(),
        no_token_id: selection.market.no_token_id.clone(),
        fair_yes,
        fair_no,
        yes_bid,
        no_bid,
        size_shares,
        yes_size_shares,
        no_size_shares,
        base_quote_half_spread,
        yes_active,
        no_active,
        inventory_net_before: inventory_net,
        quote_mode: if yes_active && no_active {
            "two-sided"
        } else if yes_active {
            "yes-only"
        } else {
            "no-only"
        }
        .to_string(),
        yes_queue_ahead_shares,
        no_queue_ahead_shares,
        yes_distance,
        no_distance,
        reward_max_distance,
        yes_reward_score,
        no_reward_score,
        two_sided_reward_score,
        visible_competition_reward_score,
        estimated_visible_reward_share,
        estimated_daily_liquidity_reward_usdc,
        quoted_capital_usdc: yes_bid * yes_size_shares + no_bid * no_size_shares,
        complete_set_discount_per_share: dec!(1) - yes_bid - no_bid,
        yes_rebate_if_filled,
        no_rebate_if_filled,
        initial_yes_mid: yes_mid,
        initial_no_mid: no_mid,
    })
}

pub fn fair_prices(yes_book: &BookSnapshot, no_book: &BookSnapshot) -> Result<(Decimal, Decimal)> {
    let yes_mid = yes_book
        .midpoint()
        .ok_or_else(|| anyhow::anyhow!("YES book is not two-sided"))?;
    let no_mid = no_book
        .midpoint()
        .ok_or_else(|| anyhow::anyhow!("NO book is not two-sided"))?;
    let fair_yes = ((yes_mid + dec!(1) - no_mid) / dec!(2)).clamp(dec!(0.001), dec!(0.999));
    Ok((fair_yes, dec!(1) - fair_yes))
}

#[allow(clippy::too_many_arguments)]
pub fn markout_observation(
    quote: &ShadowQuote,
    yes_book: &BookSnapshot,
    no_book: &BookSnapshot,
    trades: &[MarketTrade],
    trade_data_available: bool,
    active_until: Option<DateTime<Utc>>,
    target_horizon_secs: u64,
    now: DateTime<Utc>,
) -> Result<ShadowMarkout> {
    let yes_mid = yes_book
        .midpoint()
        .ok_or_else(|| anyhow::anyhow!("YES markout book is not two-sided"))?;
    let no_mid = no_book
        .midpoint()
        .ok_or_else(|| anyhow::anyhow!("NO markout book is not two-sided"))?;
    let yes_book_crossed =
        quote.yes_active && yes_book.best_ask().is_some_and(|ask| ask <= quote.yes_bid);
    let no_book_crossed =
        quote.no_active && no_book.best_ask().is_some_and(|ask| ask <= quote.no_bid);
    let quote_timestamp = quote.created_at.timestamp();
    let eligible_trades = trades
        .iter()
        // Data API timestamps have one-second precision. Strictly later avoids
        // attributing a print from just before quote creation to the quote.
        .filter(|trade| trade.timestamp > quote_timestamp)
        .filter(|trade| active_until.is_none_or(|until| trade.timestamp <= until.timestamp()))
        .filter(|trade| trade.side.eq_ignore_ascii_case("SELL"));
    let mut yes_volume = Decimal::ZERO;
    let mut no_volume = Decimal::ZERO;
    let mut yes_trade_through = false;
    let mut no_trade_through = false;
    for trade in eligible_trades {
        if quote.yes_active && trade.asset == quote.yes_token_id && trade.price <= quote.yes_bid {
            yes_volume += trade.size;
            yes_trade_through |= trade.price < quote.yes_bid;
        } else if quote.no_active && trade.asset == quote.no_token_id && trade.price <= quote.no_bid
        {
            no_volume += trade.size;
            no_trade_through |= trade.price < quote.no_bid;
        }
    }
    // A new bid sits behind displayed size at equal/better prices. Subtracting
    // it and enforcing the active window avoids optimistic duplicate fills.
    let yes_fill_proxy_shares = (yes_volume - quote.yes_queue_ahead_shares)
        .max(Decimal::ZERO)
        .min(quote.effective_yes_size_shares());
    let no_fill_proxy_shares = (no_volume - quote.no_queue_ahead_shares)
        .max(Decimal::ZERO)
        .min(quote.effective_no_size_shares());
    let yes_touched = yes_book_crossed || yes_fill_proxy_shares > Decimal::ZERO;
    let no_touched = no_book_crossed || no_fill_proxy_shares > Decimal::ZERO;
    let yes_markout = (yes_fill_proxy_shares > Decimal::ZERO).then_some(yes_mid - quote.yes_bid);
    let no_markout = (no_fill_proxy_shares > Decimal::ZERO).then_some(no_mid - quote.no_bid);
    let inventory_markout_usdc = yes_markout.unwrap_or_default() * yes_fill_proxy_shares
        + no_markout.unwrap_or_default() * no_fill_proxy_shares;
    let yes_size_shares = quote.effective_yes_size_shares();
    let no_size_shares = quote.effective_no_size_shares();
    let yes_rebate = if yes_size_shares > Decimal::ZERO {
        quote.yes_rebate_if_filled * yes_fill_proxy_shares / yes_size_shares
    } else {
        Decimal::ZERO
    };
    let no_rebate = if no_size_shares > Decimal::ZERO {
        quote.no_rebate_if_filled * no_fill_proxy_shares / no_size_shares
    } else {
        Decimal::ZERO
    };
    let estimated_rebate_usdc = yes_rebate + no_rebate;
    let purchase_cost_usdc =
        quote.yes_bid * yes_fill_proxy_shares + quote.no_bid * no_fill_proxy_shares;
    let current_yes_best_bid = yes_book.best_bid();
    let current_no_best_bid = no_book.best_bid();
    let has_fill = yes_fill_proxy_shares + no_fill_proxy_shares > Decimal::ZERO;
    let executable_exit_pnl_usdc = has_fill.then(|| {
        executable_sell_proceeds(yes_book, yes_fill_proxy_shares)
            + executable_sell_proceeds(no_book, no_fill_proxy_shares)
            - purchase_cost_usdc
            + estimated_rebate_usdc
    });
    let both_touched = yes_touched && no_touched;
    Ok(ShadowMarkout {
        quote_id: quote.quote_id.clone(),
        condition_id: quote.condition_id.clone(),
        observed_at: now,
        target_horizon_secs,
        active_until,
        actual_age_ms: now
            .signed_duration_since(quote.created_at)
            .num_milliseconds(),
        trade_data_available,
        yes_book_crossed,
        no_book_crossed,
        yes_touched,
        no_touched,
        both_touched,
        yes_trade_through,
        no_trade_through,
        yes_fill_proxy_shares,
        no_fill_proxy_shares,
        current_yes_mid: yes_mid,
        current_no_mid: no_mid,
        current_yes_best_bid,
        current_no_best_bid,
        yes_markout_per_share: yes_markout,
        no_markout_per_share: no_markout,
        inventory_markout_usdc,
        estimated_rebate_usdc,
        markout_plus_rebate_usdc: inventory_markout_usdc + estimated_rebate_usdc,
        paired_locked_pnl_usdc: (yes_fill_proxy_shares > Decimal::ZERO
            && no_fill_proxy_shares > Decimal::ZERO)
            .then_some(
                quote.complete_set_discount_per_share
                    * yes_fill_proxy_shares.min(no_fill_proxy_shares),
            ),
        executable_exit_pnl_usdc,
        settlement_pnl_if_yes_usdc: has_fill
            .then_some(yes_fill_proxy_shares - purchase_cost_usdc + estimated_rebate_usdc),
        settlement_pnl_if_no_usdc: has_fill
            .then_some(no_fill_proxy_shares - purchase_cost_usdc + estimated_rebate_usdc),
    })
}

fn default_true() -> bool {
    true
}

fn queue_ahead(book: &BookSnapshot, quote_bid: Decimal) -> Decimal {
    book.bids
        .iter()
        .filter(|level| level.size > Decimal::ZERO && level.price >= quote_bid)
        .map(|level| level.size)
        .sum()
}

fn executable_sell_proceeds(book: &BookSnapshot, shares: Decimal) -> Decimal {
    let mut levels = book
        .bids
        .iter()
        .filter(|level| level.price > Decimal::ZERO && level.size > Decimal::ZERO)
        .collect::<Vec<_>>();
    levels.sort_by_key(|level| std::cmp::Reverse(level.price));
    let mut remaining = shares.max(Decimal::ZERO);
    let mut proceeds = Decimal::ZERO;
    for level in levels {
        if remaining <= Decimal::ZERO {
            break;
        }
        let filled = remaining.min(level.size);
        proceeds += filled * level.price;
        remaining -= filled;
    }
    proceeds
}

fn maker_bid(
    fair: Decimal,
    distance: Decimal,
    best_bid: Option<Decimal>,
    best_ask: Option<Decimal>,
    tick: Decimal,
) -> Result<Decimal> {
    let ask = best_ask.ok_or_else(|| anyhow::anyhow!("book has no ask"))?;
    let target = round_down_to_tick(fair - distance, tick).max(tick);
    let joined = target.max(best_bid.unwrap_or(tick));
    let post_only_cap = ask - tick;
    if post_only_cap < tick {
        bail!("book spread cannot fit a post-only bid");
    }
    Ok(joined.min(post_only_cap).max(tick))
}

fn round_down_to_tick(value: Decimal, tick: Decimal) -> Decimal {
    if tick <= Decimal::ZERO {
        return value;
    }
    (value / tick).floor() * tick
}

fn reward_score(max_distance: Decimal, distance: Decimal, size: Decimal) -> Decimal {
    if max_distance <= Decimal::ZERO || distance >= max_distance {
        return Decimal::ZERO;
    }
    let ratio = (max_distance - distance) / max_distance;
    ratio * ratio * size
}

fn visible_reward_score(book: &BookSnapshot, fair: Decimal, max_distance: Decimal) -> Decimal {
    book.bids
        .iter()
        .filter(|level| level.size > Decimal::ZERO)
        .map(|level| reward_score(max_distance, (fair - level.price).abs(), level.size))
        .sum()
}

fn fee_per_share(rate: Decimal, exponent: u32, price: Decimal) -> Decimal {
    if rate <= Decimal::ZERO || price <= Decimal::ZERO || price >= dec!(1) {
        return Decimal::ZERO;
    }
    let base = price * (dec!(1) - price);
    let factor = (0..exponent).fold(dec!(1), |value, _| value * base);
    rate * factor
}

#[cfg(test)]
mod tests {
    use super::*;

    fn book(token: &str, bid: Decimal, ask: Decimal) -> BookSnapshot {
        BookSnapshot {
            token_id: token.into(),
            observed_at: Utc::now(),
            exchange_timestamp_ms: None,
            bids: vec![BookLevel {
                price: bid,
                size: dec!(100),
            }],
            asks: vec![BookLevel {
                price: ask,
                size: dec!(100),
            }],
        }
    }

    fn selection() -> MarketSelection {
        MarketSelection {
            market: MarketCandidate {
                condition_id: "0x1".into(),
                question: "test".into(),
                slug: "test".into(),
                category: "general".into(),
                yes_token_id: "1".into(),
                no_token_id: "2".into(),
                end_date: None,
                rewards_max_spread_cents: dec!(3),
                rewards_min_size: dec!(20),
                daily_reward_usdc: dec!(10),
                liquidity_usdc: dec!(100),
                volume_24h_usdc: dec!(1000),
                observed_spread: dec!(0.02),
                yes_reference_price: dec!(0.5),
                tick_size: dec!(0.001),
                fees_enabled: true,
                fee_rate: dec!(0.07),
                fee_exponent: 1,
                maker_rebate_rate: dec!(0.2),
            },
            selection_score: dec!(1),
            reward_per_required_share: dec!(0.5),
            reward_per_liquidity_dollar: dec!(0.1),
            estimated_max_rebate_per_100_shares: dec!(0.35),
        }
    }

    fn config() -> MarketMakingResearchConfig {
        MarketMakingResearchConfig {
            clob_url: "https://clob.test".into(),
            gamma_url: "https://gamma.test".into(),
            data_url: "https://data.test".into(),
            results_file: "events.ndjson".into(),
            reward_audit_file: "reward.ndjson".into(),
            reward_audit_enabled: false,
            reward_poll_secs: 3600,
            max_markets: 5,
            discovery_pages: 1,
            enrichment_limit: 10,
            request_concurrency: 2,
            min_daily_reward_usdc: dec!(1),
            max_rewards_min_size: dec!(100),
            min_liquidity_usdc: dec!(1),
            min_volume_24h_usdc: dec!(1),
            max_fee_rate: dec!(0.1),
            high_fee_threshold: dec!(0.04),
            high_fee_volume_multiplier: dec!(2),
            max_observed_spread: dec!(0.1),
            min_midpoint: dec!(0.1),
            max_midpoint: dec!(0.9),
            market_filter: None,
            quote_size_shares: dec!(20),
            quote_half_spread: dec!(0.01),
            quote_half_spread_grid: vec![dec!(0.01), dec!(0.02)],
            dynamic_quote_distance: false,
            max_midpoint_move: dec!(0.02),
            midpoint_move_window_secs: 30,
            min_trades_1h: 0,
            book_activity_window_secs: 300,
            min_book_updates: 0,
            max_inventory_shares: dec!(40),
            inventory_skew_per_share: dec!(0.00025),
            max_consecutive_negative_markouts: 3,
            max_quotes_per_market_hour: 120,
            reward_band_fraction: dec!(0.8),
            book_poll_secs: 5,
            quote_interval_secs: 30,
            market_refresh_secs: 300,
            min_market_residency_secs: 1800,
            replacement_score_margin: dec!(0.15),
            market_cooldown_secs: 1800,
            max_consecutive_market_errors: 6,
            min_time_to_end_secs: 1800,
            http_max_retries: 3,
            retry_base_ms: 250,
            trade_cache_secs: 5,
            websocket_books: false,
            websocket_book_max_age_ms: 15_000,
            markout_horizons_secs: vec![1, 5, 30, 60, 300, 1800],
            min_independent_fill_samples: 100,
            min_reward_capture_ratio: dec!(0.01),
            target_reward_capture_ratio: dec!(0.05),
            reward_break_even_margin_ratio: dec!(0.02),
            canary_max_markets: 2,
            canary_max_quote_notional_usdc: dec!(5),
            canary_max_inventory_usdc: dec!(10),
            canary_max_midpoint_move: dec!(0.01),
            news_halt_file: "news-halts.txt".into(),
            duration_secs: 0,
        }
    }

    #[test]
    fn quote_is_post_only_two_sided_and_reward_eligible() {
        let quote = generate_shadow_quote(
            &selection(),
            &book("1", dec!(0.48), dec!(0.52)),
            &book("2", dec!(0.48), dec!(0.52)),
            &config(),
            Utc::now(),
        )
        .unwrap();
        assert!(quote.yes_bid < dec!(0.52));
        assert!(quote.no_bid < dec!(0.52));
        assert!(quote.two_sided_reward_score > Decimal::ZERO);
        assert_eq!(quote.size_shares, dec!(20));
        assert_eq!(quote.yes_size_shares, dec!(20));
        assert_eq!(quote.no_size_shares, dec!(20));
        assert_eq!(quote.complete_set_discount_per_share, dec!(0.02));
    }

    #[test]
    fn inventory_limit_switches_to_risk_reducing_one_sided_quote() {
        let config = config();
        let quote = generate_shadow_quote_controlled(
            &selection(),
            &book("1", dec!(0.48), dec!(0.52)),
            &book("2", dec!(0.48), dec!(0.52)),
            &config,
            Utc::now(),
            dec!(0.01),
            config.max_inventory_shares,
        )
        .unwrap();
        assert!(!quote.yes_active);
        assert!(quote.no_active);
        assert_eq!(quote.quote_mode, "no-only");
        assert_eq!(quote.estimated_visible_reward_share, Decimal::ZERO);
    }

    #[test]
    fn inventory_headroom_sizes_each_leg_without_breaching_limit() {
        let config = config();
        let quote = generate_shadow_quote_controlled(
            &selection(),
            &book("1", dec!(0.48), dec!(0.52)),
            &book("2", dec!(0.48), dec!(0.52)),
            &config,
            Utc::now(),
            dec!(0.01),
            dec!(30),
        )
        .unwrap();
        assert_eq!(quote.yes_size_shares, dec!(10));
        assert_eq!(quote.no_size_shares, dec!(20));
        assert!(quote.inventory_net_before + quote.yes_size_shares <= config.max_inventory_shares);
        assert!(quote.inventory_net_before - quote.no_size_shares >= -config.max_inventory_shares);
        assert_eq!(quote.two_sided_reward_score, Decimal::ZERO);
    }

    #[test]
    fn reward_market_larger_than_inventory_cap_is_filtered_out() {
        let mut candidate = selection().market;
        candidate.rewards_min_size = dec!(50);
        assert!(rank_candidates(vec![candidate], &config()).is_empty());
    }

    #[test]
    fn high_fee_markets_require_extra_volume() {
        let mut config = config();
        config.min_volume_24h_usdc = dec!(1000);
        config.high_fee_threshold = dec!(0.04);
        config.high_fee_volume_multiplier = dec!(2);
        let mut candidate = selection().market;
        candidate.volume_24h_usdc = dec!(1500);
        assert!(rank_candidates(vec![candidate.clone()], &config).is_empty());
        candidate.volume_24h_usdc = dec!(2000);
        assert_eq!(rank_candidates(vec![candidate], &config).len(), 1);
    }

    #[test]
    fn executable_exit_walks_visible_depth_and_zero_values_remainder() {
        let snapshot = BookSnapshot {
            token_id: "1".into(),
            observed_at: Utc::now(),
            exchange_timestamp_ms: None,
            bids: vec![
                BookLevel {
                    price: dec!(0.40),
                    size: dec!(2),
                },
                BookLevel {
                    price: dec!(0.45),
                    size: dec!(3),
                },
            ],
            asks: vec![],
        };
        assert_eq!(executable_sell_proceeds(&snapshot, dec!(10)), dec!(2.15));
    }

    #[test]
    fn touched_quotes_record_adverse_markout_and_rebate() {
        let created = Utc::now();
        let quote = generate_shadow_quote(
            &selection(),
            &book("1", dec!(0.48), dec!(0.52)),
            &book("2", dec!(0.48), dec!(0.52)),
            &config(),
            created,
        )
        .unwrap();
        let trades = vec![MarketTrade {
            asset: quote.yes_token_id.clone(),
            side: "SELL".into(),
            price: quote.yes_bid,
            size: dec!(7),
            timestamp: created.timestamp() + 1,
            transaction_hash: "0xtx".into(),
        }];
        let markout = markout_observation(
            &quote,
            &book("1", dec!(0.46), dec!(0.49)),
            &book("2", dec!(0.48), dec!(0.52)),
            &trades,
            true,
            None,
            5,
            created + chrono::Duration::seconds(5),
        )
        .unwrap();
        assert!(markout.yes_touched);
        assert!(!markout.no_touched);
        assert_eq!(markout.yes_fill_proxy_shares, dec!(7));
        assert!(markout.yes_markout_per_share.unwrap() < Decimal::ZERO);
        assert!(markout.estimated_rebate_usdc > Decimal::ZERO);
        assert_eq!(markout.current_yes_best_bid, Some(dec!(0.46)));
        assert!(markout.executable_exit_pnl_usdc.unwrap() < Decimal::ZERO);
        assert!(
            markout.settlement_pnl_if_yes_usdc.unwrap()
                > markout.settlement_pnl_if_no_usdc.unwrap()
        );
    }

    #[test]
    fn queue_aware_fill_requires_volume_beyond_resting_depth() {
        let created = Utc::now();
        let quote = generate_shadow_quote(
            &selection(),
            &book("1", dec!(0.49), dec!(0.51)),
            &book("2", dec!(0.49), dec!(0.51)),
            &config(),
            created,
        )
        .unwrap();
        assert_eq!(quote.yes_queue_ahead_shares, dec!(100));
        let trades = vec![MarketTrade {
            asset: quote.yes_token_id.clone(),
            side: "SELL".into(),
            price: quote.yes_bid,
            size: dec!(107),
            timestamp: created.timestamp() + 1,
            transaction_hash: "0xtx".into(),
        }];
        let markout = markout_observation(
            &quote,
            &book("1", dec!(0.46), dec!(0.49)),
            &book("2", dec!(0.48), dec!(0.52)),
            &trades,
            true,
            None,
            5,
            created + chrono::Duration::seconds(5),
        )
        .unwrap();
        assert_eq!(markout.yes_fill_proxy_shares, dec!(7));
    }

    #[test]
    fn trades_after_quote_cancellation_are_not_attributed() {
        let created = Utc::now();
        let quote = generate_shadow_quote(
            &selection(),
            &book("1", dec!(0.48), dec!(0.52)),
            &book("2", dec!(0.48), dec!(0.52)),
            &config(),
            created,
        )
        .unwrap();
        let trades = vec![MarketTrade {
            asset: quote.yes_token_id.clone(),
            side: "SELL".into(),
            price: quote.yes_bid,
            size: dec!(20),
            timestamp: created.timestamp() + 31,
            transaction_hash: "0xlate".into(),
        }];
        let markout = markout_observation(
            &quote,
            &book("1", dec!(0.46), dec!(0.49)),
            &book("2", dec!(0.48), dec!(0.52)),
            &trades,
            true,
            Some(created + chrono::Duration::seconds(30)),
            60,
            created + chrono::Duration::seconds(60),
        )
        .unwrap();
        assert_eq!(markout.yes_fill_proxy_shares, Decimal::ZERO);
    }

    #[test]
    fn classification_handles_research_categories() {
        assert_eq!(classify_market("Will BTC exceed X?", "btc-x"), "crypto");
        assert_eq!(
            classify_market("Hyperliquid Up or Down?", "hyperliquid-updown"),
            "crypto"
        );
        assert_eq!(
            classify_market("Highest temperature in NYC?", "nyc-weather"),
            "weather"
        );
        assert_eq!(
            classify_market("Will candidate win election?", "vote"),
            "politics"
        );
        assert_eq!(
            classify_market("Will Ukraine target Moscow?", "ukraine-target"),
            "geopolitics"
        );
    }
}
