use alloy::signers::local::LocalSigner;
use alloy::signers::Signer;
use anyhow::Result;
use chrono::Utc;
use polymarket_client_sdk_v2::clob::types::request::{OrderBookSummaryRequest, OrdersRequest};
use polymarket_client_sdk_v2::clob::types::response::PostOrderResponse;
use polymarket_client_sdk_v2::clob::types::{OrderType, Side, SignedOrder};
use polymarket_client_sdk_v2::types::{Decimal, B256, U256};
use polymarket_client_sdk_v2::POLYGON;
use rust_decimal_macros::dec;
use std::collections::HashSet;
use std::str::FromStr;
use std::time::Instant;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use dashmap::DashMap;

use crate::monitor::arbitrage::ArbitrageOpportunity;
use crate::network_retry::{retry_read, ReadRetryPolicy};
use crate::trading::paper::{PaperFill, PaperSettings, PaperSnapshot, PaperTradingEngine};
use crate::trading::AuthenticatedClobClient;

#[derive(Debug)]
pub struct OrderPairResult {
    pub pair_id: String,
    pub yes_order_id: String,
    pub no_order_id: String,
    pub yes_filled: Decimal,
    pub no_filled: Decimal,
    pub yes_size: Decimal,
    pub no_size: Decimal,
    pub yes_price: Decimal,
    pub no_price: Decimal,
    pub success: bool,
    /// At least one account write may have succeeded without a definitive response.
    pub write_uncertain: bool,
}

#[derive(Debug)]
pub struct UncertainWriteError {
    pub message: String,
}

impl std::fmt::Display for UncertainWriteError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "uncertain CLOB write: {}", self.message)
    }
}

impl std::error::Error for UncertainWriteError {}

/// CLOB response amounts are asset-oriented: BUY receives outcome shares as
/// taker amount, while SELL spends outcome shares as maker amount.
pub(crate) fn outcome_shares_filled(
    side: &Side,
    making_amount: Decimal,
    taking_amount: Decimal,
) -> Decimal {
    if matches!(side, Side::Buy) {
        taking_amount
    } else {
        making_amount
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExecutionQuote {
    pub yes_price: Decimal,
    pub no_price: Decimal,
    pub size: Decimal,
    pub total_cost: Decimal,
    pub edge_per_share: Decimal,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WinnerBuyExecution {
    pub requested: Decimal,
    pub filled: Decimal,
    pub cost: Decimal,
    pub average_price: Decimal,
    pub estimated_fee: Decimal,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WinnerBookTop {
    /// Local wall-clock time at which this process received the snapshot.
    pub received_at_ms: i64,
    /// Exchange timestamp for diagnostics. An unchanged book can legitimately
    /// have an old exchange timestamp while a freshly fetched snapshot is valid.
    pub exchange_updated_at_ms: i64,
    pub bid_price: Option<Decimal>,
    pub ask_price: Option<Decimal>,
    pub ask_size: Option<Decimal>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WinnerFeePolicy {
    pub rate: Decimal,
    pub exponent: u32,
}

impl WinnerFeePolicy {
    pub fn fee_per_share(self, price: Decimal) -> Decimal {
        if self.rate <= dec!(0) || price <= dec!(0) || price >= dec!(1) {
            return dec!(0);
        }
        let base = price * (dec!(1) - price);
        let factor = (0..self.exponent).fold(dec!(1), |value, _| value * base);
        self.rate * factor
    }
}

struct QuoteRequest<'a> {
    opportunity: &'a ArbitrageOpportunity,
    yes_slippage: Decimal,
    no_slippage: Decimal,
    max_order_notional: Decimal,
    min_profit_threshold: Decimal,
    execution_spread: Decimal,
    fee_buffer: Decimal,
    yes_tick: Decimal,
    no_tick: Decimal,
}

fn calculate_execution_quote(request: QuoteRequest<'_>) -> Result<ExecutionQuote> {
    let yes_price = round_buy_price_to_tick(
        request.opportunity.yes_ask_price + request.yes_slippage,
        request.yes_tick,
    )?;
    let no_price = round_buy_price_to_tick(
        request.opportunity.no_ask_price + request.no_slippage,
        request.no_tick,
    )?;
    let required_edge =
        request.min_profit_threshold.max(request.execution_spread) + request.fee_buffer;
    let edge_per_share = dec!(1) - yes_price - no_price;
    if edge_per_share < required_edge {
        anyhow::bail!(
            "final quote is not profitable: YES {} + NO {} leaves {}, required {}",
            yes_price,
            no_price,
            edge_per_share,
            required_edge
        );
    }
    let affordable_yes = request.max_order_notional / yes_price;
    let affordable_no = request.max_order_notional / no_price;
    let raw_size = request
        .opportunity
        .yes_size
        .min(request.opportunity.no_size)
        .min(affordable_yes)
        .min(affordable_no);
    let size = (raw_size * dec!(100)).floor() / dec!(100);
    if size <= dec!(0) {
        anyhow::bail!("final order size is zero after notional and precision limits");
    }
    let yes_cost = yes_price * size;
    let no_cost = no_price * size;
    if yes_cost <= dec!(1) || no_cost <= dec!(1) {
        anyhow::bail!(
            "order size below minimum: YES {:.2}, NO {:.2}; both must be > $1",
            yes_cost,
            no_cost
        );
    }
    Ok(ExecutionQuote {
        yes_price,
        no_price,
        size,
        total_cost: yes_cost + no_cost,
        edge_per_share,
    })
}

fn round_buy_price_to_tick(price: Decimal, tick: Decimal) -> Result<Decimal> {
    if tick <= dec!(0) || tick >= dec!(1) {
        anyhow::bail!("invalid market tick size {tick}");
    }
    let bounded = price.clamp(tick, dec!(1) - tick);
    let units = (bounded / tick).ceil();
    Ok((units * tick).min(dec!(1) - tick))
}

pub struct TradingExecutor {
    client: AuthenticatedClobClient,
    private_key: String,
    max_order_size: Decimal,
    slippage: [Decimal; 2], // [first, second]: down uses second, up/flat uses first
    min_profit_threshold: Decimal,
    execution_spread: Decimal,
    fee_buffer: Decimal,
    gtd_expiration_secs: u64,
    arbitrage_order_type: OrderType,
    dry_run: bool,
    paper_winner_buy_delay_ms: u64,
    paper: PaperTradingEngine,
    /// token_id -> minimum tick size, cached to avoid an HTTP round-trip per quote.
    tick_cache: DashMap<U256, Decimal>,
    winner_fee_cache: DashMap<B256, WinnerFeePolicy>,
}

#[derive(Debug, Clone)]
pub struct ExecutionPolicy {
    pub max_order_size_usdc: f64,
    pub slippage: [f64; 2],
    pub min_profit_threshold: f64,
    pub execution_spread: f64,
    pub fee_buffer: f64,
    pub gtd_expiration_secs: u64,
    pub order_type: OrderType,
    pub dry_run: bool,
    pub paper_initial_balance_usdc: f64,
    pub paper_liquidity_haircut: f64,
    pub paper_second_leg_delay_ms: u64,
    pub paper_winner_buy_delay_ms: u64,
}

struct SendErrorContext<'a> {
    pair_id: &'a str,
    yes_price: Decimal,
    no_price: Decimal,
    order_size: Decimal,
    build_sign_elapsed: u128,
    send_start: Instant,
    total_start: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutcomeLeg {
    Yes,
    No,
}

/// The thinner best-ask leg is less likely to absorb the requested size, so it
/// goes first. If both depths are equal, prefer the cheaper residual exposure.
fn harder_leg(opportunity: &ArbitrageOpportunity, quote: &ExecutionQuote) -> OutcomeLeg {
    match opportunity.yes_size.cmp(&opportunity.no_size) {
        std::cmp::Ordering::Less => OutcomeLeg::Yes,
        std::cmp::Ordering::Greater => OutcomeLeg::No,
        std::cmp::Ordering::Equal => {
            if quote.yes_price <= quote.no_price {
                OutcomeLeg::Yes
            } else {
                OutcomeLeg::No
            }
        }
    }
}

fn opposite_leg_size(first_fill: Decimal, requested: Decimal) -> Decimal {
    ((first_fill.min(requested).max(dec!(0))) * dec!(100)).floor() / dec!(100)
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum FirstLegDecision {
    StopUnfilled,
    PauseUncertain,
    RecoverDust,
    SubmitOpposite { size: Decimal },
}

fn decide_after_first_leg(
    confirmed_fill: Decimal,
    frozen: bool,
    requested: Decimal,
    opposite_price: Decimal,
) -> FirstLegDecision {
    if !frozen {
        return FirstLegDecision::PauseUncertain;
    }
    if confirmed_fill <= dec!(0) {
        return FirstLegDecision::StopUnfilled;
    }
    let size = opposite_leg_size(confirmed_fill, requested);
    if size <= dec!(0) || opposite_price * size <= dec!(1) {
        return FirstLegDecision::RecoverDust;
    }
    FirstLegDecision::SubmitOpposite { size }
}

impl TradingExecutor {
    pub fn from_client(
        client: AuthenticatedClobClient,
        private_key: String,
        policy: ExecutionPolicy,
    ) -> Self {
        Self {
            client,
            private_key,
            max_order_size: Decimal::try_from(policy.max_order_size_usdc)
                .unwrap_or(rust_decimal_macros::dec!(100.0)),
            slippage: [
                Decimal::try_from(policy.slippage[0]).unwrap_or(dec!(0.0)),
                Decimal::try_from(policy.slippage[1]).unwrap_or(dec!(0.01)),
            ],
            min_profit_threshold: Decimal::try_from(policy.min_profit_threshold)
                .unwrap_or(dec!(0.001)),
            execution_spread: Decimal::try_from(policy.execution_spread).unwrap_or(dec!(0.01)),
            fee_buffer: Decimal::try_from(policy.fee_buffer).unwrap_or(dec!(0.002)),
            gtd_expiration_secs: policy.gtd_expiration_secs,
            arbitrage_order_type: policy.order_type,
            dry_run: policy.dry_run,
            paper_winner_buy_delay_ms: policy.paper_winner_buy_delay_ms,
            paper: PaperTradingEngine::new(PaperSettings {
                initial_balance: Decimal::try_from(policy.paper_initial_balance_usdc)
                    .unwrap_or(dec!(50)),
                liquidity_haircut: Decimal::try_from(policy.paper_liquidity_haircut)
                    .unwrap_or(dec!(0.75)),
                second_leg_delay_ms: policy.paper_second_leg_delay_ms,
            }),
            tick_cache: DashMap::new(),
            winner_fee_cache: DashMap::new(),
        }
    }

    /// Verify auth actually succeeded via api_keys()
    pub async fn verify_authentication(&self) -> Result<()> {
        self.client
            .api_keys()
            .await
            .map_err(|e| anyhow::anyhow!("Auth verification failed: API error: {}", e))?;
        Ok(())
    }

    pub async fn winner_book_top(&self, token_id: U256) -> Result<Option<WinnerBookTop>> {
        let book = self.live_paper_book(token_id).await?;
        let bid = book
            .bids
            .iter()
            .filter(|level| level.size > dec!(0))
            .max_by_key(|level| level.price);
        let ask = book
            .asks
            .iter()
            .filter(|level| level.size > dec!(0))
            .min_by_key(|level| level.price);
        if bid.is_none() && ask.is_none() {
            return Ok(None);
        }
        Ok(Some(WinnerBookTop {
            received_at_ms: Utc::now().timestamp_millis(),
            exchange_updated_at_ms: book.timestamp.timestamp_millis(),
            bid_price: bid.map(|level| level.price),
            ask_price: ask.map(|level| level.price),
            ask_size: ask.map(|level| level.size),
        }))
    }

    pub async fn winner_fee_policy(&self, condition_id: B256) -> Result<WinnerFeePolicy> {
        if let Some(cached) = self.winner_fee_cache.get(&condition_id) {
            return Ok(*cached);
        }
        let condition = condition_id.to_string();
        let info = retry_read(ReadRetryPolicy::default(), || async {
            self.client
                .clob_market_info(&condition)
                .await
                .map_err(anyhow::Error::from)
        })
        .await?;
        let details = info.fee_details.ok_or_else(|| {
            anyhow::anyhow!("CLOB V2 fee_details missing for Winner Buy condition {condition_id}")
        })?;
        let policy = WinnerFeePolicy {
            rate: details.rate,
            exponent: details.exponent,
        };
        self.winner_fee_cache.insert(condition_id, policy);
        Ok(policy)
    }

    /// Conservative cash/risk reservation for a Winner Buy. Live fills use
    /// exchange-reported amounts; paper fills use the configured fee buffer.
    pub fn winner_planned_cost(
        &self,
        limit_price: Decimal,
        size: Decimal,
        fee_per_share: Decimal,
    ) -> Decimal {
        (limit_price + fee_per_share).min(dec!(1)) * size
    }

    /// Single-leg winner-candidate buy for the window-end strategy.
    /// Dry-run mode simulates a fill against the live CLOB book; live mode posts
    /// a real buy order. Paper cash is reserved before the live-book simulation.
    pub async fn buy_winner(
        &self,
        token_id: U256,
        limit_price: Decimal,
        size: Decimal,
        fee_per_share: Decimal,
    ) -> Result<WinnerBuyExecution> {
        if self.dry_run {
            let reservation =
                self.paper
                    .reserve(self.winner_planned_cost(limit_price, size, fee_per_share))?;
            if self.paper_winner_buy_delay_ms > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(
                    self.paper_winner_buy_delay_ms,
                ))
                .await;
            }
            let book = self.live_paper_book(token_id).await?;
            let mut fill =
                self.paper
                    .simulate_buy(&book.asks, limit_price, size, &self.arbitrage_order_type);
            let estimated_fee = fill.filled * fee_per_share;
            if fill.filled > dec!(0) {
                fill.cost += estimated_fee;
                fill.average_price = fill.cost / fill.filled;
            }
            let paper = reservation.commit_winner(fill);
            info!(target: "polypulse::strategy::winner_buy",
                filled = %fill.filled,
                average_price = %fill.average_price,
                cost = %fill.cost,
                cash_remaining = %paper.cash_remaining,
                winner_positions_open = paper.winner_positions_open,
                "PAPER winner-buy cash committed"
            );
            return Ok(WinnerBuyExecution {
                requested: size,
                filled: fill.filled,
                cost: fill.cost,
                average_price: fill.average_price,
                estimated_fee,
            });
        }
        let expiration = Utc::now() + chrono::Duration::seconds(self.gtd_expiration_secs as i64);
        let signed = self
            .build_and_sign_buy(token_id, limit_price, size, expiration)
            .await?;
        let response = self.client.post_order(signed).await?;
        let filled =
            outcome_shares_filled(&Side::Buy, response.making_amount, response.taking_amount);
        let cost = if filled > dec!(0) {
            response.making_amount.max(dec!(0))
        } else {
            dec!(0)
        };
        let average_price = if filled > dec!(0) && cost > dec!(0) {
            cost / filled
        } else if filled > dec!(0) {
            limit_price
        } else {
            dec!(0)
        };
        Ok(WinnerBuyExecution {
            requested: size,
            filled,
            cost: if cost > dec!(0) {
                cost
            } else {
                average_price * filled
            },
            average_price,
            estimated_fee: dec!(0),
        })
    }

    pub fn restore_paper_winner_history(
        &self,
        pending_cost: Decimal,
        total_spent: Decimal,
        realized_pnl: Decimal,
        open_positions: u64,
    ) -> Option<PaperSnapshot> {
        self.dry_run.then(|| {
            self.paper.restore_winner_history(
                pending_cost,
                total_spent,
                realized_pnl,
                open_positions,
            )
        })
    }

    pub fn settle_paper_winner(&self, cost: Decimal, payout: Decimal) -> Option<PaperSnapshot> {
        self.dry_run.then(|| self.paper.settle_winner(cost, payout))
    }

    /// Cancel only active orders whose token belongs to the target window.
    pub async fn cancel_orders_for_tokens(&self, tokens: &HashSet<U256>) -> Result<usize> {
        let mut cursor = None;
        let mut ids = Vec::new();
        loop {
            let page = self
                .client
                .orders(&OrdersRequest::default(), cursor)
                .await?;
            ids.extend(
                page.data
                    .into_iter()
                    .filter(|order| tokens.contains(&order.asset_id))
                    .map(|order| order.id),
            );
            if page.next_cursor.is_empty() || page.next_cursor == "LTE=" {
                break;
            }
            cursor = Some(page.next_cursor);
        }
        if ids.is_empty() {
            return Ok(0);
        }
        let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
        self.client.cancel_orders(&refs).await?;
        Ok(ids.len())
    }

    /// Place GTC sell at given price (wind-down: market-intent for one-sided leg)
    pub async fn sell_at_price(
        &self,
        token_id: U256,
        price: Decimal,
        size: Decimal,
    ) -> Result<PostOrderResponse> {
        let signer = LocalSigner::from_str(&self.private_key)?.with_chain_id(Some(POLYGON));
        let order = self
            .client
            .limit_order()
            .token_id(token_id)
            .side(Side::Sell)
            .price(price)
            .size(size)
            .order_type(OrderType::GTC)
            .build()
            .await?;
        let signed = self.client.sign(&signer, order).await?;
        self.client
            .post_order(signed)
            .await
            .map_err(|e| anyhow::anyhow!("Sell order submit failed: {}", e))
    }

    /// Slippage by direction: down(↓) uses second, up(↑) and flat(−/empty) use first
    fn slippage_for_direction(&self, dir: &str) -> Decimal {
        if dir == "↓" {
            self.slippage[1]
        } else {
            self.slippage[0]
        }
    }

    /// Build the exact prices and size that will be sent to CLOB. The profitability
    /// check deliberately happens after slippage and includes a fee/rounding buffer.
    pub async fn execution_quote(
        &self,
        opp: &ArbitrageOpportunity,
        yes_dir: &str,
        no_dir: &str,
    ) -> Result<ExecutionQuote> {
        let yes_token = U256::from_str(&opp.yes_token_id.to_string())?;
        let no_token = U256::from_str(&opp.no_token_id.to_string())?;
        let (yes_tick, no_tick) = tokio::join!(
            self.cached_tick_size(yes_token),
            self.cached_tick_size(no_token)
        );
        let yes_tick = yes_tick?;
        let no_tick = no_tick?;
        calculate_execution_quote(QuoteRequest {
            opportunity: opp,
            yes_slippage: self.slippage_for_direction(yes_dir),
            no_slippage: self.slippage_for_direction(no_dir),
            max_order_notional: self.max_order_size,
            min_profit_threshold: self.min_profit_threshold,
            execution_spread: self.execution_spread,
            fee_buffer: self.fee_buffer,
            yes_tick,
            no_tick,
        })
    }

    /// Minimum tick size for a token, cached per token to skip the HTTP round-trip
    /// on every quote. Tick size does not change within a 5-minute window.
    async fn cached_tick_size(&self, token_id: U256) -> Result<Decimal> {
        if let Some(tick) = self.tick_cache.get(&token_id) {
            return Ok(*tick);
        }
        let response = self.client.tick_size(token_id).await?;
        let tick = Decimal::from(response.minimum_tick_size);
        self.tick_cache.insert(token_id, tick);
        Ok(tick)
    }

    async fn build_and_sign_buy(
        &self,
        token_id: U256,
        price: Decimal,
        size: Decimal,
        expiration: chrono::DateTime<Utc>,
    ) -> Result<SignedOrder> {
        let builder = self
            .client
            .limit_order()
            .token_id(token_id)
            .side(Side::Buy)
            .price(price)
            .size(size)
            .order_type(self.arbitrage_order_type.clone());
        let order = if matches!(self.arbitrage_order_type, OrderType::GTD) {
            builder.expiration(expiration).build().await?
        } else {
            builder.build().await?
        };
        let signer = LocalSigner::from_str(&self.private_key)?.with_chain_id(Some(POLYGON));
        Ok(self.client.sign(&signer, order).await?)
    }

    /// Cancel any resting remainder before using the observed fill to size the
    /// opposite leg. `frozen=false` means the first leg can still grow and the
    /// second leg must not be submitted.
    async fn freeze_and_reconcile_fill(
        &self,
        leg: OutcomeLeg,
        response: &PostOrderResponse,
        requested: Decimal,
    ) -> (Decimal, bool) {
        let observed =
            outcome_shares_filled(&Side::Buy, response.making_amount, response.taking_amount);
        let mut frozen = true;
        let remainder_is_auto_cancelled =
            matches!(&self.arbitrage_order_type, OrderType::FOK | OrderType::FAK);
        if observed < requested && !response.order_id.is_empty() && !remainder_is_auto_cancelled {
            if let Err(error) = self.client.cancel_order(&response.order_id).await {
                frozen = false;
                warn!(
                    ?leg,
                    order_id = %response.order_id,
                    %error,
                    "Could not freeze order remainder"
                );
            }
        }
        let cumulative = if response.order_id.is_empty() {
            observed
        } else {
            match self.client.order(&response.order_id).await {
                Ok(order) => order.size_matched.max(observed),
                Err(error) => {
                    // A failed reconciliation means the fill quantity is not
                    // trustworthy enough to size another write request.
                    frozen = false;
                    warn!(
                        ?leg,
                        order_id = %response.order_id,
                        %error,
                        "Could not reconcile cumulative fill"
                    );
                    observed
                }
            }
        };
        (cumulative, frozen)
    }

    async fn live_paper_book(
        &self,
        token_id: U256,
    ) -> Result<polymarket_client_sdk_v2::clob::types::response::OrderBookSummaryResponse> {
        let request = OrderBookSummaryRequest::builder()
            .token_id(token_id)
            .build();
        retry_read(ReadRetryPolicy::default(), || async {
            self.client
                .order_book(&request)
                .await
                .map_err(anyhow::Error::from)
        })
        .await
    }

    async fn execute_paper_pair(
        &self,
        opp: &ArbitrageOpportunity,
        quote: &ExecutionQuote,
        yes_token_id: U256,
        no_token_id: U256,
        pair_id: String,
    ) -> Result<OrderPairResult> {
        let paper_reservation = self.paper.reserve(quote.total_cost)?;
        let first_leg = harder_leg(opp, quote);
        let (first_token, first_limit, second_token, second_limit) = match first_leg {
            OutcomeLeg::Yes => (yes_token_id, quote.yes_price, no_token_id, quote.no_price),
            OutcomeLeg::No => (no_token_id, quote.no_price, yes_token_id, quote.yes_price),
        };

        // Pre-flight: confirm BOTH legs still have takable ask liquidity at the
        // limit prices before committing the first leg. Phantom signals (guard
        // quotes that vanish on touch) are the main source of one-sided fills:
        // the WS snapshot saw both asks, but by execution time the losing side
        // has no sellers. Abort cleanly here instead of eating one-sided exposure.
        let (first_book_pre, second_book_pre) = tokio::join!(
            self.live_paper_book(first_token),
            self.live_paper_book(second_token),
        );
        let (first_book_pre, second_book_pre) = (first_book_pre?, second_book_pre?);
        let first_pre = self.paper.simulate_buy(
            &first_book_pre.asks,
            first_limit,
            quote.size,
            &self.arbitrage_order_type,
        );
        let second_pre = self.paper.simulate_buy(
            &second_book_pre.asks,
            second_limit,
            quote.size,
            &self.arbitrage_order_type,
        );
        if first_pre.filled <= dec!(0) || second_pre.filled <= dec!(0) {
            warn!(
                ?first_leg,
                first_fill = %first_pre.filled,
                second_fill = %second_pre.filled,
                "PAPER pre-flight: opportunity evaporated before execution; skipping to avoid one-sided exposure"
            );
            return Err(anyhow::anyhow!(
                "opportunity disappeared before execution (pre-flight depth check)"
            ));
        }

        // First leg consumes the pre-flight book so the simulated fill matches
        // what the pre-flight check approved.
        let first_fill = self.paper.simulate_buy(
            &first_book_pre.asks,
            first_limit,
            quote.size,
            &self.arbitrage_order_type,
        );
        let second_fill = if first_fill.filled > dec!(0) {
            tokio::time::sleep(std::time::Duration::from_millis(
                self.paper.second_leg_delay_ms(),
            ))
            .await;
            match self.live_paper_book(second_token).await {
                Ok(second_book) => self.paper.simulate_buy(
                    &second_book.asks,
                    second_limit,
                    first_fill.filled,
                    &self.arbitrage_order_type,
                ),
                Err(error) => {
                    warn!(%error, "PAPER opposite book refresh failed; recording one-sided virtual fill");
                    PaperFill {
                        requested: first_fill.filled,
                        filled: dec!(0),
                        cost: dec!(0),
                        average_price: dec!(0),
                    }
                }
            }
        } else {
            PaperFill {
                requested: dec!(0),
                filled: dec!(0),
                cost: dec!(0),
                average_price: dec!(0),
            }
        };

        let (yes_fill, no_fill) = match first_leg {
            OutcomeLeg::Yes => (first_fill, second_fill),
            OutcomeLeg::No => (second_fill, first_fill),
        };
        let paper_snapshot = paper_reservation.commit(yes_fill, no_fill);
        let paired = yes_fill.filled.min(no_fill.filled);
        let target_size = first_fill.filled.max(quote.size.min(dec!(0.01)));
        let first_order_id = format!("paper-{:?}-{}", first_leg, Uuid::new_v4());
        let second_order_id = if first_fill.filled > dec!(0) {
            format!("paper-opposite-{}", Uuid::new_v4())
        } else {
            String::new()
        };
        let (yes_order_id, no_order_id) = match first_leg {
            OutcomeLeg::Yes => (first_order_id, second_order_id),
            OutcomeLeg::No => (second_order_id, first_order_id),
        };
        info!(
            ?first_leg,
            yes_filled = %yes_fill.filled,
            no_filled = %no_fill.filled,
            yes_vwap = %yes_fill.average_price,
            no_vwap = %no_fill.average_price,
            paired = %paired,
            cash_remaining = %paper_snapshot.cash_remaining,
            paper_pnl = %paper_snapshot.theoretical_pnl,
            unmatched_cost = %paper_snapshot.unmatched_cost,
            "🧪 PAPER live-book execution"
        );

        Ok(OrderPairResult {
            pair_id,
            yes_order_id,
            no_order_id,
            yes_filled: yes_fill.filled,
            no_filled: no_fill.filled,
            yes_size: target_size,
            no_size: target_size,
            yes_price: if yes_fill.filled > dec!(0) {
                yes_fill.average_price
            } else {
                quote.yes_price
            },
            no_price: if no_fill.filled > dec!(0) {
                no_fill.average_price
            } else {
                quote.no_price
            },
            success: paired > dec!(0) && yes_fill.filled == no_fill.filled,
            write_uncertain: false,
        })
    }

    /// Execute arbitrage: submit YES+NO via sequential post_order (V2); order type from config
    /// yes_dir / no_dir: direction "↑" "↓" "−" or "" for slippage (down=second, up/flat=first)
    pub async fn execute_arbitrage_pair(
        &self,
        opp: &ArbitrageOpportunity,
        quote: &ExecutionQuote,
    ) -> Result<OrderPairResult> {
        let total_start = Instant::now();

        let expiry_info = if matches!(self.arbitrage_order_type, OrderType::GTD) {
            format!("expiry:{}s", self.gtd_expiration_secs)
        } else {
            "no expiry".to_string()
        };
        debug!(
            market_id = %opp.market_id,
            profit_pct = %opp.profit_percentage,
            order_type = %self.arbitrage_order_type,
            "Execute arbitrage (V2 sequential, type:{}, {})",
            self.arbitrage_order_type,
            expiry_info
        );

        let yes_token_id = U256::from_str(&opp.yes_token_id.to_string())?;
        let no_token_id = U256::from_str(&opp.no_token_id.to_string())?;

        let order_size = quote.size;
        let pair_id = Uuid::new_v4().to_string();
        let expiration = Utc::now() + chrono::Duration::seconds(self.gtd_expiration_secs as i64);

        let yes_price_with_slippage = quote.yes_price;
        let no_price_with_slippage = quote.no_price;

        if self.dry_run {
            return self
                .execute_paper_pair(opp, quote, yes_token_id, no_token_id, pair_id)
                .await;
        }

        info!(
            "📋 Level | YES {:.4}×{:.2} NO {:.4}×{:.2}",
            yes_price_with_slippage, order_size, no_price_with_slippage, order_size
        );

        let expiry_suffix = if matches!(self.arbitrage_order_type, OrderType::GTD) {
            format!(" | GTD {}s", self.gtd_expiration_secs)
        } else {
            String::new()
        };
        info!(
            "📤 Order | YES {:.4}→{:.4}×{} NO {:.4}→{:.4}×{} | {}{}",
            opp.yes_ask_price,
            yes_price_with_slippage,
            order_size,
            opp.no_ask_price,
            no_price_with_slippage,
            order_size,
            self.arbitrage_order_type,
            expiry_suffix
        );

        let yes_amount_usd = yes_price_with_slippage * order_size;
        let no_amount_usd = no_price_with_slippage * order_size;
        if yes_amount_usd <= dec!(1) || no_amount_usd <= dec!(1) {
            warn!(
                "⏭️ Skip order | YES:{:.2} pUSD NO:{:.2} pUSD | both must be > $1",
                yes_amount_usd, no_amount_usd
            );
            return Err(anyhow::anyhow!(
                "Order size below min: YES {:.2} pUSD, NO {:.2} pUSD; both must be > $1",
                yes_amount_usd,
                no_amount_usd
            ));
        }

        let first_leg = harder_leg(opp, quote);
        let (first_token, first_price, second_token, second_price) = match first_leg {
            OutcomeLeg::Yes => (
                yes_token_id,
                yes_price_with_slippage,
                no_token_id,
                no_price_with_slippage,
            ),
            OutcomeLeg::No => (
                no_token_id,
                no_price_with_slippage,
                yes_token_id,
                yes_price_with_slippage,
            ),
        };
        info!(
            ?first_leg,
            yes_depth = %opp.yes_size,
            no_depth = %opp.no_size,
            "Submitting thinner best-ask leg first"
        );

        // Pre-flight: confirm both legs still have takable ask depth at the limit
        // prices before submitting anything. The WS snapshot that created this
        // opportunity can be stale by hundreds of ms; on 5-min markets the losing
        // side often has no real sellers, so submitting the first leg would leave
        // one-sided exposure. Fail closed: if the live book cannot be fetched or
        // either leg has no depth inside the limit, do not write any order.
        let (first_book_pre, second_book_pre) = tokio::join!(
            self.live_paper_book(first_token),
            self.live_paper_book(second_token),
        );
        let (first_book_pre, second_book_pre) = match (first_book_pre, second_book_pre) {
            (Ok(first), Ok(second)) => (first, second),
            (Err(first_error), Err(second_error)) => {
                warn!(
                    %first_error,
                    %second_error,
                    "Pre-flight book queries failed; aborting to avoid one-sided exposure"
                );
                return Err(anyhow::anyhow!("pre-flight book queries failed; aborting"));
            }
            (Err(error), _) | (_, Err(error)) => {
                warn!(
                    %error,
                    "Pre-flight book query failed; aborting to avoid one-sided exposure"
                );
                return Err(anyhow::anyhow!("pre-flight book query failed; aborting"));
            }
        };
        let first_depth: Decimal = first_book_pre
            .asks
            .iter()
            .take_while(|level| level.price <= first_price)
            .map(|level| level.price * level.size)
            .sum();
        let second_depth: Decimal = second_book_pre
            .asks
            .iter()
            .take_while(|level| level.price <= second_price)
            .map(|level| level.price * level.size)
            .sum();
        if first_depth <= dec!(0) || second_depth <= dec!(0) {
            warn!(
                ?first_leg,
                %first_depth,
                %second_depth,
                "Pre-flight: opposite leg liquidity gone; skipping to avoid one-sided exposure"
            );
            return Err(anyhow::anyhow!(
                "opportunity disappeared before execution (pre-flight depth check)"
            ));
        }

        let build_start = Instant::now();
        let signed_first = self
            .build_and_sign_buy(first_token, first_price, order_size, expiration)
            .await?;
        let build_sign_elapsed = build_start.elapsed().as_millis();
        let send_start = Instant::now();
        let first_result = match self.client.post_order(signed_first).await {
            Ok(response) => response,
            Err(error) => {
                // The request outcome is ambiguous after a transport error. Never
                // add the opposite leg. Best-effort cancellation reduces the chance
                // of leaving an accepted-but-unacknowledged order resting.
                let tokens = HashSet::from([yes_token_id, no_token_id]);
                if let Err(cancel_error) = self.cancel_orders_for_tokens(&tokens).await {
                    warn!(%cancel_error, "Uncertain first write; token-scoped cancellation failed");
                }
                return Err(Self::log_send_error(
                    SendErrorContext {
                        pair_id: &pair_id,
                        yes_price: yes_price_with_slippage,
                        no_price: no_price_with_slippage,
                        order_size,
                        build_sign_elapsed,
                        send_start,
                        total_start,
                    },
                    error,
                ));
            }
        };

        let (first_filled, first_frozen) = self
            .freeze_and_reconcile_fill(first_leg, &first_result, order_size)
            .await;
        let first_order_id = first_result.order_id.clone();
        let first_only_result = |filled: Decimal, write_uncertain: bool| OrderPairResult {
            pair_id: pair_id.clone(),
            yes_order_id: if first_leg == OutcomeLeg::Yes {
                first_order_id.clone()
            } else {
                String::new()
            },
            no_order_id: if first_leg == OutcomeLeg::No {
                first_order_id.clone()
            } else {
                String::new()
            },
            yes_filled: if first_leg == OutcomeLeg::Yes {
                filled
            } else {
                dec!(0)
            },
            no_filled: if first_leg == OutcomeLeg::No {
                filled
            } else {
                dec!(0)
            },
            yes_size: order_size,
            no_size: order_size,
            yes_price: yes_price_with_slippage,
            no_price: no_price_with_slippage,
            success: false,
            write_uncertain,
        };

        // Follow only a frozen, confirmed first-leg fill. Two-decimal sizing
        // leaves, at most, dust for recovery rather than over-hedging.
        let hedge_size =
            match decide_after_first_leg(first_filled, first_frozen, order_size, second_price) {
                FirstLegDecision::PauseUncertain => {
                    return Ok(first_only_result(first_filled, true));
                }
                FirstLegDecision::StopUnfilled => {
                    info!(?first_leg, "First leg had no fill; opposite leg suppressed");
                    return Err(anyhow::anyhow!(
                        "first {:?} leg unfilled; opposite leg was not submitted",
                        first_leg
                    ));
                }
                FirstLegDecision::RecoverDust => {
                    warn!(
                        ?first_leg,
                        %first_filled,
                        "Confirmed fill is below the opposite leg minimum; recovery required"
                    );
                    return Ok(first_only_result(first_filled, false));
                }
                FirstLegDecision::SubmitOpposite { size } => size,
            };

        let signed_second = match self
            .build_and_sign_buy(second_token, second_price, hedge_size, expiration)
            .await
        {
            Ok(order) => order,
            Err(error) => {
                error!(?first_leg, %error, "Could not build/sign opposite leg after first-leg fill");
                return Ok(first_only_result(first_filled, false));
            }
        };
        let second_result = match self.client.post_order(signed_second).await {
            Ok(response) => response,
            Err(error) => {
                let tokens = HashSet::from([yes_token_id, no_token_id]);
                if let Err(cancel_error) = self.cancel_orders_for_tokens(&tokens).await {
                    warn!(%cancel_error, "Uncertain opposite write; token-scoped cancellation failed");
                }
                error!(?first_leg, %error, "Opposite leg transport failed after first-leg fill");
                return Ok(first_only_result(first_filled, true));
            }
        };
        let (second_filled, second_frozen) = self
            .freeze_and_reconcile_fill(
                if first_leg == OutcomeLeg::Yes {
                    OutcomeLeg::No
                } else {
                    OutcomeLeg::Yes
                },
                &second_result,
                hedge_size,
            )
            .await;

        let (yes_result, no_result, yes_filled, no_filled, yes_size, no_size) = match first_leg {
            OutcomeLeg::Yes => (
                first_result,
                second_result,
                first_filled,
                second_filled,
                order_size,
                hedge_size,
            ),
            OutcomeLeg::No => (
                second_result,
                first_result,
                second_filled,
                first_filled,
                hedge_size,
                order_size,
            ),
        };

        let send_elapsed = send_start.elapsed().as_millis();
        let total_elapsed = total_start.elapsed().as_millis();
        info!(
            "⏱️ Latency | {} | build+sign {}ms send {}ms total {}ms",
            &pair_id[..8],
            build_sign_elapsed,
            send_elapsed,
            total_elapsed
        );

        if yes_filled == dec!(0) && no_filled == dec!(0) {
            let yes_error_msg = yes_result.error_msg.as_deref().unwrap_or("unknown error");
            let no_error_msg = no_result.error_msg.as_deref().unwrap_or("unknown error");

            let yes_error_simple = if yes_error_msg.contains("no orders found to match") {
                "No matching orders in orderbook"
            } else if yes_error_msg.contains("GTD")
                || yes_error_msg.contains("FOK")
                || yes_error_msg.contains("FAK")
                || yes_error_msg.contains("GTC")
            {
                "Order cannot fill"
            } else {
                yes_error_msg
            };

            let no_error_simple = if no_error_msg.contains("no orders found to match") {
                "No matching orders in orderbook"
            } else if no_error_msg.contains("GTD")
                || no_error_msg.contains("FOK")
                || no_error_msg.contains("FAK")
                || no_error_msg.contains("GTC")
            {
                "Order cannot fill"
            } else {
                no_error_msg
            };

            error!(
                "❌ Arbitrage failed | pair_id:{} | YES:{} | NO:{}",
                &pair_id[..8],
                yes_error_simple,
                no_error_simple
            );

            debug!(
                pair_id = %pair_id,
                yes_order_id = ?yes_result.order_id,
                no_order_id = ?no_result.order_id,
                yes_success = yes_result.success,
                no_success = no_result.success,
                yes_error = %yes_error_msg,
                no_error = %no_error_msg,
                "Both orders unfilled (details)"
            );

            return Err(anyhow::anyhow!(
                "Arbitrage failed: YES and NO orders both unfilled | YES: {}, NO: {}",
                yes_error_simple,
                no_error_simple
            ));
        }

        if !yes_result.success || !no_result.success {
            let yes_error_msg = yes_result.error_msg.as_deref().unwrap_or("unknown error");
            let no_error_msg = no_result.error_msg.as_deref().unwrap_or("unknown error");

            let yes_error_simple = if is_unfilled_order_message(yes_error_msg) {
                "Partially unfilled (order posted)"
            } else {
                "Status abnormal"
            };

            let no_error_simple = if is_unfilled_order_message(no_error_msg) {
                "Partially unfilled (order posted)"
            } else {
                "Status abnormal"
            };

            warn!(
                "⚠️ Partial order status | pair_id:{} | YES:{} (filled:{}) | NO:{} (filled:{}) | risk mgmt triggered",
                &pair_id[..8],
                yes_error_simple,
                yes_filled,
                no_error_simple,
                no_filled
            );

            debug!(
                pair_id = %pair_id,
                yes_order_id = ?yes_result.order_id,
                no_order_id = ?no_result.order_id,
                yes_success = yes_result.success,
                no_success = no_result.success,
                yes_error = %yes_error_msg,
                no_error = %no_error_msg,
                "Order submit status details"
            );
        }

        if yes_filled > dec!(0) && no_filled > dec!(0) {
            info!(
                "✅ Arbitrage success | pair_id:{} | YES filled:{} | NO filled:{} | total:{}",
                &pair_id[..8],
                yes_filled,
                no_filled,
                yes_filled.min(no_filled)
            );
        } else if yes_filled > dec!(0) || no_filled > dec!(0) {
            let side = if yes_filled > dec!(0) { "YES" } else { "NO" };
            let filled = if yes_filled > dec!(0) {
                yes_filled
            } else {
                no_filled
            };
            let other_side = if yes_filled > dec!(0) { "NO" } else { "YES" };
            warn!(
                "⚠️ One-sided fill | {} | {} filled {}, {} unfilled (handed to risk)",
                &pair_id[..8],
                side,
                filled,
                other_side
            );
        } else {
            warn!(
                "❌ Arbitrage failed | pair_id:{} | YES and NO both unfilled",
                &pair_id[..8]
            );
        }

        Ok(OrderPairResult {
            pair_id,
            yes_order_id: yes_result.order_id.clone(),
            no_order_id: no_result.order_id.clone(),
            yes_filled,
            no_filled,
            yes_size,
            no_size,
            yes_price: yes_price_with_slippage,
            no_price: no_price_with_slippage,
            success: yes_filled >= hedge_size && no_filled >= hedge_size,
            write_uncertain: !second_frozen,
        })
    }

    fn log_send_error(
        context: SendErrorContext<'_>,
        error: impl std::fmt::Display,
    ) -> anyhow::Error {
        let send_elapsed = context.send_start.elapsed().as_millis();
        let total_elapsed = context.total_start.elapsed().as_millis();
        error!(
            "❌ V2 order API failed | pair_id:{} | YES:{} NO:{} size:{} | build+sign {}ms send {}ms total {}ms | err:{}",
            &context.pair_id[..8],
            context.yes_price,
            context.no_price,
            context.order_size,
            context.build_sign_elapsed,
            send_elapsed,
            total_elapsed,
            error
        );
        UncertainWriteError {
            message: error.to_string(),
        }
        .into()
    }
}

fn is_unfilled_order_message(message: &str) -> bool {
    message.contains("no orders found to match")
        || ["GTD", "FOK", "FAK", "GTC"]
            .iter()
            .any(|order_type| message.contains(order_type))
}

#[cfg(test)]
mod tests {
    use super::*;
    use polymarket_client_sdk::types::B256;

    fn opportunity() -> ArbitrageOpportunity {
        ArbitrageOpportunity {
            market_id: B256::ZERO,
            yes_token_id: polymarket_client_sdk::types::U256::from(1),
            no_token_id: polymarket_client_sdk::types::U256::from(2),
            yes_ask_price: dec!(0.45),
            no_ask_price: dec!(0.50),
            total_cost: dec!(0.95),
            profit_percentage: dec!(5),
            yes_size: dec!(100),
            no_size: dec!(100),
        }
    }

    #[test]
    fn rejects_quote_when_slippage_consumes_edge() {
        let opportunity = opportunity();
        let result = calculate_execution_quote(QuoteRequest {
            opportunity: &opportunity,
            yes_slippage: dec!(0.02),
            no_slippage: dec!(0.02),
            max_order_notional: dec!(100),
            min_profit_threshold: dec!(0.01),
            execution_spread: dec!(0.03),
            fee_buffer: dec!(0.002),
            yes_tick: dec!(0.01),
            no_tick: dec!(0.01),
        });
        assert!(result.is_err());
    }

    #[test]
    fn enforces_per_leg_notional_limit() {
        let opportunity = opportunity();
        let quote = calculate_execution_quote(QuoteRequest {
            opportunity: &opportunity,
            yes_slippage: dec!(0),
            no_slippage: dec!(0),
            max_order_notional: dec!(10),
            min_profit_threshold: dec!(0.001),
            execution_spread: dec!(0.01),
            fee_buffer: dec!(0.002),
            yes_tick: dec!(0.01),
            no_tick: dec!(0.01),
        })
        .unwrap();
        assert!(quote.yes_price * quote.size <= dec!(10));
        assert!(quote.no_price * quote.size <= dec!(10));
        assert_eq!(quote.size, dec!(20));
    }

    #[test]
    fn buy_prices_round_up_to_market_tick() {
        assert_eq!(
            round_buy_price_to_tick(dec!(0.451), dec!(0.01)).unwrap(),
            dec!(0.46)
        );
        assert_eq!(
            round_buy_price_to_tick(dec!(0.451), dec!(0.001)).unwrap(),
            dec!(0.451)
        );
    }

    #[test]
    fn maps_clob_amounts_to_outcome_shares_by_side() {
        assert_eq!(
            outcome_shares_filled(&Side::Buy, dec!(4.5), dec!(10)),
            dec!(10)
        );
        assert_eq!(
            outcome_shares_filled(&Side::Sell, dec!(10), dec!(4.5)),
            dec!(10)
        );
    }

    #[test]
    fn submits_the_thinner_leg_first() {
        let mut opportunity = opportunity();
        let quote = ExecutionQuote {
            yes_price: dec!(0.70),
            no_price: dec!(0.25),
            size: dec!(10),
            total_cost: dec!(9.5),
            edge_per_share: dec!(0.05),
        };

        opportunity.yes_size = dec!(4);
        opportunity.no_size = dec!(20);
        assert_eq!(harder_leg(&opportunity, &quote), OutcomeLeg::Yes);

        opportunity.yes_size = dec!(20);
        opportunity.no_size = dec!(4);
        assert_eq!(harder_leg(&opportunity, &quote), OutcomeLeg::No);
    }

    #[test]
    fn equal_depth_prefers_the_cheaper_residual_exposure() {
        let opportunity = opportunity();
        let quote = ExecutionQuote {
            yes_price: dec!(0.70),
            no_price: dec!(0.25),
            size: dec!(10),
            total_cost: dec!(9.5),
            edge_per_share: dec!(0.05),
        };

        assert_eq!(harder_leg(&opportunity, &quote), OutcomeLeg::No);
    }

    #[test]
    fn opposite_leg_follows_only_confirmed_first_fill() {
        assert_eq!(opposite_leg_size(dec!(0), dec!(10)), dec!(0));
        assert_eq!(opposite_leg_size(dec!(3.456), dec!(10)), dec!(3.45));
        assert_eq!(opposite_leg_size(dec!(12), dec!(10)), dec!(10));
    }

    #[test]
    fn winner_fee_uses_v2_rate_and_exponent() {
        let policy = WinnerFeePolicy {
            rate: dec!(0.07),
            exponent: 1,
        };
        assert_eq!(policy.fee_per_share(dec!(0.95)), dec!(0.003325));
        assert_eq!(policy.fee_per_share(dec!(0.98)), dec!(0.001372));
        assert_eq!(
            WinnerFeePolicy {
                rate: dec!(0),
                exponent: 0,
            }
            .fee_per_share(dec!(0.95)),
            dec!(0)
        );
    }

    #[test]
    fn first_leg_state_machine_is_fail_closed() {
        assert_eq!(
            decide_after_first_leg(dec!(2), false, dec!(10), dec!(0.5)),
            FirstLegDecision::PauseUncertain
        );
        assert_eq!(
            decide_after_first_leg(dec!(0), true, dec!(10), dec!(0.5)),
            FirstLegDecision::StopUnfilled
        );
        assert_eq!(
            decide_after_first_leg(dec!(1), true, dec!(10), dec!(0.5)),
            FirstLegDecision::RecoverDust
        );
        assert_eq!(
            decide_after_first_leg(dec!(3.456), true, dec!(10), dec!(0.5)),
            FirstLegDecision::SubmitOpposite { size: dec!(3.45) }
        );
    }
}
